//! Disk images and QEMU: write a disk with `bcvk to-disk`, boot it under KVM
//! the way the plan's "Display device" section describes, and reach it over
//! SSH.
//!
//! One [`Vm`] is one `qemu-system-x86_64` process holding a machine-wide VM
//! slot ([`crate::slots`]) for as long as it lives. Everything a client needs
//! to attach (`ilex-vmctl`, a human with a VNC viewer) is published in
//! `$XDG_RUNTIME_DIR/ilex-vm/<slug>/<purpose>/`:
//!
//! ```text
//! qmp.sock   QMP control socket (screendump, input-send-event)
//! vnc.sock   VNC server on a unix socket, for a human watching
//! vm.toml    TOML description of the running VM (see [`VmInfo`])
//! qemu.log   QEMU's stdout/stderr
//! serial.log guest serial console
//! ```
//!
//! The guest is reached over SSH through a user-mode-networking port forward
//! on `127.0.0.1`. The root login key is a throwaway per-checkout test key,
//! handed to the guest as the systemd credential `ssh.authorized_keys.root`
//! through SMBIOS type 11, so no key is baked into the image.
//!
//! Boots run from a copy-on-write overlay of the base disk by default, so the
//! base stays pristine and several VMs can share it.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::slots::{self, VmSlot};
use crate::{Workspace, cmd};

// ---------------------------------------------------------------------------
// Disk images
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    Qcow2,
    Raw,
}

impl DiskFormat {
    /// The name `bcvk to-disk --format` and `qemu-img -f` use.
    pub fn name(self) -> &'static str {
        match self {
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw => "raw",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw => "img",
        }
    }

    /// Guess from a file name (`.qcow2` → qcow2, anything else → raw).
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some("qcow2") => DiskFormat::Qcow2,
            _ => DiskFormat::Raw,
        }
    }
}

/// What to write with `bcvk to-disk`.
#[derive(Debug, Clone)]
pub struct DiskSpec {
    /// Image reference in local container storage.
    pub image: String,
    pub format: DiskFormat,
    /// Virtual size, e.g. `30G`.
    pub size: String,
    /// Output path; defaults to [`default_disk_path`].
    pub out: Option<PathBuf>,
}

impl DiskSpec {
    pub fn new(image: impl Into<String>) -> Self {
        Self { image: image.into(), format: DiskFormat::Qcow2, size: "30G".into(), out: None }
    }
}

/// `<state dir>/disks/<image, made path-safe>.<ext>`.
pub fn default_disk_path(ws: &Workspace, image: &str, format: DiskFormat) -> PathBuf {
    let safe: String = image
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    ws.state_dir().join("disks").join(format!("{safe}.{}", format.extension()))
}

/// Write (or refresh) the disk for `spec` with `bcvk to-disk` and return its
/// path. bcvk records what a disk was built from and skips the install when
/// the image hasn't changed, so calling this before every boot is cheap.
pub fn ensure_disk(ws: &Workspace, spec: &DiskSpec) -> Result<PathBuf> {
    cmd::require("bcvk")?;
    let out = spec.out.clone().unwrap_or_else(|| default_disk_path(ws, &spec.image, spec.format));
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Tiers run in parallel threads and each wants the disk: one writer at a
    // time, the rest wait and then find it up to date.
    let lock_path = out.with_file_name(format!(
        "{}.lock",
        out.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
    ));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock().with_context(|| format!("locking {}", lock_path.display()))?;

    let mut bcvk = Command::new("bcvk");
    bcvk.args(["to-disk", "--format", spec.format.name()])
        .args(["--disk-size", &spec.size])
        .arg(&spec.image)
        .arg(&out);
    cmd::run(&mut bcvk)?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// SSH test key
// ---------------------------------------------------------------------------

/// A throwaway ed25519 key pair for logging in to test VMs.
#[derive(Debug, Clone)]
pub struct TestKey {
    pub private: PathBuf,
    /// One line, as in `authorized_keys`.
    pub public: String,
}

/// The checkout's test key (`<state dir>/ssh/id_ed25519`), created on first use.
pub fn test_key(ws: &Workspace) -> Result<TestKey> {
    let private = ws.state_dir().join("ssh").join("id_ed25519");
    let public_path = private.with_extension("pub");
    if !(private.is_file() && public_path.is_file()) {
        cmd::require("ssh-keygen")?;
        fs::create_dir_all(private.parent().unwrap())?;
        let _ = fs::remove_file(&private);
        let _ = fs::remove_file(&public_path);
        cmd::run(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "ilex-test"])
                .arg("-f")
                .arg(&private),
        )?;
    }
    let public = fs::read_to_string(&public_path)
        .with_context(|| format!("reading {}", public_path.display()))?
        .trim()
        .to_string();
    Ok(TestKey { private, public })
}

