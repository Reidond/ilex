//! `ilex-vmctl`: drive an ilex VM that `cargo xtask vm` or the e2e harness
//! booted. It never starts or stops VMs (xtask owns that, under the machine
//! wide VM slot cap); it attaches by worktree slug to the QMP socket under
//! `$XDG_RUNTIME_DIR/ilex-vm/<slug>/<purpose>/` and offers:
//!
//! - [`Vmctl::screenshot_png`]: the guest framebuffer, via QMP `screendump`
//! - [`Vmctl::click`], [`Vmctl::type_text`], [`Vmctl::key`], ...: input at the
//!   virtual hardware (`input-send-event`), so it works before anyone logs in
//! - [`Vmctl::wait_screen`]: block until the picture is not blank / settled
//!
//! The CLI (`main.rs`) and the MCP server ([`mcp`]) are thin wrappers; the
//! e2e `graphical::` tier uses the same library.

pub mod backend;
pub mod image;
pub mod keys;
pub mod mcp;
pub mod qmp;

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use backend::{self as qemu, VmInfo};
use ilex_devkit::Workspace;
use serde_json::{Value, json};

pub use image::Frame;
use qmp::Qmp;

/// Mouse buttons `click` can press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    fn qmp(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Middle => "middle",
            Button::Right => "right",
        }
    }

    pub fn parse(s: &str) -> Result<Button> {
        match s {
            "left" => Ok(Button::Left),
            "middle" => Ok(Button::Middle),
            "right" => Ok(Button::Right),
            other => bail!("unknown mouse button `{other}` (left, middle, right)"),
        }
    }
}

/// What `wait_screen` waits for; every set condition must hold at once.
#[derive(Debug, Clone, Default)]
pub struct WaitCond {
    /// The picture is not blank.
    pub nonblank: bool,
    /// The picture differs from the first one seen.
    pub changed: bool,
    /// The picture has not changed for this long.
    pub stable: Option<Duration>,
    /// The framebuffer has this size. Firmware, GRUB and Plymouth draw at
    /// their own mode, so the first non-blank frame of a boot is often not
    /// the guest's final resolution: wait for the size instead of assuming it.
    pub size: Option<(u32, u32)>,
}

/// Parse `3840x2160`.
pub fn parse_size(s: &str) -> Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .with_context(|| format!("size `{s}` is not WIDTHxHEIGHT"))?;
    Ok((
        w.trim().parse().context("width")?,
        h.trim().parse().context("height")?,
    ))
}

impl WaitCond {
    pub fn nonblank() -> Self {
        Self {
            nonblank: true,
            ..Self::default()
        }
    }

    /// With nothing asked for, wait for a non-blank screen.
    fn normalized(&self) -> Self {
        if !self.nonblank && !self.changed && self.stable.is_none() && self.size.is_none() {
            Self::nonblank()
        } else {
            self.clone()
        }
    }
}

/// A handle on one running VM.
#[derive(Debug, Clone)]
pub struct Vmctl {
    pub info: VmInfo,
    mapping: std::sync::Arc<std::sync::Mutex<FrameMapping>>,
}

#[derive(Debug, Default)]
struct FrameMapping {
    size: (u32, u32),
    generation: u64,
    captured_unix_ms: Option<u64>,
    #[cfg(target_os = "windows")]
    source_ticks: Option<i64>,
    #[cfg(target_os = "windows")]
    screenshot_source: Option<String>,
}

impl Vmctl {
    /// Attach to the checkout's VM named `purpose`, or its only running VM.
    pub fn attach(ws: &Workspace, purpose: Option<&str>) -> Result<Vmctl> {
        Ok(Self::from_info(qemu::find_vm(ws, purpose)?))
    }

    pub fn from_info(info: VmInfo) -> Vmctl {
        Vmctl {
            info,
            mapping: Default::default(),
        }
    }

