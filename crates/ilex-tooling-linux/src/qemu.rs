//! Disk images and QEMU: write a disk with `bcvk to-disk`, boot it under KVM
//! the way the plan's "Display device" section describes, and reach it over
//! SSH.
//!
//! One [`Vm`] is one `qemu-system-x86_64` process holding a machine-wide VM
//! slot ([`crate::slots`]) for as long as it lives. Everything a client needs
//! to attach (`ilexctl vm`, a human with a VNC viewer) is published in
//! `$XDG_RUNTIME_DIR/ilex-vm/<slug>/<purpose>/`:
//!
//! ```text
//! qmp.sock   QMP control socket (screendump, input-send-event)
//! vnc.sock   VNC server on a unix socket, for a human watching
//! vm.toml    TOML description of the running VM (see [`VmInfo`])
//! qemu.log   QEMU's stdout/stderr
//! serial.log guest serial console
//! firmware.log OVMF's debug console (only with `ILEX_VM_FIRMWARE_LOG=1`)
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
        Self {
            image: image.into(),
            format: DiskFormat::Qcow2,
            size: "30G".into(),
            out: None,
        }
    }
}

/// `<state dir>/disks/<image, made path-safe>.<ext>`.
pub fn default_disk_path(ws: &Workspace, image: &str, format: DiskFormat) -> PathBuf {
    let safe: String = image
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    ws.state_dir().join("disks").join(format!(
        "{safe}-{}.{}",
        &ilex_tooling_core::digest(image.as_bytes())[7..23],
        format.extension()
    ))
}

