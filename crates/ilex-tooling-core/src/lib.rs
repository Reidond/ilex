//! Host-independent contracts. Backends own paths, transports and execution.

pub mod capability;
pub mod config;
pub mod identity;
pub mod installer;
pub mod qualification;
pub mod request;
pub mod snapshot;
pub mod state;
pub mod transfer;
pub mod vm;

pub use capability::*;
pub use identity::*;
pub use request::*;
pub use snapshot::*;
pub use state::*;
pub use transfer::*;
pub use vm::*;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Increment before changing persisted or remote contract semantics.
pub const SCHEMA_VERSION: u32 = 1;

pub fn check_version(version: u32) -> Result<()> {
    if version != SCHEMA_VERSION {
        bail!(
            "tooling schema version {version} is unsupported (expected {SCHEMA_VERSION}); no mutation performed"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    X86_64,
    Aarch64,
}

impl Architecture {
    pub fn host() -> Result<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::X86_64),
            "aarch64" => Ok(Self::Aarch64),
            other => bail!("unsupported host architecture {other}"),
        }
    }
}
