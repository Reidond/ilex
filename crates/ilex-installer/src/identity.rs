//! The edition this live image installs: written into the image by
//! `ilexctl iso --next` from `installer/next/editions.toml` (the only place its
//! display name lives). The interface reads it for the serial markers and the
//! self-check; what the person sees comes from the helper's probe.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where the image build puts the edition.
pub const EDITION_FILE: &str = "/usr/share/ilex-installer/edition.json";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edition {
    pub schema: u32,
    /// Stable identifier (`gnome`): names the image stage and its files.
    pub edition: String,
    /// What a person reads. It lives in `installer/next/editions.toml` only.
    #[serde(rename = "display-name")]
    pub display_name: String,
}

pub fn read() -> Result<Edition> {
    read_from(Path::new(EDITION_FILE))
}

fn read_from(path: &Path) -> Result<Edition> {
    let edition: Edition = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    if edition.schema != 1 || edition.edition.is_empty() || edition.display_name.is_empty() {
        bail!("{} is not a schema 1 edition record", path.display());
    }
    Ok(edition)
}