    pub fn capabilities(&self) -> ilex_tooling_core::VmCapabilities {
        #[cfg(target_os = "windows")]
        let mut caps = self.info.capabilities();
        #[cfg(not(target_os = "windows"))]
        let mut caps = ilex_devkit::provider::capabilities(
            if self.info.display == "viewer" && self.info.gpu {
                "viewer-gl"
            } else {
                &self.info.display
            },
            (self.info.width, self.info.height),
        );
        caps.vm_id = Some(format!("{}:{}", self.info.pid, self.info.process_start));
        #[cfg(target_os = "linux")]
        {
            use ilex_tooling_core::{Accelerator, Capability, CapabilityStatus};

            // Provisioning is separate from profile evidence and current QMP state.
            if self.info.accelerator == Accelerator::Tcg {
                caps.provider = "linux-qemu-tcg".into();
                caps.features.insert(Capability::Start, CapabilityStatus::Available);
            }
            if self.info.secure_boot {
                caps.provisioning.insert(Capability::FirmwareEnforcement, CapabilityStatus::Available);
            }
            if self.info.persistent_tpm {
                caps.provisioning.insert(Capability::PersistentTpm, CapabilityStatus::Available);
            }
            ilex_devkit::qualification::runtime(&self.info, &mut caps);
        }
        caps.screenshot_dimensions = self
            .mapping
            .lock()
            .ok()
            .and_then(|mapping| (mapping.size.0 > 0).then_some(mapping.size));
        #[cfg(target_os = "windows")]
        if let Ok(mapping) = self.mapping.lock()
            && let Some(source) = &mapping.screenshot_source
        {
            caps.screenshot_source = Some(source.clone());
        }
        caps
    }

    pub fn handle(&self) -> ilex_tooling_core::VmHandle {
        #[cfg(target_os = "windows")]
        let provider = "windows-qemu-whpx";
        #[cfg(not(target_os = "windows"))]
        let provider = if self.info.accelerator == ilex_tooling_core::Accelerator::Tcg {
            "linux-qemu-tcg"
        } else {
            "linux-qemu-kvm"
        };
        ilex_tooling_core::VmHandle {
            schema: self.info.schema,
            owner: self.info.owner.clone(),
            run: self.info.run.clone(),
            provider: provider.into(),
            opaque_id: format!("{}:{}", self.info.pid, self.info.process_start),
            guest_arch: self.info.architecture,
        }
    }

    fn qmp(&self) -> Result<Qmp> {
        ilex_tooling_core::check_version(self.info.schema)?;
        anyhow::ensure!(
            self.info.is_running(),
            "VM process identity no longer matches; attach again"
        );
        #[cfg(target_os = "windows")]
        {
            self.info.validate()?;
            Qmp::connect_serialized(
                &self.info.qmp_endpoint,
                &self.info.dir.join("qmp-client.lock"),
            )
        }
        #[cfg(not(target_os = "windows"))]
        Qmp::connect(&self.info.qmp_sock)
    }

    /// Read-only live QMP diagnostic, using the same serialized transport as input.
    pub fn status(&self) -> Result<Value> {
        self.qmp()?.execute("query-status", json!({}))
    }
    pub fn view_generation(&self) -> u64 {
        self.mapping.lock().map_or(0, |mapping| mapping.generation)
    }

    // ----- screen -----

