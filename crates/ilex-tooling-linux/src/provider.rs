//! Contract adapter around the existing QEMU/KVM implementation. Legacy
//! fixtures enter the same validator before taking slots or creating files.
use crate::{
    Workspace,
    qemu::{self, Display, VmConfig},
};
use anyhow::{Context, Result, bail, ensure};
use ilex_tooling_core::*;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn require_arch(guest: Architecture) -> Result<()> {
    crate::require_linux()?;
    ensure!(
        Architecture::host()? == Architecture::X86_64 && guest == Architecture::X86_64,
        "Linux legacy QEMU/bcvk backend only qualifies native x86_64; requested host {:?}, guest {guest:?}",
        Architecture::host()?
    );
    Ok(())
}

pub fn capabilities(display: &str, dimensions: (u32, u32)) -> VmCapabilities {
    use Capability::*;
    use CapabilityStatus::*;
    let mut features = BTreeMap::new();
    for feature in [
        PrebootKeyboard,
        SessionKeyboard,
        SessionPointer,
        Serial,
        Start,
        Stop,
        Destroy,
        Screenshot,
    ] {
        features.insert(feature, Available);
    }
    features.insert(
        PrebootPointer,
        NotYetQualified(
            "firmware pointer delivery has not been qualified; use preboot keyboard".into(),
        ),
    );
    features.insert(
        FirmwareEnforcement,
        NotYetQualified(
            "ordinary Linux defaults use nonsecure OVMF; secure qualification is T6".into(),
        ),
    );
    features.insert(
        PersistentTpm,
        NotYetQualified(
            "persistent OVMF/TPM bundle is not implemented in the ordinary Linux path".into(),
        ),
    );
    features.insert(
        Suspend,
        Unavailable("suspend is not part of the current provider lifecycle".into()),
    );
    features.insert(
        GpuRendering,
        if display == "gtk-gl" {
            if qemu::host_gl() {
                Available
            } else {
                Unavailable("no DRM render node".into())
            }
        } else {
            Unavailable("this VM has software virtio-vga".into())
        },
    );
    features.insert(
        Clipboard,
        Unavailable("clipboard integration is unavailable on Linux VMs".into()),
    );
    if display == "gtk-gl" {
        features.insert(
            Screenshot,
            Unavailable("QMP screendump cannot capture virgl GTK output (no surface)".into()),
        );
    }
    if !cfg!(target_os = "linux") {
        for status in features.values_mut() {
            *status = Unavailable("Linux backend requires Linux".into());
        }
    } else if let Err(error) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
    {
        features.insert(
            Start,
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                PermissionDenied(format!("opening /dev/kvm: {error}"))
            } else {
                Unavailable(format!("opening /dev/kvm: {error}"))
            },
        );
    }
    let can_capture = matches!(features.get(&Screenshot), Some(Available));
    VmCapabilities {
        provisioning: BTreeMap::new(),
        qualification: None,
        runtime: VmRuntimeStatus::Unknown,
        schema: SCHEMA_VERSION,
        provider: "linux-qemu-kvm".into(),
        vm_id: None,
        phase: BootPhase::Unknown,
        features,
        screenshot_source: can_capture.then(|| "qmp-screendump".into()),
        screenshot_dimensions: can_capture.then_some(dimensions),
        text_layout: Some("US physical keyboard; ASCII text only".into()),
    }
}

pub fn memory_bytes(value: &str) -> Result<u64> {
    let split = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let number: u64 = value[..split]
        .parse()
        .context("VM memory must be an integer with optional K/M/G/T suffix")?;
    let scale: u64 = match value[split..].to_ascii_uppercase().as_str() {
        "" | "M" => 1024 * 1024,
        "K" => 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024 * 1024 * 1024 * 1024,
        "B" => 1,
        _ => bail!("unsupported VM memory size {value}"),
    };
    number.checked_mul(scale).context("VM memory size overflow")
}