// ---------------------------------------------------------------------------
// What is running
// ---------------------------------------------------------------------------

/// The published description of a running VM: `<runtime dir>/<purpose>/vm.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub purpose: String,
    /// PID of the `qemu-system-x86_64` process.
    pub pid: u32,
    pub ssh_port: u16,
    pub ssh_key: PathBuf,
    pub qmp_sock: PathBuf,
    pub vnc_sock: PathBuf,
    /// Directory holding this VM's sockets and logs (where `vm.toml` lives, so
    /// not stored in it).
    #[serde(skip)]
    pub dir: PathBuf,
    /// Configured virtio-vga mode (what the guest reads from EDID).
    pub width: u32,
    pub height: u32,
    /// `none` (headless), `gtk`, or `gtk-gl` (a GTK window and virgl: QMP
    /// `screendump` can't capture it and there is no VNC server).
    pub display: String,
}

const INFO_FILE: &str = "vm.toml";

impl VmInfo {
    fn write(&self) -> Result<()> {
        let path = self.dir.join(INFO_FILE);
        let text = toml::to_string(self).context("serializing vm.toml")?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// Read `<dir>/vm.toml`.
    pub fn read(dir: &Path) -> Result<Self> {
        let path = dir.join(INFO_FILE);
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut info: VmInfo =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        info.dir = dir.to_path_buf();
        Ok(info)
    }

    /// Is the QEMU process still there?
    pub fn is_running(&self) -> bool {
        process_alive(self.pid)
    }

    /// `ssh` arguments to reach this VM as root, ready for display.
    pub fn ssh_command_line(&self) -> String {
        format!(
            "ssh -p {} -i {} {} root@127.0.0.1",
            self.ssh_port,
            self.ssh_key.display(),
            SSH_OPTS.join(" ")
        )
    }
}

fn process_alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Every VM of this checkout whose QEMU is still running, sorted by purpose.
pub fn running_vms(ws: &Workspace) -> Vec<VmInfo> {
    let Ok(entries) = fs::read_dir(ws.runtime_dir()) else { return Vec::new() };
    let mut vms: Vec<VmInfo> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| VmInfo::read(&e.path()).ok())
        .filter(VmInfo::is_running)
        .collect();
    vms.sort_by(|a, b| a.purpose.cmp(&b.purpose));
    vms
}