/// Write (or refresh) the disk for `spec` with `bcvk to-disk` and return its
/// path. bcvk records what a disk was built from and skips the install when
/// the image hasn't changed, so calling this before every boot is cheap.
pub fn ensure_disk(ws: &Workspace, spec: &DiskSpec) -> Result<PathBuf> {
    ws.ensure_state()?;
    ws.verify_implicit_image(&spec.image)?;
    crate::provider::require_arch(ilex_tooling_core::Architecture::X86_64)?;
    cmd::require("bcvk")?;
    let out = spec
        .out
        .clone()
        .unwrap_or_else(|| default_disk_path(ws, &spec.image, spec.format));
    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // Tiers run in parallel threads and each wants the disk: one writer at a
    // time, the rest wait and then find it up to date.
    let lock_path = out.with_file_name(format!(
        "{}.lock",
        out.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    ));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("opening {}", lock_path.display()))?;
    lock.lock()
        .with_context(|| format!("locking {}", lock_path.display()))?;
    crate::ownership::claim_file(ws, &out)?;

    // bcvk's inner containers-storage transport parses `sha256:...` as a
    // registry name, unlike podman inspect. Give it an owned content-addressed
    // local alias while retaining the original immutable ID as the disk key.
    let source_alias = if let Some(digest) = spec.image.strip_prefix("sha256:") {
        anyhow::ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source image ID"
        );
        // A fresh versioned alias avoids implicitly adopting aliases allocated
        // by the old, unreceipted path. The helper retains a store-wide lease.
        let alias = format!("{}-disk-v1-{}", ws.image_ref(), &digest[..16]);
        Some(crate::ownership::ImageAlias::for_image_id(
            ws,
            &alias,
            &spec.image,
        )?)
    } else {
        None
    };
    let source = source_alias
        .as_ref()
        .map_or(spec.image.as_str(), |alias| alias.reference());
    let mut bcvk = Command::new("bcvk");
    // Installation boots a real VM too. Share the same slots as tests instead
    // of allocating it outside the cap on a busy Linux host.
    let _install_slot = slots::acquire(&format!("{}disk-install", ws.vm_prefix()))?;
    let logs = ws
        .ensure_runtime()?
        .join(format!("disk-install-{}", std::process::id()));
    fs::create_dir_all(&logs)?;
    bcvk.args(["to-disk", "--format", spec.format.name()])
        .arg("--console")
        .arg("--log-dir")
        .arg(format!("journal,console={}", logs.display()))
        .args(["--disk-size", &spec.size])
        .arg(source)
        .arg(&out);
    cmd::run(&mut bcvk)?;
    if let Some(alias) = &source_alias {
        alias.verify()?;
    }
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
    ws.ensure_state()?;
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
    #[serde(default)]
    pub security_profile: Option<ilex_tooling_core::SecurityProfile>,
    #[serde(default)]
    pub qualification_record: Option<PathBuf>,
    #[serde(default)]
    pub accelerator: ilex_tooling_core::Accelerator,
    #[serde(default)]
    pub secure_boot: bool,
    #[serde(default)]
    pub persistent_tpm: bool,
    #[serde(default)]
    pub schema: u32,
    pub owner: String,
    pub run: String,
    pub architecture: ilex_tooling_core::Architecture,
    /// Linux process start ticks; PID alone can refer to an unrelated process.
    pub process_start: String,
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
    #[serde(default)]
    pub gpu: bool,
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
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut info: VmInfo =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        ilex_tooling_core::check_version(info.schema)?;
        info.dir = dir.to_path_buf();
        Ok(info)
    }

    /// Is the QEMU process still there?
    pub fn is_running(&self) -> bool {
        crate::provider::process_start(self.pid).is_ok_and(|start| start == self.process_start)
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

/// Every VM of this checkout whose QEMU is still running, sorted by purpose.
pub fn running_vms(ws: &Workspace) -> Vec<VmInfo> {
    let Ok(entries) = fs::read_dir(ws.runtime_dir()) else {
        return Vec::new();
    };
    let mut vms: Vec<VmInfo> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| VmInfo::read(&e.path()).ok())
        .filter(|info| {
            ws.root.as_os_str().is_empty() || ws.identity().is_ok_and(|id| id.owner == info.owner)
        })
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
            format!(
                "no running `{p}` VM for `{}` (boot one with `cargo ilexctl vm run --gui`)",
                ws.slug
            )
        }),
        None => match vms.len() {
            0 => bail!(
                "no VM running for `{}` (boot one with `cargo ilexctl vm run --gui`)",
                ws.slug
            ),
            1 => Ok(vms.into_iter().next().unwrap()),
            _ => {
                let names: Vec<_> = vms.iter().map(|v| v.purpose.as_str()).collect();
                bail!(
                    "several VMs are running for `{}` ({}); pick one with --vm",
                    ws.slug,
                    names.join(", ")
                )
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Booting
// ---------------------------------------------------------------------------

/// How long a started QEMU may take to answer its first QMP client. It does so
/// within a few tens of milliseconds, loaded or not.
const QMP_READY_TIMEOUT: Duration = Duration::from_secs(30);

const SSH_OPTS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "LogLevel=ERROR",
    "-o",
    "IdentitiesOnly=yes",
    "-o",
    "BatchMode=yes",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Display {
    /// No window; VNC and QMP only. What tests use, so every run gets the
    /// same framebuffer size.
    None,
    /// A GTK window for a human. The guest follows the window's size.
    Gtk,
}

/// Virgl rendering on Linux requires a DRM render node.
pub fn host_gl() -> bool {
    fs::read_dir("/dev/dri")
        .map(|dir| {
            dir.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        })
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
    /// Enable Flathub downloads; harnesses opt in explicitly.
    pub apps: bool,
    /// Keep SSH on a restricted management NIC while testing an offline link.
    pub isolated_ssh: bool,
    pub memory: String,
    pub vcpus: u32,
    /// virtio-vga preferred mode, read by the guest from EDID.
    pub width: u32,
    pub height: u32,
    pub display: Display,
    /// With [`Display::Gtk`]: give the guest a
    /// 3D-capable `virtio-vga-gl` whose OpenGL the host renders (virgl),
    /// instead of llvmpipe in the guest. QEMU then has no VNC server and QMP
    /// `screendump` can't capture the screen. See
    /// [`host_gl`].
    pub gl: bool,
    /// An install ISO attached as a CD-ROM. Firmware tries the disk first and
    /// falls through to the CD when the disk is empty.
    pub cdrom: Option<PathBuf>,
    /// Attach a second UART, saved as `serial-secondary.log`, for observing
    /// faults in the primary diagnostic sink independently.
    pub secondary_serial: bool,
    /// Use an xHCI USB keyboard for firmware input. Some OVMF builds lose
    /// navigation/modifier keys from virtio-keyboard before the OS boots.
    pub usb_keyboard: bool,
    /// Attach `cdrom` as a USB mass-storage disk (a stick the ISO was written
    /// to: `/dev/sdX` with its partitions, removable) instead of an optical
    /// drive (`/dev/sr0`).
    pub usb_medium: bool,
    /// Extra raw disks, attached read-write after the main one (an installer
    /// finds its kickstart on one labelled `OEMDRV`).
    pub extra_disks: Vec<PathBuf>,
    /// Raw images attached as removable USB sticks (`/dev/sdX`, read-write,
    /// serial `ILEXSTICK<n>`) beside the ISO's stick or alone: where an
    /// installer's log goes. They are not boot devices.
    pub extra_usb_disks: Vec<PathBuf>,
    /// Give each disk a serial number (`ilex-disk-0` for the main disk, then
    /// `ilex-disk-1`, `ilex-disk-2`, ... for the extra ones), which udev turns
    /// into a `/dev/disk/by-id/virtio-<serial>` name. Without one a virtio disk has
    /// no by-id name at all (its id is its PCI slot), and an installation that
    /// writes a disk by that name has no name to write it by.
    pub disk_serials: bool,
    /// With [`disk_serials`](Self::disk_serials): the last this many extra disks
    /// get none, so that they have no `/dev/disk/by-id` name (a disk whose only
    /// stable name is its bus path).
    pub bare_disks: usize,
    /// A PCIe root port with nothing in it (`ilexhp`), which a disk can be
    /// added to while the guest runs (QMP `device_add ... bus=ilexhp`): the
    /// stick someone plugs in after the boot.
    pub hotplug_port: bool,
    /// A directory served read-only as a small FAT disk (QEMU's `fat:`), first
    /// in the boot order: an EFI system partition whose `EFI/BOOT/BOOTX64.EFI`
    /// is the boot loader. For booting an ISO that is a *file* on another disk
    /// (GRUB's loopback, as Ventoy and iso-scan boots do), which no attached
    /// ISO shows. It is attached last, so the other disks keep their names.
    pub esp_dir: Option<PathBuf>,
    /// Give the guest the harness's SSH key as the `ssh.authorized_keys.root`
    /// credential (the default). An image that keeps its SSH server off
    /// unless that credential exists can be booted without it.
    pub ssh_credential: bool,
    /// Extra SMBIOS structures; double commas inside values for QEMU.
    pub smbios: Vec<String>,
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
            apps: false,
            isolated_ssh: false,
            memory: "4G".into(),
            vcpus: 2,
            width: 1280,
            height: 800,
            display: Display::None,
            gl: false,
            cdrom: None,
            secondary_serial: false,
            usb_keyboard: false,
            usb_medium: false,
            extra_disks: Vec::new(),
            extra_usb_disks: Vec::new(),
            disk_serials: false,
            bare_disks: 0,
            hotplug_port: false,
            esp_dir: None,
            ssh_credential: true,
            smbios: Vec::new(),
            efi_vars: None,
        }
    }

    /// The plan's graphical test machine: 8 GiB, 4 vCPUs, 3840x2160.
    pub fn graphical(purpose: impl Into<String>, disk: impl Into<PathBuf>) -> Self {
        Self {
            memory: "8G".into(),
            vcpus: 4,
            width: 3840,
            height: 2160,
            ..Self::new(purpose, disk)
        }
    }
}

