use std::collections::BTreeMap;

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
    pub screenshot_source: Option<String>,
    pub screenshot_dimensions: Option<(u32, u32)>,
    pub text_layout: Option<String>,
}

impl VmCapabilities {
    pub fn require(&self, feature: Capability) -> Result<()> {
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
