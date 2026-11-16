use crate::{Architecture, CheckoutIdentity, DiskFormat, Firmware, config::Selection};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildOperation {
    Image(ImageBuildOptions),
    Rechunk {
        image: String,
        output: Option<String>,
    },
    Disk {
        image: String,
        format: DiskFormat,
        size_bytes: u64,
    },
    Iso {
        testing_tag: Option<String>,
    },
    Tool {
        name: String,
        packages: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageBuildOptions {
    pub output: String,
    pub no_pull: bool,
    pub no_host_kache: bool,
    pub podman_args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceEntryKind {
    File { executable: bool },
    Symlink { target: String },
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceEntry {
    /// Slash-separated relative path. No absolute/parent components.
    pub path: String,
    pub kind: SourceEntryKind,
    pub sha256: Option<String>,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceManifest {
    pub schema: u32,
    pub revision: String,
    pub dirty: bool,
    pub digest: String,
    pub entries: Vec<SourceEntry>,
}
impl SourceManifest {
    pub fn validate(&self) -> Result<()> {
        crate::check_version(self.schema)?;
        anyhow::ensure!(
            self.digest == crate::digest(&serde_json::to_vec(&self.entries)?),
            "source manifest digest mismatch"
        );
        let mut previous: Option<&str> = None;
        for entry in &self.entries {
            anyhow::ensure!(
                !entry.path.contains('\\')
                    && entry
                        .path
                        .split('/')
                        .all(|p| !p.is_empty() && p != "." && p != ".."),
                "unsafe manifest path"
            );
            anyhow::ensure!(
                previous.is_none_or(|p| p < entry.path.as_str()),
                "manifest paths are duplicated or unsorted"
            );
            previous = Some(&entry.path);
            match entry.kind {
                SourceEntryKind::Deleted => anyhow::ensure!(
                    entry.sha256.is_none() && entry.bytes == 0,
                    "deleted entry carries content"
                ),
                _ => anyhow::ensure!(
                    entry
                        .sha256
                        .as_ref()
                        .is_some_and(|s| s.starts_with("sha256:")
                            && s.len() == 71
                            && s[7..].bytes().all(|b| b.is_ascii_hexdigit())),
                    "source entry has no valid SHA256"
                ),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildRequest {
    pub schema: u32,
    pub job: String,
    pub checkout: CheckoutIdentity,
    pub operation: BuildOperation,
    pub selection: Option<Selection>,
    pub profile: Option<String>,
    pub host_arch: Architecture,
    pub guest_arch: Architecture,
    pub source: SourceManifest,
    /// Explicit allowlist; never the host's complete process environment.
    pub environment: BTreeMap<String, String>,
    pub recipe_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageIdentity {
    /// A local tag is meaningful only within this executor's image store.
    pub executor_id: String,
    pub local_reference: String,
    pub immutable_id: String,
    pub registry_digest: Option<String>,
    pub architecture: Architecture,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactLocation {
    pub executor_id: String,
    pub path: PathBuf,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildResult {
    pub schema: u32,
    pub job: String,
    pub owner: String,
    pub source_digest: String,
    pub image: Option<ImageIdentity>,
    pub executor_id: String,
    pub versions: BTreeMap<String, String>,
    pub logs: Vec<ArtifactLocation>,
    pub artifacts: Vec<ArtifactLocation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskArtifact {
    pub schema: u32,
    pub architecture: Architecture,
    pub format: DiskFormat,
    pub virtual_bytes: u64,
    pub allocated_bytes: u64,
    pub sector_bytes: u32,
    pub image: ImageIdentity,
    pub install_recipe_hash: String,
    pub firmware: Firmware,
    pub provisioning_hash: String,
    pub location: ArtifactLocation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorProbe {
    pub executor_id: String,
    pub host_arch: Architecture,
    pub supported_guest_arches: Vec<Architecture>,
    pub versions: BTreeMap<String, String>,
}

pub trait BuildExecutor {
    fn probe(&self) -> Result<ExecutorProbe>;
    fn stage(&self, request: &BuildRequest) -> Result<String>;
    fn execute(&self, request: &BuildRequest) -> Result<BuildResult>;
    fn inspect(&self, job: &str) -> Result<BuildResult>;
    fn export(&self, image: &ImageIdentity, output: &Path) -> Result<ArtifactLocation>;
    fn fetch(&self, artifact: &ArtifactLocation, output: &Path) -> Result<()>;
    fn cancel(&self, job: &str) -> Result<()>;
}