/// A booted VM. Dropping it kills QEMU, removes its sockets and overlay and
/// frees its slot.
#[derive(Debug)]
pub struct Vm {
    pub info: VmInfo,
    child: Child,
    overlay: Option<PathBuf>,
    _security: Option<crate::security::Bundle>,
    // Freed after QEMU and swtpm; the lifetime name lease follows it.
    _slot: VmSlot,
    _lease: fs::File,
}

impl Vm {
    /// Take a VM slot (waiting if none is free) and start QEMU. Returns as
    /// soon as QEMU is up; use [`Vm::wait_ssh`] to wait for the guest.
    pub fn boot(ws: &Workspace, config: &VmConfig) -> Result<Vm> {
        let spec = crate::provider::validate_config(ws, config)?;
        Self::boot_validated(ws, config, spec)
    }

    pub(crate) fn boot_validated(
        ws: &Workspace,
        config: &VmConfig,
        spec: ilex_tooling_core::VmSpec,
    ) -> Result<Vm> {
        cmd::require("qemu-system-x86_64")?;
        if spec.accelerator == ilex_tooling_core::Accelerator::Native
            && !Path::new("/dev/kvm").exists()
        {
            bail!("/dev/kvm not found: booting VMs needs KVM");
        }
        let name = ws.vm_name(&config.purpose);
        ws.ensure_state()?;
        let lease = crate::ownership::vm_lease(ws, &name)?;
        let slot = slots::acquire(&name)?;
        let dir = ws.runtime_dir().join(&config.purpose);
        if dir.join(INFO_FILE).exists() {
            let old = VmInfo::read(&dir)?;
            anyhow::ensure!(
                old.owner == spec.owner,
                "{name} metadata belongs to another checkout; refusing to replace it"
            );
            anyhow::ensure!(
                !old.is_running(),
                "{name} is already running (pid {}); stop its owning launcher first",
                old.pid
            );
        }
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let key = test_key(ws)?;
        let security = if crate::security::requested(&spec) {
            Some(crate::security::Bundle::prepare(ws, &spec, &dir)?)
        } else {
            None
        };
        let (ovmf_code, ovmf_vars) = match &security {
            Some(bundle) => (bundle.code.clone(), bundle.vars.clone()),
            None => find_ovmf()?,
        };
        let vars = match &config.efi_vars {
            Some(persistent) => std::path::absolute(persistent)?,
            None if security.is_some() => ovmf_vars.clone(),
            None => dir.join("OVMF_VARS.fd"),
        };
        if config.efi_vars.is_some() && !vars.starts_with(ws.state_dir()) {
            if let Some(parent) = vars.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::ownership::claim_file(ws, &vars)?;
        }
        if security.is_none() && (config.efi_vars.is_none() || !vars.exists()) {
            if let Some(parent) = vars.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&ovmf_vars, &vars)
                .with_context(|| format!("copying {}", ovmf_vars.display()))?;
        }

