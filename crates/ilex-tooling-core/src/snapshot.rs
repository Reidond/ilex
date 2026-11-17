//! Source transport metadata. Contents are addressed independently of host paths.
use crate::{CheckoutIdentity, SourceManifest, check_version};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

pub const CHUNK_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub schema: u32,
    pub identity: CheckoutIdentity,
    pub source: SourceManifest,
    pub files: BTreeMap<String, Vec<String>>,
    /// Original HEAD commit and its tree only, imported with an explicit shallow
    /// boundary. No parent history, config, hooks, credentials or gitfile.
    pub git_pack: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotContext {
    pub schema: u32,
    pub root: PathBuf,
    pub identity: CheckoutIdentity,
    pub source: SourceManifest,
}

impl SnapshotContext {
    pub fn validate(&self) -> Result<()> {
        check_version(self.schema)?;
        self.source.validate()?;
        let derived = CheckoutIdentity::new(
            self.identity.repository_id.clone(),
            self.identity.checkout_id.clone(),
            "remote",
            false,
        );
        ensure!(
            derived.owner == self.identity.owner,
            "invalid original checkout owner"
        );
        ensure!(
            self.identity.slug == "main" && self.identity.is_main
                || !self.identity.is_main
                    && self
                        .identity
                        .slug
                        .ends_with(&crate::digest(self.identity.owner.as_bytes())[7..39]),
            "invalid original checkout namespace"
        );
        ensure!(
            self.identity.snapshot_digest.as_ref() == Some(&self.source.digest),
            "source identity mismatch"
        );
        let revision = format!(
            "{}{}",
            self.source.revision,
            if self.source.dirty { "-dirty" } else { "" }
        );
        ensure!(
            self.identity.source_revision == revision,
            "source revision mismatch"
        );
        Ok(())
    }
}
