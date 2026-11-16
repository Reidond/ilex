//! Versioned metadata shared by backends and future typed job transport.
use crate::{BuildRequest, BuildResult, CheckoutIdentity, VmCapabilities, VmHandle, check_version};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnedResource {
    pub schema: u32,
    pub owner: String,
    pub run: String,
    pub provider: String,
    /// Exact UUID or PID plus creation identity, never just a name prefix.
    pub opaque_id: String,
    pub created_unix_ms: u64,
    pub paths: Vec<PathBuf>,
}
impl OwnedResource {
    pub fn authorize(&self, identity: &CheckoutIdentity, run: &str, opaque_id: &str) -> Result<()> {
        check_version(self.schema)?;
        identity.authorize(&self.owner)?;
        ensure!(
            self.run == run && self.opaque_id == opaque_id,
            "resource run/process identity mismatch; refusing mutation"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmState {
    pub schema: u32,
    pub handle: VmHandle,
    pub resource: OwnedResource,
    pub capabilities: VmCapabilities,
    pub endpoints: BTreeMap<String, Endpoint>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum Endpoint {
    Unix { path: PathBuf },
    LoopbackTcp { port: u16 },
    NamedPipe { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobState {
    pub schema: u32,
    pub owner: String,
    pub job: String,
    pub request_digest: String,
    pub result: Option<BuildResult>,
}
/// A finite operation, not a remote host shell string. Stdout carries one
/// versioned reply; human diagnostics belong on stderr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteRequest {
    pub schema: u32,
    pub request_id: String,
    pub operation: RemoteOperation,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", content = "request", rename_all = "snake_case")]
pub enum RemoteOperation {
    Probe,
    Stage(Box<BuildRequest>),
    Execute(Box<BuildRequest>),
    Inspect { owner: String, job: String },
    Cancel { owner: String, job: String },
    Fetch { owner: String, artifact_id: String },
}
impl RemoteRequest {
    pub fn validate(&self) -> Result<()> {
        check_version(self.schema)?;
        ensure!(
            !self.request_id.is_empty() && self.request_id.len() <= 128,
            "invalid request ID"
        );
        if let RemoteOperation::Stage(request) | RemoteOperation::Execute(request) = &self.operation
        {
            check_version(request.schema)?;
            check_version(request.source.schema)?;
        }
        Ok(())
    }
}