        let base_disk = std::path::absolute(&config.disk)
            .with_context(|| format!("resolving {}", config.disk.display()))?;
        if !base_disk.is_file() {
            bail!(
                "disk image {} not found (write it with `cargo ilexctl disk`)",
                base_disk.display()
            );
        }
        for writable in config
            .extra_disks
            .iter()
            .chain((!config.snapshot).then_some(&base_disk))
        {
            if crate::provider::require_owned_path(&ws.state_dir(), writable).is_err() {
                crate::ownership::claim_file(ws, writable)?;
            }
        }
        let (drive_file, drive_format, overlay) = if config.snapshot {
            let overlay = ws
                .state_dir()
                .join("overlays")
                .join(format!("{name}.qcow2"));
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
            (
                base_disk.clone(),
                DiskFormat::from_path(&base_disk).name(),
                None,
            )
        };

        let ssh_port = free_port()?;
        let qmp_sock = dir.join("qmp.sock");
        let vnc_sock = dir.join("vnc.sock");

        let security_profile = if security.is_some() {
            Some(crate::qualification::profile(ws, &spec)?)
        } else {
            None
        };
        let qualification_record = security_profile
            .as_ref()
            .map(|p| crate::qualification::record_path(ws, p))
            .transpose()?;
        let mut qemu = Command::new("qemu-system-x86_64");
        // exit-with-parent: a killed harness or xtask must not leave a
        // multi-gigabyte VM (and its slot's RAM) running.
        qemu.args(["-name", &name])
            .args(["-run-with", "exit-with-parent=on"])
            .args([
                "-accel",
                "kvm",
                "-machine",
                if security.is_some() {
                    "q35,smm=on"
                } else {
                    "q35"
                },
                "-cpu",
                "host",
            ])
            .args(["-smp", &config.vcpus.to_string(), "-m", &config.memory])
            .arg("-drive")
            .arg(format!(
                "if=pflash,format=raw,unit=0,readonly=on,file={}",
                ovmf_code.display()
            ))
            .arg("-drive")
            .arg(format!(
                "if=pflash,format=raw,unit=1,file={}",
                vars.display()
            ))
            .arg("-drive")
            .arg(if config.disk_serials {
                // A serial number is a property of the device (QEMU's legacy
                // `-drive serial=` is refused for qcow2), so the disk is a
                // device of its own, created where the legacy one would be.
                format!(
                    "if=none,id=ilexd0,format={drive_format},file={}",
                    drive_file.display()
                )
            } else {
                format!(
                    "if=virtio,format={drive_format},file={}",
                    drive_file.display()
                )
            });
        // The devices of disks that have serial numbers: created after every
        // other device (below), as QEMU creates a legacy `if=virtio` drive's
        // device, so that they keep the PCI slots (and the names, and what
        // firmware sees of them) a disk without a serial number has.
        let mut serial_devices: Vec<String> = Vec::new();
        if config.disk_serials {
            serial_devices.push("virtio-blk-pci,drive=ilexd0,serial=ilex-disk-0".into());
        }
        if security.is_some() {
            qemu.args(["-global", "driver=cfi.pflash01,property=secure,value=on"]);
        }
        if !matches!(spec.tpm, ilex_tooling_core::TpmLifecycle::Off) {
            qemu.arg("-chardev")
                .arg(format!(
                    "socket,id=chrtpm,path={}",
                    dir.join("tpm.sock").display()
                ))
                .args([
                    "-tpmdev",
                    "emulator,id=tpm0,chardev=chrtpm",
                    "-device",
                    "tpm-tis,tpmdev=tpm0",
                ]);
        }
        // A stick (and its controller, `ilexusb`) exists only with an ISO to
        // put on it; `usb_medium` alone adds nothing.
        let usb_stick = config.usb_medium && config.cdrom.is_some();
        if usb_stick || !config.extra_usb_disks.is_empty() {
            // The controller the sticks are on.
            qemu.args(["-device", "qemu-xhci,id=ilexusb"]);
        }
        if let Some(iso) = &config.cdrom {
            if usb_stick {
                // A stick: the whole ISO as a removable USB disk, first in
                // the boot order, on a controller of its own. The guest can
                // write to it, as to a real stick (a read-only disk is not
                // offered as a target at all); its writes go to a temporary
                // overlay, never to the ISO.
                qemu.arg("-drive")
                    .arg(format!(
                        "if=none,id=medium,media=disk,format=raw,snapshot=on,file={}",
                        std::path::absolute(iso)?.display()
                    ))
                    .args([
                        "-device",
                        "usb-storage,bus=ilexusb.0,drive=medium,removable=on,bootindex=1",
                    ]);
            } else {
                qemu.arg("-drive").arg(format!(
                    "media=cdrom,readonly=on,file={}",
                    std::path::absolute(iso)?.display()
                ));
            }
        }
        for (n, stick) in config.extra_usb_disks.iter().enumerate() {
            qemu.arg("-drive").arg(format!(
                "if=none,id=stick{n},media=disk,format=raw,file={}",
                std::path::absolute(stick)?.display()
            ));
            qemu.args([
                "-device",
                &format!(
                    "usb-storage,bus=ilexusb.0,drive=stick{n},removable=on,serial=ILEXSTICK{n}"
                ),
            ]);
        }
        let serialed = config.extra_disks.len().saturating_sub(config.bare_disks);
        for (index, extra) in config.extra_disks.iter().enumerate() {
            if config.disk_serials && index < serialed {
                let n = index + 1;
                qemu.arg("-drive").arg(format!(
                    "if=none,id=ilexd{n},format=raw,file={}",
                    std::path::absolute(extra)?.display()
                ));
                serial_devices.push(format!(
                    "virtio-blk-pci,drive=ilexd{n},serial=ilex-disk-{n}"
                ));
            } else {
                qemu.arg("-drive").arg(format!(
                    "if=virtio,format=raw,file={}",
                    std::path::absolute(extra)?.display()
                ));
            }
        }
        if config.hotplug_port {
            qemu.args([
                "-device",
                "pcie-root-port,id=ilexhp,bus=pcie.0,chassis=9,slot=9",
            ]);
        }
        if let Some(dir) = &config.esp_dir {
            // Two devices never share a boot index: with a stick (first) the
            // loader disk comes second, and without one it is first.
            let bootindex = if usb_stick { 2 } else { 1 };
            qemu.arg("-drive")
                .arg(format!(
                    "if=none,id=esp,format=raw,readonly=on,file=fat:{}",
                    std::path::absolute(dir)?.display()
                ))
                .args([
                    "-device",
                    &format!("virtio-blk-pci,drive=esp,bootindex={bootindex}"),
                ]);
        }
        let gl = config.gl && config.display != Display::None;
        let vga = if gl { "virtio-vga-gl" } else { "virtio-vga" };
        qemu.arg("-device")
            .arg(format!(
                "{vga},xres={},yres={}",
                config.width, config.height
            ))
            .args(["-device", "virtio-tablet-pci"]);
        if config.usb_keyboard && usb_stick {
            qemu.args(["-device", "usb-kbd,bus=ilexusb.0"]);
        } else if config.usb_keyboard {
            qemu.args(["-device", "qemu-xhci", "-device", "usb-kbd"]);
        } else {
            qemu.args(["-device", "virtio-keyboard-pci"]);
        }
        qemu.args(["-object", "rng-random,id=rng0,filename=/dev/urandom"])
            .args(["-device", "virtio-rng-pci,rng=rng0"])
            .arg("-netdev")
            .arg(if config.isolated_ssh {
                "user,id=net0".to_string()
            } else {
                format!("user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22")
            })
            .args(["-device", "virtio-net-pci,netdev=net0,id=internet"]);
        if config.ssh_credential {
            qemu.arg("-smbios").arg(format!(
                "type=11,value=io.systemd.credential.binary:ssh.authorized_keys.root={}",
                base64(key.public.as_bytes())
            ));
        }
        if std::env::var_os("ILEX_VM_FIRMWARE_LOG").is_some_and(|v| !v.is_empty()) {
            qemu.arg("-debugcon")
                .arg(format!("file:{}", dir.join("firmware.log").display()))
                .args(["-global", "isa-debugcon.iobase=0x402"]);
        }
        qemu.arg("-serial")
            .arg(format!("file:{}", dir.join("serial.log").display()))
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()));
        if config.secondary_serial {
            qemu.arg("-serial").arg(format!(
                "file:{}",
                dir.join("serial-secondary.log").display()
            ));
        }
        if config.isolated_ssh {
            qemu.arg("-netdev").arg(format!("user,id=management,restrict=on,net=10.0.3.0/24,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"))
                .args(["-device", "virtio-net-pci,netdev=management,id=management"]);
        }
        for structure in &config.smbios {
            qemu.args(["-smbios", structure]);
        }
        if !config.apps {
            qemu.arg("-smbios").arg(format!(
                "type=11,value=io.systemd.credential.binary:systemd.unit-dropin.ilex-apps.timer={}",
                base64(b"[Unit]\nConditionPathExists=/run/ilex-harness-enable-apps\n")
            ));
        }
        // The VNC server asks for an audio backend, and QEMU 11 then probes its
        // list in order, PipeWire first. Without a PipeWire daemon the probe
        // fails and tears its thread loop down with `pw_thread_loop_stop`, which
        // in libpipewire intermittently never returns (the loop thread idles in
        // `epoll_wait`, QEMU's main thread waits in `pthread_join`): the guest
        // never starts, its serial console stays empty and nothing answers on
        // QMP. About one boot in a hundred hung that way on the Boat hosts. No
        // test wants sound, so name the one backend that does nothing: QEMU
        // then loads and probes none (strace: no audio module, no connect to a
        // PipeWire or PulseAudio socket).
        qemu.args(["-audiodev", "none,id=ilex-noaudio"]);
        for device in &serial_devices {
            qemu.args(["-device", device]);
        }
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

        // QEMU binds the QMP socket (creating its path) before it listens, and
        // answers only once its main loop runs; a QEMU that hangs while starting
        // up never does, and the guest it never started would otherwise be waited
        // for until a marker deadline. One deadline covers the path appearing,
        // the connection and the handshake, and the process is always reaped.
        if let Err(e) = crate::qmp::Qmp::connect_ready(&qmp_sock, QMP_READY_TIMEOUT, || {
            Ok(child.try_wait()?.is_some())
        }) {
            let log = fs::read_to_string(dir.join("qemu.log")).unwrap_or_default();
            if let Some(status) = child.try_wait()? {
                bail!("qemu exited with {status} before it was ready:\n{log}");
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "qemu (pid {}) was not ready on {} within {QMP_READY_TIMEOUT:?}, so it hung while starting: {e:#}\nqemu.log:\n{log}",
                child.id(),
                qmp_sock.display(),
            );
        }

        let info = VmInfo {
            security_profile,
            qualification_record,
            accelerator: spec.accelerator,
            secure_boot: spec.secure_boot == ilex_tooling_core::Requirement::Required,
            persistent_tpm: !matches!(spec.tpm, ilex_tooling_core::TpmLifecycle::Off),
            gpu: gl,
            schema: ilex_tooling_core::SCHEMA_VERSION,
            owner: spec.owner,
            run: spec.run,
            architecture: spec.guest_arch,
            process_start: crate::provider::process_start(child.id())?,
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
        Ok(Vm {
            info,
            child,
            overlay,
            _security: security,
            _slot: slot,
            _lease: lease,
        })
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
            if self.info.secure_boot
                && fs::read_to_string(self.info.dir.join("qemu.log"))
                    .is_ok_and(|log| log.contains("KVM: entry failed"))
            {
                crate::qualification::runtime_failure(
                    &self.info,
                    "KVM entry failed (SMM); see retained qemu.log",
                )?;
                bail!(
                    "secure KVM guest is unqualified: KVM entry failed (see {})",
                    self.info.dir.join("qemu.log").display()
                );
            }
            if let Some(status) = self.exited()? {
                bail!(
                    "qemu exited with {status} while waiting for SSH ({})",
                    self.info.dir.display()
                );
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
                bail!(
                    "{} did not come back from a reboot within {timeout:?}",
                    self.info.name
                );
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

fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("finding a free TCP port")?;
    Ok(listener.local_addr()?.port())
}

/// OVMF firmware code and a matching variable store template.
fn find_ovmf() -> Result<(PathBuf, PathBuf)> {
    if let (Some(code), Some(vars)) = (
        std::env::var_os("ILEX_OVMF_CODE"),
        std::env::var_os("ILEX_OVMF_VARS"),
    ) {
        return Ok((code.into(), vars.into()));
    }
    const DIRS: &[&str] = &[
        "/usr/share/edk2/ovmf",
        "/usr/share/edk2/x64",
        "/usr/share/edk2-ovmf/x64",
        "/usr/share/OVMF",
        "/usr/share/qemu",
    ];
    const PAIRS: &[(&str, &str)] = &[
        ("OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"),
        ("OVMF_CODE.fd", "OVMF_VARS.fd"),
    ];
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
    ws.ensure_state()?;
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