    /// The framebuffer as PNG bytes: as QEMU wrote it, or, for a VM behind
    /// the display bridge (`xtask vm --gui` in WSL), from the bridge.
    pub fn screenshot_png(&self) -> Result<Vec<u8>> {
        ilex_tooling_core::check_version(self.info.schema)?;
        self.capabilities()
            .require(ilex_tooling_core::Capability::Screenshot)?;
        #[cfg(target_os = "windows")]
        if self.info.hwnd != 0 {
            self.info.validate()?;
            let captured = ilex_tooling_windows::capture::capture(&self.info)?;
            let mut mapping = self
                .mapping
                .lock()
                .map_err(|_| anyhow::anyhow!("frame mapping poisoned"))?;
            anyhow::ensure!(
                mapping
                    .source_ticks
                    .is_none_or(|ticks| captured.source_ticks > ticks),
                "capture did not produce a new compositor frame"
            );
            mapping.source_ticks = Some(captured.source_ticks);
            mapping.screenshot_source = Some(captured.source.into());
            mapping.generation = captured.generation;
            mapping.captured_unix_ms = Some(captured.timestamp_ms);
            if mapping.size != (captured.width, captured.height) {
                mapping.size = (captured.width, captured.height);
            }
            return Frame {
                width: captured.width,
                height: captured.height,
                rgb: captured.rgb,
            }
            .encode_png();
        }
        if self.info.display == "viewer" {
            return self.bridge_screenshot()?.encode_png();
        }
        if self.info.display == "gtk-gl" {
            bail!(
                "{} renders with virgl in a GTK window, and QEMU can't take screenshots of GL output \
                 (QMP screendump: \"no surface\"); input still works. Use `cargo xtask vm --disk`, or \
                 `--gui --no-gl`, for screenshots",
                self.info.name
            );
        }
        let file = self
            .info
            .dir
            .join(format!("screendump-{}.png", std::process::id()));
        let result = (|| {
            self.qmp()?
                .execute("screendump", json!({ "filename": file, "format": "png" }))?;
            std::fs::read(&file).with_context(|| format!("reading {}", file.display()))
        })();
        let _ = std::fs::remove_file(&file);
        result
    }

