//! Helpers shared by every tier.

#![allow(dead_code)]

use anyhow::Result;
use ilex_devkit::Workspace;

/// The checkout under test.
pub fn workspace() -> Result<Workspace> {
    Workspace::discover()
}

/// Image under test: `ILEX_IMAGE`, or this checkout's local image.
pub fn image() -> Result<String> {
    match std::env::var("ILEX_IMAGE") {
        Ok(image) if !image.is_empty() => Ok(image),
        _ => Ok(workspace()?.image_ref()),
    }
}
