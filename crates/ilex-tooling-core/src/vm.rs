use std::path::PathBuf;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::{Architecture, Capability, CheckoutIdentity, VmCapabilities, check_version};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    Off,
    Required,
}

/// Hardware acceleration (KVM, WHPX or Hyper-V). ilex never emulates a guest CPU.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Accelerator {
    #[default]
    Native,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityLaunch {
    #[default]
    RequireQualified,
    /// Explicitly launch an unqualified measurement fixture. Prerequisites
    /// remain required and capability status is never promoted by this flag.
    QualificationFixture,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TpmLifecycle {
    Off,
    Persistent { bundle_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Firmware {
    Bios,
    Uefi,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VmDisplay {
    Headless {
        width: u32,
        height: u32,
    },
    Window {
        width: u32,
        height: u32,
        gpu: Requirement,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BootSource {
    BlankDisk {
        bytes: u64,
        format: DiskFormat,
    },
    Image {
        reference: String,
        executor_id: String,
    },
    Disk {
        path: PathBuf,
        format: DiskFormat,
        writable: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiskFormat {
    Qcow2,
    Raw,
    Vhdx,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachedDisk {
    pub path: PathBuf,
    pub format: DiskFormat,
    pub blank_bytes: Option<u64>,
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resources {
    pub memory_bytes: u64,
    pub vcpus: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NetworkIntent {
    Isolated,
    User { ssh: bool },
    Managed { network_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshProvisioning {
    pub user: String,
    pub public_key: String,
    pub recipe_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmSpec {
    pub schema: u32,
    #[serde(default)]
    pub accelerator: Accelerator,
    #[serde(default)]
    pub security_launch: SecurityLaunch,
    pub owner: String,
    pub run: String,
    pub purpose: String,
    pub host_arch: Architecture,
    pub guest_arch: Architecture,
    pub boot: BootSource,
    pub iso: Option<PathBuf>,
    pub additional_disks: Vec<AttachedDisk>,
    pub resources: Resources,
    pub display: VmDisplay,
    pub firmware: Firmware,
    pub firmware_state: Option<PathBuf>,
    pub secure_boot: Requirement,
    pub tpm: TpmLifecycle,
    pub network: NetworkIntent,
    pub ssh: Option<SshProvisioning>,
    pub require: Vec<Capability>,
}

pub fn validate_vm_purpose(purpose: &str) -> Result<()> {
    ensure!(
        !purpose.is_empty()
            && purpose.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid VM purpose (expected ASCII letters, digits or dashes)"
    );
    Ok(())
}

impl VmSpec {
    pub fn validate(&self, identity: &CheckoutIdentity, caps: &VmCapabilities) -> Result<()> {
        check_version(self.schema)?;
        check_version(caps.schema)?;
        identity.authorize(&self.owner)?;
        ensure!(!self.run.is_empty(), "VM run identity is empty");
        validate_vm_purpose(&self.purpose)?;
        ensure!(
            self.resources.memory_bytes > 0 && self.resources.vcpus > 0,
            "VM resources must be nonzero"
        );
        match self.display {
            VmDisplay::Headless { width, height } | VmDisplay::Window { width, height, .. } => {
                ensure!(
                    width > 0 && height > 0,
                    "framebuffer dimensions must be nonzero"
                );
            }
        }
        if matches!(
            self.display,
            VmDisplay::Window {
                gpu: Requirement::Required,
                ..
            }
        ) {
            caps.require(Capability::GpuRendering)?;
        }
        if self.secure_boot == Requirement::Required {
            self.require_security(caps, Capability::FirmwareEnforcement)?;
        }
        if !matches!(self.tpm, TpmLifecycle::Off) {
            self.require_security(caps, Capability::PersistentTpm)?;
        }
        for &feature in &self.require {
            if matches!(
                feature,
                Capability::FirmwareEnforcement | Capability::PersistentTpm
            ) {
                self.require_security(caps, feature)?;
            } else {
                caps.require(feature)?;
            }
        }
        Ok(())
    }
    fn require_security(&self, caps: &VmCapabilities, feature: Capability) -> Result<()> {
        if self.security_launch == SecurityLaunch::QualificationFixture {
            caps.require_provisioning(feature)
        } else {
            caps.require(feature)?;
            let profile = &caps.qualification.as_ref().unwrap().profile;
            ensure!(
                profile.accelerator == self.accelerator
                    && profile.resources == self.resources
                    && profile.guest_arch == self.guest_arch,
                "qualification does not match requested accelerator/resources/architecture"
            );
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmHandle {
    pub schema: u32,
    pub owner: String,
    pub run: String,
    pub provider: String,
    pub opaque_id: String,
    pub guest_arch: Architecture,
}

impl VmHandle {
    pub fn authorize(&self, identity: &CheckoutIdentity) -> Result<()> {
        check_version(self.schema)?;
        identity.authorize(&self.owner)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestEndpoint {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub host_key: Option<String>,
}

pub trait VmProvider {
    type Vm;
    fn probe(&self, spec: &VmSpec) -> Result<VmCapabilities>;
    fn validate(&self, spec: &VmSpec) -> Result<()>;
    fn create(&self, spec: VmSpec) -> Result<Self::Vm>;
    fn start(&self, vm: &mut Self::Vm) -> Result<VmHandle>;
    fn inspect(&self, vm: &Self::Vm) -> Result<VmHandle>;
    fn stop(&self, vm: &mut Self::Vm) -> Result<()>;
    /// Borrow the guard so an ownership error cannot implicitly drop/stop it.
    fn destroy_owned(&self, vm: &mut Self::Vm) -> Result<()>;
    fn guest_endpoint(&self, vm: &Self::Vm) -> Result<GuestEndpoint>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    FramebufferPixels,
    ClientPixels,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub vm: VmHandle,
    pub captured_unix_ms: u64,
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub viewport: Viewport,
    pub coordinate_space: CoordinateSpace,
    /// Packed RGB8, top-to-bottom rows. Backends normalize stride/format.
    pub pixels: Vec<u8>,
}

impl Frame {
    pub fn validate_pointer(&self, vm: &VmHandle, generation: u64, x: u32, y: u32) -> Result<()> {
        ensure!(
            &self.vm == vm && self.generation == generation,
            "stale frame or wrong VM; capture again after resize"
        );
        ensure!(
            x < self.width && y < self.height,
            "pointer outside captured framebuffer"
        );
        Ok(())
    }
}

/// Debug intentionally redacts text, including passwords supplied at preboot.
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Input {
    PhysicalKey {
        code: u32,
        down: bool,
    },
    Chord {
        codes: Vec<u32>,
    },
    Text {
        text: String,
        layout: String,
        sensitive: bool,
    },
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PhysicalKey { code, down } => f
                .debug_tuple("PhysicalKey")
                .field(code)
                .field(down)
                .finish(),
            Self::Chord { codes } => f.debug_tuple("Chord").field(codes).finish(),
            Self::Text {
                layout, sensitive, ..
            } => f
                .debug_struct("Text")
                .field("text", &"<redacted>")
                .field("layout", layout)
                .field("sensitive", sensitive)
                .finish(),
        }
    }
}

impl Input {
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Text { text, layout, .. } => {
                ensure!(text.len() <= 64 * 1024, "text input exceeds 64 KiB");
                ensure!(
                    !layout.is_empty(),
                    "text input needs an explicit keyboard layout"
                );
            }
            Self::Chord { codes } => ensure!(
                !codes.is_empty() && codes.len() <= 16,
                "chord needs 1..=16 physical keys"
            ),
            _ => {}
        }
        Ok(())
    }
}

pub trait VmControl {
    fn capabilities(&self) -> Result<VmCapabilities>;
    fn capture(&self) -> Result<Frame>;
    fn key(&self, input: &Input) -> Result<()>;
    fn pointer(&self, frame: &Frame, generation: u64, x: u32, y: u32) -> Result<()>;
    fn serial(&self) -> Result<Vec<u8>>;
    fn guest_exec(&self, argv: &[String]) -> Result<Vec<u8>>;
    fn upload(&self, local: &std::path::Path, guest: &str) -> Result<()>;
    fn download(&self, guest: &str, local: &std::path::Path) -> Result<()>;
}
