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

pub mod image;
pub mod keys;
pub mod mcp;
pub mod qmp;

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_devkit::Workspace;
use ilex_devkit::qemu::{self, VmInfo};
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
}

impl WaitCond {
    pub fn nonblank() -> Self {
        Self { nonblank: true, ..Self::default() }
    }

    /// With nothing asked for, wait for a non-blank screen.
    fn normalized(&self) -> Self {
        if !self.nonblank && !self.changed && self.stable.is_none() {
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
}

impl Vmctl {
    /// Attach to the checkout's VM named `purpose`, or its only running VM.
    pub fn attach(ws: &Workspace, purpose: Option<&str>) -> Result<Vmctl> {
        Ok(Vmctl { info: qemu::find_vm(ws, purpose)? })
    }

    pub fn from_info(info: VmInfo) -> Vmctl {
        Vmctl { info }
    }

    fn qmp(&self) -> Result<Qmp> {
        Qmp::connect(&self.info.qmp_sock)
    }

    // ----- screen -----

    /// The framebuffer as PNG bytes, exactly as QEMU wrote it.
    pub fn screenshot_png(&self) -> Result<Vec<u8>> {
        let file = self.info.dir.join(format!("screendump-{}.png", std::process::id()));
        let result = (|| {
            self.qmp()?.execute("screendump", json!({ "filename": file, "format": "png" }))?;
            std::fs::read(&file).with_context(|| format!("reading {}", file.display()))
        })();
        let _ = std::fs::remove_file(&file);
        result
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
                && (!cond.changed || Some(fp) != first)
                && cond.stable.is_none_or(|s| since.elapsed() >= s);
            if ok {
                return Ok((png, frame));
            }
            if started.elapsed() > timeout {
                bail!(
                    "screen did not reach {cond:?} within {timeout:?} (last frame {}x{}, {:.1}% one colour)",
                    frame.width,
                    frame.height,
                    frame.dominant_fraction() * 100.0
                );
            }
            thread::sleep(Duration::from_millis(1000));
        }
    }

    // ----- pointer -----

    /// Absolute pointer move to screen pixel (x, y).
    pub fn move_to(&self, x: u32, y: u32) -> Result<()> {
        let (w, h) = self.screen_size()?;
        let mut qmp = self.qmp()?;
        send_events(&mut qmp, abs_events(x, y, w, h))
    }

    /// Move to (x, y) and click `button` `count` times (2 = double click).
    pub fn click(&self, x: u32, y: u32, button: Button, count: u32) -> Result<()> {
        let (w, h) = self.screen_size()?;
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
        let (w, h) = self.screen_size()?;
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
        let strokes = text.chars().map(keys::stroke_for_char).collect::<Result<Vec<_>>>()?;
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

fn key_event(qcode: &str, down: bool) -> Value {
    json!({ "type": "key", "data": { "down": down, "key": { "type": "qcode", "data": qcode } } })
}

fn btn_event(button: &str, down: bool) -> Value {
    json!({ "type": "btn", "data": { "down": down, "button": button } })
}

/// QEMU's absolute axes run 0..=0x7fff across the screen.
fn abs_events(x: u32, y: u32, width: u32, height: u32) -> Vec<Value> {
    let scale = |v: u32, size: u32| -> u32 {
        let max = size.saturating_sub(1).max(1) as u64;
        ((v.min(size.saturating_sub(1)) as u64 * 0x7fff + max / 2) / max) as u32
    };
    vec![
        json!({ "type": "abs", "data": { "axis": "x", "value": scale(x, width) } }),
        json!({ "type": "abs", "data": { "axis": "y", "value": scale(y, height) } }),
    ]
}