/// Find a running VM of this checkout: the named `purpose`, or the only one
/// running when `purpose` is `None`.
pub fn find_vm(ws: &Workspace, purpose: Option<&str>) -> Result<VmInfo> {
    let vms = running_vms(ws);
    match purpose {
        Some(p) => vms.into_iter().find(|v| v.purpose == p).with_context(|| {
            format!("no running `{p}` VM for `{}` (boot one with `cargo xtask vm --gui`)", ws.slug)
        }),
        None => match vms.len() {
            0 => bail!("no VM running for `{}` (boot one with `cargo xtask vm --gui`)", ws.slug),
            1 => Ok(vms.into_iter().next().unwrap()),
            _ => {
                let names: Vec<_> = vms.iter().map(|v| v.purpose.as_str()).collect();
                bail!("several VMs are running for `{}` ({}); pick one with --vm", ws.slug, names.join(", "))
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Booting
// ---------------------------------------------------------------------------

const SSH_OPTS: &[&str] = &[
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR",
    "-o", "IdentitiesOnly=yes",
    "-o", "BatchMode=yes",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    /// No window; VNC and QMP only. What tests use, so every run gets the
    /// same framebuffer size.
    None,
    /// A GTK window for a human. The guest follows the window's size.
    Gtk,
}

/// Can this host render a guest's OpenGL (virgl) on a GPU? WSL2 has Mesa's
/// D3D12 driver on `/dev/dxg`; elsewhere a DRM render node is needed.
pub fn host_gl() -> bool {
    Path::new("/dev/dxg").exists()
        || fs::read_dir("/dev/dri")
            .map(|dir| dir.flatten().any(|e| e.file_name().to_string_lossy().starts_with("renderD")))
            .unwrap_or(false)
}

#[derive(Debug, Clone)]
pub struct VmConfig {
    /// Names the VM (`ilex-<slug>-<purpose>`) and its runtime subdirectory.
    pub purpose: String,
    /// Base disk image.
    pub disk: PathBuf,
    /// Boot from a throwaway overlay of `disk` (default) or write to it.
    pub snapshot: bool,
    pub memory: String,
    pub vcpus: u32,
    /// virtio-vga preferred mode, read by the guest from EDID.
    pub width: u32,
    pub height: u32,
    pub display: Display,
    /// With [`Display::Gtk`]: give the guest a 3D-capable `virtio-vga-gl`
    /// whose OpenGL the host renders (virgl), instead of llvmpipe in the
    /// guest. QEMU then has no VNC server and QMP `screendump` can't capture
    /// the screen. See [`host_gl`].
    pub gl: bool,
    /// An install ISO attached as a CD-ROM. Firmware tries the disk first and
    /// falls through to the CD when the disk is empty.
    pub cdrom: Option<PathBuf>,
    /// Extra raw disks, attached read-write after the main one (an installer
    /// finds its kickstart on one labelled `OEMDRV`).
    pub extra_disks: Vec<PathBuf>,
    /// Keep the UEFI variable store here, copying the firmware's template when
    /// it doesn't exist yet. Boot entries an installer writes survive into
    /// later boots that name the same file; by default every boot starts from
    /// a fresh store in the VM's runtime directory.
    pub efi_vars: Option<PathBuf>,
}

impl VmConfig {
    /// Headless, 4 GiB, 1280x800: enough for the disk tier.
    pub fn new(purpose: impl Into<String>, disk: impl Into<PathBuf>) -> Self {
        Self {
            purpose: purpose.into(),
            disk: disk.into(),
            snapshot: true,
            memory: "4G".into(),
            vcpus: 2,
            width: 1280,
            height: 800,
            display: Display::None,
            gl: false,
            cdrom: None,
            extra_disks: Vec::new(),
            efi_vars: None,
        }
    }

    /// The plan's graphical test machine: 8 GiB, 4 vCPUs, 3840x2160.
    pub fn graphical(purpose: impl Into<String>, disk: impl Into<PathBuf>) -> Self {
        Self { memory: "8G".into(), vcpus: 4, width: 3840, height: 2160, ..Self::new(purpose, disk) }
    }
}

/// A booted VM. Dropping it kills QEMU, removes its sockets and overlay and
/// frees its slot.
#[derive(Debug)]
pub struct Vm {
    pub info: VmInfo,
    child: Child,
    overlay: Option<PathBuf>,
    // Last field: freed only after QEMU is gone.
    _slot: VmSlot,
}

impl Vm {
    /// Take a VM slot (waiting if none is free) and start QEMU. Returns as
    /// soon as QEMU is up; use [`Vm::wait_ssh`] to wait for the guest.
    pub fn boot(ws: &Workspace, config: &VmConfig) -> Result<Vm> {
        cmd::require("qemu-system-x86_64")?;
        if !Path::new("/dev/kvm").exists() {
            bail!("/dev/kvm not found: booting VMs needs KVM (on Windows, run inside WSL2 with nested virtualization)");
        }
        let name = ws.vm_name(&config.purpose);
        let slot = slots::acquire(&name)?;

        let dir = ws.runtime_dir().join(&config.purpose);
        if let Ok(old) = VmInfo::read(&dir)
            && old.is_running()
        {
            bail!("{name} is already running (pid {}); stop it first (kill {})", old.pid, old.pid);
        }
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let key = test_key(ws)?;
        let (ovmf_code, ovmf_vars) = find_ovmf()?;
        let vars = match &config.efi_vars {
            Some(persistent) => std::path::absolute(persistent)?,
            None => dir.join("OVMF_VARS.fd"),
        };
        if config.efi_vars.is_none() || !vars.exists() {
            if let Some(parent) = vars.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&ovmf_vars, &vars)
                .with_context(|| format!("copying {}", ovmf_vars.display()))?;
        }

        let base_disk = std::path::absolute(&config.disk)
            .with_context(|| format!("resolving {}", config.disk.display()))?;
        if !base_disk.is_file() {
            bail!("disk image {} not found (write it with `cargo xtask disk`)", base_disk.display());
        }
        let (drive_file, drive_format, overlay) = if config.snapshot {
            let overlay = ws.state_dir().join("overlays").join(format!("{name}.qcow2"));
            fs::create_dir_all(overlay.parent().unwrap())?;
            let _ = fs::remove_file(&overlay);
            cmd::require("qemu-img")?;
            cmd::run(
                Command::new("qemu-img")
                    .args(["create", "-q", "-f", "qcow2", "-F"])
                    .arg(DiskFormat::from_path(&base_disk).name())
                    .arg("-b")
                    .arg(&base_disk)
                    .arg(&overlay),
            )?;
            (overlay.clone(), "qcow2", Some(overlay))
        } else {
            (base_disk.clone(), DiskFormat::from_path(&base_disk).name(), None)
        };

        let ssh_port = free_port()?;
        let qmp_sock = dir.join("qmp.sock");
        let vnc_sock = dir.join("vnc.sock");

        let mut qemu = Command::new("qemu-system-x86_64");
        // exit-with-parent: a killed harness or xtask must not leave a
        // multi-gigabyte VM (and its slot's RAM) running.
        qemu.args(["-name", &name])
            .args(["-run-with", "exit-with-parent=on"])
            .args(["-accel", "kvm", "-machine", "q35", "-cpu", "host"])
            .args(["-smp", &config.vcpus.to_string(), "-m", &config.memory])
            .arg("-drive")
            .arg(format!("if=pflash,format=raw,unit=0,readonly=on,file={}", ovmf_code.display()))
            .arg("-drive")
            .arg(format!("if=pflash,format=raw,unit=1,file={}", vars.display()))
            .arg("-drive")
            .arg(format!("if=virtio,format={drive_format},file={}", drive_file.display()));
        if let Some(iso) = &config.cdrom {
            qemu.arg("-drive")
                .arg(format!("media=cdrom,readonly=on,file={}", std::path::absolute(iso)?.display()));
        }
        for extra in &config.extra_disks {
            qemu.arg("-drive")
                .arg(format!("if=virtio,format=raw,file={}", std::path::absolute(extra)?.display()));
        }
        let gui = config.display == Display::Gtk;
        let gl = gui && config.gl;
        let vga = if gl { "virtio-vga-gl" } else { "virtio-vga" };
        qemu
            .arg("-device")
            .arg(format!("{vga},xres={},yres={}", config.width, config.height))
            .args(["-device", "virtio-tablet-pci", "-device", "virtio-keyboard-pci"])
            .args(["-object", "rng-random,id=rng0,filename=/dev/urandom"])
            .args(["-device", "virtio-rng-pci,rng=rng0"])
            .arg("-netdev")
            .arg(format!("user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"))
            .args(["-device", "virtio-net-pci,netdev=net0"])
            .arg("-smbios")
            .arg(format!(
                "type=11,value=io.systemd.credential.binary:ssh.authorized_keys.root={}",
                base64(key.public.as_bytes())
            ))
            .arg("-serial")
            .arg(format!("file:{}", dir.join("serial.log").display()))
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()));
        // QEMU refuses a VNC server next to a GL display ("Display vnc is
        // incompatible with the GL context").
        if !gl {
            qemu.arg("-vnc").arg(format!("unix:{}", vnc_sock.display()));
        }
        match config.display {
            Display::None => {
                qemu.args(["-display", "none"]);
            }
            Display::Gtk => {
                qemu.args(["-display", if gl { "gtk,gl=on" } else { "gtk" }]);
                gui_env(&mut qemu, gl);
            }
        }

        let log = File::create(dir.join("qemu.log"))?;
        eprintln!("$ {}", cmd::display(&qemu));
        let mut child = qemu
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .context("spawning qemu-system-x86_64")?;

        // QEMU creates the QMP socket before it starts the guest.
        let started = Instant::now();
        while !qmp_sock.exists() {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "qemu exited with {status} before it was ready:\n{}",
                    fs::read_to_string(dir.join("qemu.log")).unwrap_or_default()
                );
            }
            if started.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                bail!("qemu did not create {} within 30s", qmp_sock.display());
            }
            thread::sleep(Duration::from_millis(50));
        }

        let info = VmInfo {
            name,
            purpose: config.purpose.clone(),
            pid: child.id(),
            ssh_port,
            ssh_key: key.private,
            qmp_sock,
            vnc_sock,
            dir,
            width: config.width,
            height: config.height,
            display: match config.display {
                Display::None => "none",
                Display::Gtk if gl => "gtk-gl",
                Display::Gtk => "gtk",
            }
            .to_string(),
        };
        if let Err(e) = info.write() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(Vm { info, child, overlay, _slot: slot })
    }

    /// Has QEMU exited? (A guest that powers off ends the process.)
    pub fn exited(&mut self) -> Result<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    /// Block until QEMU exits (the window was closed, or the guest powered off).
    pub fn wait(&mut self) -> Result<std::process::ExitStatus> {
        Ok(self.child.wait()?)
    }

    fn ssh_base(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args(SSH_OPTS)
            .args(["-o", "ConnectTimeout=5", "-o", "ServerAliveInterval=10"])
            .arg("-i")
            .arg(&self.info.ssh_key)
            .args(["-p", &self.info.ssh_port.to_string()])
            .arg("root@127.0.0.1")
            .stdin(Stdio::null());
        c
    }

    /// Run `script` (a shell command line) as root in the guest.
    pub fn ssh(&self, script: &str) -> Result<Output> {
        let mut c = self.ssh_base();
        c.arg("--").arg(script);
        c.output().context("running ssh")
    }

    /// Like [`Vm::ssh`] but fail unless the command succeeded; returns
    /// stdout, trimmed.
    pub fn ssh_ok(&self, script: &str) -> Result<String> {
        let out = self.ssh(script)?;
        if !out.status.success() {
            bail!(
                "`{script}` in {} failed with {}:\n{}{}",
                self.info.name,
                out.status,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Wait until the guest accepts an SSH login. Fails early if QEMU dies.
    pub fn wait_ssh(&mut self, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.exited()? {
                bail!("qemu exited with {status} while waiting for SSH ({})", self.info.dir.display());
            }
            if self.ssh("true").is_ok_and(|o| o.status.success()) {
                return Ok(());
            }
            if started.elapsed() > timeout {
                bail!(
                    "{} did not accept SSH within {timeout:?} (serial log: {})",
                    self.info.name,
                    self.info.dir.join("serial.log").display()
                );
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    /// Reboot the guest (QEMU keeps running, so sockets and the disk
    /// persist) and wait until it accepts SSH again on a new boot.
    pub fn reboot(&mut self, timeout: Duration) -> Result<()> {
        const BOOT_ID: &str = "cat /proc/sys/kernel/random/boot_id";
        let before = self.ssh_ok(BOOT_ID)?;
        // The connection drops as the guest goes down; that is not an error.
        let _ = self.ssh("systemctl reboot");
        let started = Instant::now();
        loop {
            if let Some(status) = self.exited()? {
                bail!("qemu exited with {status} while rebooting");
            }
            if let Ok(out) = self.ssh(BOOT_ID)
                && out.status.success()
                && String::from_utf8_lossy(&out.stdout).trim() != before
            {
                return Ok(());
            }
            if started.elapsed() > timeout {
                bail!("{} did not come back from a reboot within {timeout:?}", self.info.name);
            }
            thread::sleep(Duration::from_secs(2));
        }
    }

    /// Poll `script` over SSH until it succeeds (exit 0), up to `timeout`.
    /// Returns its stdout.
    pub fn wait_for(&mut self, what: &str, script: &str, timeout: Duration) -> Result<String> {
        let started = Instant::now();
        loop {
            if let Some(status) = self.exited()? {
                bail!("qemu exited with {status} while waiting for {what}");
            }
            if let Ok(out) = self.ssh(script)
                && out.status.success()
            {
                return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
            }
            if started.elapsed() > timeout {
                bail!("timed out after {timeout:?} waiting for {what} (`{script}`)");
            }
            thread::sleep(Duration::from_secs(2));
        }
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_file(&self.info.qmp_sock);
        let _ = fs::remove_file(&self.info.vnc_sock);
        let _ = fs::remove_file(self.info.dir.join(INFO_FILE));
        if let Some(overlay) = &self.overlay {
            let _ = fs::remove_file(overlay);
        }
    }
}

/// Point the GTK window at WSLg when this is WSL.
///
/// With GL the window is a Wayland client of WSLg. Through XWayland, Mesa's
/// D3D12 driver (no DRI3 there) keeps presenting the window's first height
/// after it grows, and everything below stays garbage; GTK's own GL widget
/// on Wayland doesn't. Without GL, GTK on XWayland is the long-tested path.
fn gui_env(qemu: &mut Command, gl: bool) {
    const WSLG_WAYLAND: &str = "/mnt/wslg/runtime-dir/wayland-0";
    const WSLG_X11: &str = "/mnt/wslg/.X11-unix/X0";
    if gl && Path::new(WSLG_WAYLAND).exists() {
        // libwayland takes an absolute WAYLAND_DISPLAY as the socket path.
        qemu.env("GDK_BACKEND", "wayland").env("WAYLAND_DISPLAY", WSLG_WAYLAND).env_remove("DISPLAY");
        return;
    }
    let have_display = std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
        || std::env::var_os("WAYLAND_DISPLAY").is_some_and(|d| !d.is_empty());
    if !have_display && Path::new(WSLG_X11).exists() {
        qemu.env("DISPLAY", ":0");
    }
    if Path::new(WSLG_X11).exists() {
        qemu.env("GDK_BACKEND", "x11");
    }
}

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("finding a free TCP port")?;
    Ok(listener.local_addr()?.port())
}

/// OVMF firmware code and a matching variable store template.
fn find_ovmf() -> Result<(PathBuf, PathBuf)> {
    if let (Some(code), Some(vars)) = (std::env::var_os("ILEX_OVMF_CODE"), std::env::var_os("ILEX_OVMF_VARS")) {
        return Ok((code.into(), vars.into()));
    }
    const DIRS: &[&str] = &[
        "/usr/share/edk2/ovmf",
        "/usr/share/edk2/x64",
        "/usr/share/edk2-ovmf/x64",
        "/usr/share/OVMF",
        "/usr/share/qemu",
    ];
    const PAIRS: &[(&str, &str)] =
        &[("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"), ("OVMF_CODE.fd", "OVMF_VARS.fd")];
    for dir in DIRS {
        for (code, vars) in PAIRS {
            let (code, vars) = (Path::new(dir).join(code), Path::new(dir).join(vars));
            if code.is_file() && vars.is_file() {
                return Ok((code, vars));
            }
        }
    }
    bail!("OVMF firmware not found (install edk2-ovmf, or set ILEX_OVMF_CODE and ILEX_OVMF_VARS)")
}

/// Standard base64, enough for one SMBIOS credential.
pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Where a tier leaves evidence (screenshots, journals): `$ILEX_ARTIFACTS/<tier>`
/// (CI points this at the directory it uploads) or
/// `<state dir>/artifacts/<tier>`. Created if missing.
pub fn artifact_dir(ws: &Workspace, tier: &str) -> Result<PathBuf> {
    let base = match std::env::var_os("ILEX_ARTIFACTS").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => ws.state_dir().join("artifacts"),
    };
    let dir = base.join(tier);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Save `text` as an artifact file (ignores write errors beyond the first).
pub fn write_artifact(dir: &Path, name: &str, text: &str) -> Result<()> {
    let mut f = File::create(dir.join(name))?;
    f.write_all(text.as_bytes())?;
    Ok(())
}