    /// The display bridge's current frame (`screen.sock`: `u32` width, `u32`
    /// height, BGRX pixels).
    #[cfg(target_os = "linux")]
    fn bridge_screenshot(&self) -> Result<Frame> {
        use std::io::Read;
        let path = self.info.dir.join("screen.sock");
        let mut stream = std::os::unix::net::UnixStream::connect(&path).with_context(|| {
            format!(
                "connecting to {} (is the display bridge running?)",
                path.display()
            )
        })?;
        let mut head = [0u8; 8];
        stream.read_exact(&mut head)?;
        let width = u32::from_le_bytes(head[..4].try_into().unwrap());
        let height = u32::from_le_bytes(head[4..].try_into().unwrap());
        let mut bgrx = vec![0u8; width as usize * height as usize * 4];
        stream.read_exact(&mut bgrx).context("reading the frame")?;
        let rgb = bgrx
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|p| [p[2], p[1], p[0]])
            .collect();
        Ok(Frame { width, height, rgb })
    }

    #[cfg(not(target_os = "linux"))]
    fn bridge_screenshot(&self) -> Result<Frame> {
        bail!("the display bridge's socket is a unix socket: run ilex-vmctl inside WSL")
    }

    /// The framebuffer, decoded.
    pub fn screenshot(&self) -> Result<Frame> {
        Frame::decode(&self.screenshot_png()?)
    }

    /// Save a screenshot as a PNG at `path` and return it decoded.
    pub fn save_screenshot(&self, path: &Path) -> Result<Frame> {
        let png = self.screenshot_png()?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, &png).with_context(|| format!("writing {}", path.display()))?;
        #[cfg(target_os = "windows")]
        {
            let source = self.capabilities().screenshot_source;
            let mapping = self
                .mapping
                .lock()
                .map_err(|_| anyhow::anyhow!("frame mapping poisoned"))?;
            std::fs::write(
                path.with_extension("frame.json"),
                serde_json::to_vec_pretty(
                    &json!({"schema":1,"vm":self.handle(),"captured_unix_ms":mapping.captured_unix_ms,"source_ticks":mapping.source_ticks,"generation":mapping.generation,"client_size":mapping.size,"source":source}),
                )?,
            )?;
        }
        Frame::decode(&png)
    }

    /// Current framebuffer size in pixels.
    pub fn screen_size(&self) -> Result<(u32, u32)> {
        let f = self.screenshot()?;
        Ok((f.width, f.height))
    }

    /// Poll screenshots until `cond` holds or `timeout` passes. Returns the
    /// frame that satisfied it (PNG bytes and decoded).
    pub fn wait_screen(&self, cond: &WaitCond, timeout: Duration) -> Result<(Vec<u8>, Frame)> {
        let cond = cond.normalized();
        let started = Instant::now();
        let mut first: Option<u64> = None;
        let mut last: Option<(u64, Instant)> = None;
        loop {
            let png = self.screenshot_png()?;
            let frame = Frame::decode(&png)?;
            let fp = frame.fingerprint();
            first.get_or_insert(fp);
            let since = match last {
                Some((prev, since)) if prev == fp => since,
                _ => Instant::now(),
            };
            last = Some((fp, since));

            let ok = (!cond.nonblank || !frame.is_blank())
                && cond.size.is_none_or(|s| s == (frame.width, frame.height))
                && (!cond.changed || Some(fp) != first)
                && cond.stable.is_none_or(|s| since.elapsed() >= s);
            if ok {
                return Ok((png, frame));
            }
            if started.elapsed() > timeout {
                bail!(
                    "screen did not reach {cond:?} within {timeout:?} (last frame {}x{}, {:.1}% one colour; \
                     a size that never changes means the guest never switched modes)",
                    frame.width,
                    frame.height,
                    frame.dominant_fraction() * 100.0
                );
            }
            thread::sleep(Duration::from_millis(1000));
        }
    }

    // ----- pointer -----

    /// SDL can letterbox while the guest is still changing modes. Its human
    /// input path knows that viewport; QMP input must wait for pixel parity.
    fn input_size(&self) -> Result<(u32, u32)> {
        let size = self.screen_size()?;
        #[cfg(target_os = "windows")]
        if self.info.hwnd != 0 {
            let actual = ilex_tooling_windows::session::display_info(&self.info, None).context(
                "guest mode unavailable; use keyboard input until the display session is ready",
            )?;
            let wanted = json!([size.0, size.1]);
            let matched = actual["monitors"].as_array().is_some_and(|monitors| {
                monitors.iter().any(|m| {
                    m["current"]
                        .as_array()
                        .is_some_and(|modes| modes.contains(&wanted))
                })
            });
            anyhow::ensure!(
                matched,
                "guest mode has not acknowledged {}x{} client pixels; capture/wait after resize before pointer input",
                size.0,
                size.1
            );
        }
        Ok(size)
    }

    /// Absolute pointer move to screen pixel (x, y).
    pub fn move_to(&self, x: u32, y: u32) -> Result<()> {
        let (w, h) = self.input_size()?;
        let mut qmp = self.qmp()?;
        send_events(&mut qmp, abs_events(x, y, w, h))
    }

    /// Move to (x, y) and click `button` `count` times (2 = double click).
    pub fn click(&self, x: u32, y: u32, button: Button, count: u32) -> Result<()> {
        let (w, h) = self.input_size()?;
        let mut qmp = self.qmp()?;
        send_events(&mut qmp, abs_events(x, y, w, h))?;
        // Let the compositor see the pointer arrive before the button.
        thread::sleep(Duration::from_millis(120));
        for i in 0..count.max(1) {
            if i > 0 {
                thread::sleep(Duration::from_millis(80));
            }
            send_events(&mut qmp, vec![btn_event(button.qmp(), true)])?;
            thread::sleep(Duration::from_millis(60));
            send_events(&mut qmp, vec![btn_event(button.qmp(), false)])?;
        }
        Ok(())
    }

    /// Scroll the wheel at (x, y): `amount` notches `up` or down.
    pub fn scroll(&self, x: u32, y: u32, up: bool, amount: u32) -> Result<()> {
        let (w, h) = self.input_size()?;
        let mut qmp = self.qmp()?;
        send_events(&mut qmp, abs_events(x, y, w, h))?;
        thread::sleep(Duration::from_millis(120));
        let wheel = if up { "wheel-up" } else { "wheel-down" };
        for _ in 0..amount.max(1) {
            send_events(&mut qmp, vec![btn_event(wheel, true)])?;
            send_events(&mut qmp, vec![btn_event(wheel, false)])?;
            thread::sleep(Duration::from_millis(40));
        }
        Ok(())
    }

    // ----- keyboard -----

    /// Type `text` on a US keyboard. `\n` presses Return.
    pub fn type_text(&self, text: &str) -> Result<()> {
        anyhow::ensure!(text.len() <= 64 * 1024, "text input exceeds 64 KiB");
        let strokes = text
            .chars()
            .map(keys::stroke_for_char)
            .collect::<Result<Vec<_>>>()?;
        let mut qmp = self.qmp()?;
        for s in strokes {
            let mut down = Vec::new();
            if s.shift {
                down.push(key_event("shift", true));
            }
            down.push(key_event(s.qcode, true));
            send_events(&mut qmp, down)?;
            thread::sleep(Duration::from_millis(10));
            let mut up = vec![key_event(s.qcode, false)];
            if s.shift {
                up.push(key_event("shift", false));
            }
            send_events(&mut qmp, up)?;
            thread::sleep(Duration::from_millis(15));
        }
        Ok(())
    }

    /// Press a key combination such as `ctrl+alt+t` or `Return`.
    pub fn key(&self, combo: &str) -> Result<()> {
        let keys = keys::parse_combo(combo)?;
        let mut qmp = self.qmp()?;
        for k in &keys {
            send_events(&mut qmp, vec![key_event(k, true)])?;
            thread::sleep(Duration::from_millis(10));
        }
        thread::sleep(Duration::from_millis(30));
        for k in keys.iter().rev() {
            send_events(&mut qmp, vec![key_event(k, false)])?;
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }
}