pub fn spec(ws: &Workspace, config: &VmConfig) -> Result<VmSpec> {
    let identity = ws.identity()?;
    let mut require = Vec::new();
    if config.gl {
        require.push(Capability::GpuRendering);
    }
    Ok(VmSpec {
        schema: SCHEMA_VERSION,
        accelerator: Accelerator::Native,
        security_launch: SecurityLaunch::RequireQualified,
        owner: identity.owner,
        run: format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ),
        purpose: config.purpose.clone(),
        host_arch: Architecture::host()?,
        guest_arch: Architecture::X86_64,
        boot: BootSource::Disk {
            path: config.disk.clone(),
            format: match qemu::DiskFormat::from_path(&config.disk) {
                qemu::DiskFormat::Qcow2 => DiskFormat::Qcow2,
                qemu::DiskFormat::Raw => DiskFormat::Raw,
            },
            writable: !config.snapshot,
        },
        iso: config.cdrom.clone(),
        additional_disks: config
            .extra_disks
            .iter()
            .map(|p| AttachedDisk {
                path: p.clone(),
                format: DiskFormat::Raw,
                blank_bytes: None,
                read_only: false,
            })
            .collect(),
        resources: Resources {
            memory_bytes: memory_bytes(&config.memory)?,
            vcpus: config.vcpus,
        },
        display: if config.display == Display::None {
            VmDisplay::Headless {
                width: config.width,
                height: config.height,
            }
        } else {
            VmDisplay::Window {
                width: config.width,
                height: config.height,
                gpu: if config.gl {
                    Requirement::Required
                } else {
                    Requirement::Off
                },
            }
        },
        firmware: Firmware::Uefi,
        firmware_state: config.efi_vars.clone(),
        secure_boot: Requirement::Off,
        tpm: TpmLifecycle::Off,
        network: NetworkIntent::User { ssh: true },
        ssh: None,
        require,
    })
}

pub fn validate_config(ws: &Workspace, config: &VmConfig) -> Result<VmSpec> {
    let spec = spec(ws, config)?;
    let provider = LinuxProvider::new(ws.clone())?;
    provider.validate(&spec)?;
    if config.gl {
        ensure!(
            config.display != Display::None,
            "GPU rendering requires a graphical display"
        );
        capabilities("gtk-gl", (config.width, config.height)).require(Capability::GpuRendering)?;
    }
    Ok(spec)
}

