//! Bind commands that use checkout resources to the sources Cargo compiled.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use ilex_devkit::Workspace;

/// Check even help requests, before clap can return without dispatch.
pub fn check_checkout() -> Result<()> {
    if let Some(root) = Workspace::checkout_root()? {
        check_source(&root)?;
    }
    Ok(())
}

pub(crate) fn workspace() -> Result<Workspace> {
    let root = Workspace::require_checkout_root()?;
    check_source(&root)?;
    Ok(Workspace::from_root(root))
}

fn check_source(root: &Path) -> Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask lives under the workspace's crates directory")
        .canonicalize()
        .context("resolving the checkout this command was built from")?;
    ensure!(
        root == source,
        "checkout mismatch: this command was built from {}, but the selected checkout is {}; run Cargo from that checkout",
        source.display(),
        root.display()
    );
    Ok(())
}