fn send_events(qmp: &mut Qmp, events: Vec<Value>) -> Result<()> {
    qmp.execute("input-send-event", json!({ "events": events }))?;
    Ok(())
}

pub use ilex_tooling_qmp::{absolute_events as abs_events, button_event as btn_event, key_event};

impl ilex_tooling_core::VmControl for Vmctl {
    fn capabilities(&self) -> Result<ilex_tooling_core::VmCapabilities> {
        Ok(self.capabilities())
    }
    fn capture(&self) -> Result<ilex_tooling_core::Frame> {
        use ilex_tooling_core::{CoordinateSpace, Viewport};
        let image = self.screenshot()?;
        let mut mapping = self
            .mapping
            .lock()
            .map_err(|_| anyhow::anyhow!("frame mapping poisoned"))?;
        if mapping.size != (image.width, image.height) {
            mapping.size = (image.width, image.height);
            mapping.generation = mapping
                .generation
                .checked_add(1)
                .context("frame generation exhausted")?;
        }
        Ok(ilex_tooling_core::Frame {
            vm: self.handle(),
            captured_unix_ms: mapping.captured_unix_ms.unwrap_or(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_millis()
                    .try_into()?,
            ),
            generation: mapping.generation,
            width: image.width,
            height: image.height,
            viewport: Viewport {
                x: 0,
                y: 0,
                width: image.width,
                height: image.height,
            },
            coordinate_space: if cfg!(target_os = "windows") {
                CoordinateSpace::ClientPixels
            } else {
                CoordinateSpace::FramebufferPixels
            },
            pixels: image.rgb,
        })
    }
    fn key(&self, input: &ilex_tooling_core::Input) -> Result<()> {
        use ilex_tooling_core::Input;
        use ilex_tooling_qmp::physical_key_event;
        input.validate()?;
        match input {
            Input::Text { text, layout, .. } => {
                anyhow::ensure!(
                    layout == "us",
                    "Linux QMP text input requires layout us; Unicode is unsupported"
                );
                // Validate before sending anything; errors never disclose text.
                anyhow::ensure!(
                    text.chars().all(|c| keys::stroke_for_char(c).is_ok()),
                    "text is not representable on the declared US keyboard"
                );
                self.type_text(text)
            }
            Input::PhysicalKey { code, down } => {
                send_events(&mut self.qmp()?, vec![physical_key_event(*code, *down)])
            }
            Input::Chord { codes } => {
                let mut qmp = self.qmp()?;
                let down = send_events(
                    &mut qmp,
                    codes.iter().map(|&c| physical_key_event(c, true)).collect(),
                );
                thread::sleep(Duration::from_millis(30));
                let up = send_events(
                    &mut qmp,
                    codes
                        .iter()
                        .rev()
                        .map(|&c| physical_key_event(c, false))
                        .collect(),
                );
                down.and(up)
            }
        }
    }
    fn pointer(
        &self,
        frame: &ilex_tooling_core::Frame,
        generation: u64,
        x: u32,
        y: u32,
    ) -> Result<()> {
        let current = ilex_tooling_core::VmControl::capture(self)?;
        let input_size = self.input_size()?;
        anyhow::ensure!(
            input_size == (current.width, current.height),
            "display changed while validating pointer coordinates; capture again"
        );
        frame.validate_pointer(&current.vm, generation, x, y)?;
        anyhow::ensure!(
            current.generation == generation
                && (current.width, current.height) == (frame.width, frame.height),
            "framebuffer resized; capture again before pointer input"
        );
        anyhow::ensure!(
            current.coordinate_space == frame.coordinate_space
                && current.viewport == frame.viewport,
            "frame coordinate space or viewport changed; capture again"
        );
        send_events(
            &mut self.qmp()?,
            abs_events(x, y, current.width, current.height),
        )
    }
    fn serial(&self) -> Result<Vec<u8>> {
        Ok(std::fs::read(self.info.dir.join("serial.log"))?)
    }
    fn guest_exec(&self, argv: &[String]) -> Result<Vec<u8>> {
        anyhow::ensure!(
            !argv.is_empty() && argv.iter().all(|a| !a.contains('\0')),
            "invalid guest argv"
        );
        let line = argv
            .iter()
            .map(|a| format!("'{}'", a.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ");
        #[cfg(target_os = "windows")]
        let out = self.info.ssh().arg(line).output()?;
        #[cfg(not(target_os = "windows"))]
        let out = std::process::Command::new("ssh")
            .args(SSH_OPTIONS)
            .args(["-p", &self.info.ssh_port.to_string(), "-i"])
            .arg(&self.info.ssh_key)
            .arg("root@127.0.0.1")
            .arg(line)
            .output()?;
        anyhow::ensure!(
            out.status.success(),
            "guest command failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(out.stdout)
    }
    fn upload(&self, local: &Path, guest: &str) -> Result<()> {
        transfer(self, local, guest, false)
    }
    fn download(&self, guest: &str, local: &Path) -> Result<()> {
        transfer(self, local, guest, true)
    }
}

#[cfg(not(target_os = "windows"))]
const SSH_OPTIONS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
    "-o",
    "IdentitiesOnly=yes",
];

fn transfer(vm: &Vmctl, local: &Path, guest: &str, download: bool) -> Result<()> {
    anyhow::ensure!(
        guest.starts_with('/')
            && guest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
        "guest transfer path must be an absolute simple path"
    );
    let remote = format!("root@127.0.0.1:{guest}");
    let local = std::path::absolute(local)?;
    #[cfg(target_os = "windows")]
    let mut command = {
        vm.info.validate()?;
        vm.info.scp()
    };
    #[cfg(not(target_os = "windows"))]
    let mut command = std::process::Command::new("scp");
    #[cfg(not(target_os = "windows"))]
    command
        .args(SSH_OPTIONS)
        .args(["-P", &vm.info.ssh_port.to_string(), "-i"])
        .arg(&vm.info.ssh_key);
    if download {
        command.arg(remote).arg(local);
    } else {
        command.arg(local).arg(remote);
    }
    ilex_devkit::cmd::run(&mut command)
}