pub struct LinuxProvider {
    ws: Workspace,
    identity: CheckoutIdentity,
    usb_keyboard: bool,
    usb_medium: bool,
    esp_dir: Option<PathBuf>,
    disk_serials: bool,
    hotplug_port: bool,
}
pub struct LinuxVm {
    spec: VmSpec,
    running: Option<qemu::Vm>,
}
impl LinuxVm {
    /// Backend SSH/lifecycle adapter while portable guest execution evolves.
    pub fn running_mut(&mut self) -> Result<&mut qemu::Vm> {
        self.running.as_mut().context("VM has not been started")
    }
    /// Temporary fixture export retaining the same process, overlay and slot.
    pub fn into_legacy_vm(mut self) -> Result<qemu::Vm> {
        self.running.take().context("VM has not been started")
    }
}
impl LinuxProvider {
    pub fn new(ws: Workspace) -> Result<Self> {
        let identity = ws.identity()?;
        Ok(Self {
            ws,
            identity,
            usb_keyboard: false,
            usb_medium: false,
            esp_dir: None,
            disk_serials: false,
            hotplug_port: false,
        })
    }
    /// Give the guest an xHCI USB keyboard instead of a virtio one, for
    /// firmware that loses navigation keys from virtio-keyboard (GRUB menus
    /// driven through QMP). Off by default: it changes no existing launch.
    #[must_use]
    pub fn with_usb_keyboard(mut self, enabled: bool) -> Self {
        self.usb_keyboard = enabled;
        self
    }
    /// Attach the spec's ISO as a USB stick (`/dev/sdX`, removable) instead of
    /// an optical drive. Off by default: it changes no existing launch.
    #[must_use]
    pub fn with_usb_medium(mut self, enabled: bool) -> Self {
        self.usb_medium = enabled;
        self
    }
    /// Serve a directory as a small FAT disk, first in the boot order (see
    /// [`qemu::VmConfig::esp_dir`]): the boot loader of a medium that is a
    /// file on another disk. Off by default: it changes no existing launch.
    #[must_use]
    pub fn with_esp_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.esp_dir = Some(dir.into());
        self
    }
    /// Give the extra disks serial numbers, so that they have
    /// `/dev/disk/by-id` names (see [`qemu::VmConfig::disk_serials`]). Off by
    /// default: it changes no existing launch.
    #[must_use]
    pub fn with_disk_serials(mut self, enabled: bool) -> Self {
        self.disk_serials = enabled;
        self
    }
    /// An empty PCIe root port a disk can be added to while the guest runs
    /// (see [`qemu::VmConfig::hotplug_port`]). Off by default.
    #[must_use]
    pub fn with_hotplug_port(mut self, enabled: bool) -> Self {
        self.hotplug_port = enabled;
        self
    }
}
impl VmProvider for LinuxProvider {
    type Vm = LinuxVm;
    fn probe(&self, spec: &VmSpec) -> Result<VmCapabilities> {
        let (display, width, height) = match spec.display {
            VmDisplay::Headless { width, height } => ("none", width, height),
            VmDisplay::Window {
                width,
                height,
                gpu: Requirement::Required,
            } => ("gtk-gl", width, height),
            VmDisplay::Window { width, height, .. } => ("gtk", width, height),
        };
        let mut caps = capabilities(display, (width, height));
        if crate::security::requested(spec) {
            let status = match crate::security::validate(spec) {
                Ok(()) => CapabilityStatus::Available,
                Err(e) => CapabilityStatus::Unavailable(format!("{e:#}")),
            };
            caps.provisioning.insert(
                Capability::FirmwareEnforcement,
                if spec.secure_boot == Requirement::Required {
                    status.clone()
                } else {
                    CapabilityStatus::Unavailable("Secure Boot is off for this VM".into())
                },
            );
            caps.provisioning.insert(
                Capability::PersistentTpm,
                if matches!(spec.tpm, TpmLifecycle::Off) {
                    CapabilityStatus::Unavailable("TPM is off for this VM".into())
                } else {
                    status
                },
            );
            let profile = crate::qualification::profile(&self.ws, spec)?;
            let path = crate::qualification::record_path(&self.ws, &profile)?;
            let qualification = crate::qualification::load(&path, &spec.owner, &profile)?;
            crate::qualification::apply(&mut caps, qualification);
        }
        Ok(caps)
    }
    fn validate(&self, spec: &VmSpec) -> Result<()> {
        // Version/ownership precede any backend mutation.
        spec.validate(&self.identity, &self.probe(spec)?)?;
        self.probe(spec)?.require(Capability::Start)?;
        require_arch(spec.guest_arch)?;
        if crate::security::requested(spec) {
            crate::security::validate(spec)?;
            ensure!(
                matches!(spec.boot, BootSource::Disk { writable: true, .. }),
                "security bundle requires a persistent writable disk; create an owned overlay explicitly"
            );
        }
        ensure!(
            spec.host_arch == Architecture::host()?,
            "host architecture does not match VM request"
        );
        ensure!(
            spec.firmware == Firmware::Uefi,
            "legacy Linux provider only supports UEFI"
        );
        ensure!(
            matches!(spec.network, NetworkIntent::User { ssh: true }),
            "legacy Linux provider only supports user networking with SSH"
        );
        ensure!(
            spec.ssh.is_none(),
            "custom SSH provisioning is not implemented; Linux uses its per-checkout test key"
        );
        ensure!(
            spec.additional_disks
                .iter()
                .all(|d| d.format == DiskFormat::Raw && d.blank_bytes.is_none() && !d.read_only),
            "legacy Linux provider requires existing writable raw additional disks"
        );
        match &spec.boot {
            BootSource::Disk { format, .. } => ensure!(
                *format != DiskFormat::Vhdx,
                "VHDX boot is unsupported by Linux provider"
            ),
            BootSource::Image { .. } => {
                bail!("use the Linux bcvk ephemeral adapter for image boot")
            }
            BootSource::BlankDisk { .. } => bail!(
                "blank-disk allocation is not implemented by the legacy Linux provider; supply an existing owned blank disk"
            ),
        }
        Ok(())
    }
    fn create(&self, spec: VmSpec) -> Result<LinuxVm> {
        self.validate(&spec)?;
        Ok(LinuxVm {
            spec,
            running: None,
        })
    }
    fn start(&self, vm: &mut LinuxVm) -> Result<VmHandle> {
        self.validate(&vm.spec)?;
        ensure!(vm.running.is_none(), "VM is already started");
        let BootSource::Disk {
            path,
            format,
            writable,
        } = &vm.spec.boot
        else {
            unreachable!()
        };
        let expected = match qemu::DiskFormat::from_path(path) {
            qemu::DiskFormat::Qcow2 => DiskFormat::Qcow2,
            qemu::DiskFormat::Raw => DiskFormat::Raw,
        };
        ensure!(
            *format == expected,
            "disk format disagrees with the existing Linux filename convention"
        );
        let mut config = VmConfig::new(&vm.spec.purpose, path);
        config.snapshot = !writable;
        ensure!(
            vm.spec.resources.memory_bytes.is_multiple_of(1024),
            "Linux QEMU memory must be a whole KiB"
        );
        config.memory = format!("{}K", vm.spec.resources.memory_bytes / 1024);
        config.vcpus = vm.spec.resources.vcpus;
        match vm.spec.display {
            VmDisplay::Headless { width, height } => {
                config.width = width;
                config.height = height;
            }
            VmDisplay::Window { width, height, gpu } => {
                config.width = width;
                config.height = height;
                config.display = Display::Gtk;
                config.gl = gpu == Requirement::Required;
            }
        }
        config.cdrom = vm.spec.iso.clone();
        config.usb_keyboard = self.usb_keyboard;
        config.usb_medium = self.usb_medium;
        config.esp_dir = self.esp_dir.clone();
        config.disk_serials = self.disk_serials;
        config.hotplug_port = self.hotplug_port;
        config.efi_vars = vm.spec.firmware_state.clone();
        config.extra_disks = vm
            .spec
            .additional_disks
            .iter()
            .map(|d| d.path.clone())
            .collect();
        vm.running = Some(qemu::Vm::boot_validated(
            &self.ws,
            &config,
            vm.spec.clone(),
        )?);
        self.inspect(vm)
    }
    fn inspect(&self, vm: &LinuxVm) -> Result<VmHandle> {
        self.identity.authorize(&vm.spec.owner)?;
        let running = vm.running.as_ref().context("VM is not started")?;
        Ok(VmHandle {
            schema: SCHEMA_VERSION,
            owner: vm.spec.owner.clone(),
            run: vm.spec.run.clone(),
            provider: "linux-qemu-kvm".into(),
            opaque_id: format!("{}:{}", running.info.pid, running.info.process_start),
            guest_arch: vm.spec.guest_arch,
        })
    }
    fn stop(&self, vm: &mut LinuxVm) -> Result<()> {
        self.identity.authorize(&vm.spec.owner)?;
        drop(vm.running.take());
        Ok(())
    }
    fn destroy_owned(&self, vm: &mut LinuxVm) -> Result<()> {
        self.stop(vm)?;
        if crate::security::requested(&vm.spec) {
            crate::security::destroy_bundle(&self.ws, &vm.spec)?;
        }
        Ok(())
    }
    fn guest_endpoint(&self, vm: &LinuxVm) -> Result<GuestEndpoint> {
        self.identity.authorize(&vm.spec.owner)?;
        let info = &vm.running.as_ref().context("VM is not started")?.info;
        Ok(GuestEndpoint {
            host: "127.0.0.1".into(),
            port: info.ssh_port,
            user: "root".into(),
            host_key: None,
        })
    }
}

/// PID creation identity prevents stale metadata attaching to a reused PID.
pub fn process_start(pid: u32) -> Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat.rsplit_once(") ").context("invalid process stat")?.1;
    Ok(fields
        .split_whitespace()
        .nth(19)
        .context("missing process start ticks")?
        .into())
}

pub fn require_owned_path(root: &Path, path: &Path) -> Result<()> {
    ensure!(
        std::fs::canonicalize(path)?.starts_with(std::fs::canonicalize(root)?),
        "resource path escapes checkout-owned state"
    );
    Ok(())
}
