use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    GpuRendering,
    FirmwareEnforcement,
    PersistentTpm,
    Screenshot,
    PrebootKeyboard,
    SessionKeyboard,
    PrebootPointer,
    SessionPointer,
    Clipboard,
    Suspend,
    Serial,
    Start,
    Stop,
    Destroy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum CapabilityStatus {
    Available,
    Unavailable(String),
    NotYetQualified(String),
    PermissionDenied(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BootPhase {
    Firmware,
    Boot,
    Session,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmCapabilities {
    pub schema: u32,
    pub provider: String,
    pub vm_id: Option<String>,
    pub phase: BootPhase,
    pub features: BTreeMap<Capability, CapabilityStatus>,
    /// Prerequisites only. This map never attests guest enforcement/persistence.
    #[serde(default)]
    pub provisioning: BTreeMap<Capability, CapabilityStatus>,
    #[serde(default)]
    pub qualification: Option<SecurityQualification>,
    #[serde(default)]
    pub runtime: VmRuntimeStatus,
    pub screenshot_source: Option<String>,
    pub screenshot_dimensions: Option<(u32, u32)>,
    pub text_layout: Option<String>,
}

impl VmCapabilities {
    pub fn require_provisioning(&self, feature: Capability) -> Result<()> {
        match self.provisioning.get(&feature) {
            Some(CapabilityStatus::Available) => Ok(()),
            other => bail!("{}: provisioning {feature:?} is {other:?}", self.provider),
        }
    }
    pub fn require(&self, feature: Capability) -> Result<()> {
        if matches!(
            feature,
            Capability::FirmwareEnforcement | Capability::PersistentTpm
        ) {
            self.require_provisioning(feature)?;
            anyhow::ensure!(
                self.qualification
                    .as_ref()
                    .is_some_and(|q| q.outcome == QualificationStatus::Passed),
                "{}: {feature:?} has no passing profile qualification",
                self.provider
            );
            anyhow::ensure!(
                matches!(
                    self.runtime,
                    VmRuntimeStatus::Unknown | VmRuntimeStatus::Running
                ),
                "{}: guest runtime is {:?}",
                self.provider,
                self.runtime
            );
        }
        match self.features.get(&feature) {
            Some(CapabilityStatus::Available) => Ok(()),
            Some(status) => bail!(
                "{}: required capability {feature:?} is {status:?}",
                self.provider
            ),
            None => bail!(
                "{}: required capability {feature:?} was not probed",
                self.provider
            ),
        }
    }
}

/// Exact qualification scope, shared by providers, vmctl and part-4 consumers.
/// Host/backend changes invalidate evidence; RAM/vCPU values are never inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityProfile {
    pub protocol: u32,
    pub accelerator: crate::Accelerator,
    pub resources: crate::Resources,
    pub cpu_model: String,
    pub guest_arch: crate::Architecture,
    pub host: BTreeMap<String, String>,
    pub firmware: BTreeMap<String, String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum QualificationStatus {
    #[default]
    Unknown,
    Qualifying,
    Passed,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualificationEvidence {
    pub owner: String,
    pub run: String,
    pub recorded_unix_ms: u64,
    pub artifacts: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecurityQualification {
    pub profile: SecurityProfile,
    pub outcome: QualificationStatus,
    pub evidence: Option<QualificationEvidence>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "snake_case")]
pub enum VmRuntimeStatus {
    #[default]
    Unknown,
    Running,
    Paused(String),
    Failed(String),
    Stopped,
}
