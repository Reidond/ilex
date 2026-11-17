//! Shared fingerprint/cache key inputs. Preserve phase 0's publication rules.
use crate::{Workspace, cmd, config::Selection, git};
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::process::Command;
/// Repo paths whose content ends up in the image. `Cargo.toml`/`Cargo.lock`
/// pin the dependencies of the shipped binaries.
const INPUTS: &[&str] = &[
    "users",
    "rootfs",
    "crates",
    "Containerfile",
    "packages.toml",
    "Cargo.toml",
    "Cargo.lock",
    "build",
    "machines/generic",
];

/// Early root cache key: selected source trees plus both freshly pulled build
/// images. The full RPM/content fingerprint still decides publication after a
/// cache miss. Missing keys on older publications deliberately force a build.
pub fn root_inputs(ws: &Workspace, selection: &Selection) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"ilex-root-inputs v1\n");
    hash_selection(&mut hash, selection);
    hash_users(ws, &mut hash)?;
    for image in [crate::image::BASE_IMAGE, crate::image::BUILDER_IMAGE] {
        let digest = cmd::output(Command::new("podman").args([
            "image",
            "inspect",
            "--format",
            "{{.Digest}}",
            image,
        ]))?;
        if !(digest.starts_with("sha256:")
            && digest.len() == 71
            && digest[7..].bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!("invalid upstream digest for {image}: {digest}");
        }
        hash.update(format!("upstream {image} {digest}\n"));
    }
    hash_inputs(ws, &mut hash, selection_inputs(selection))?;
    Ok(format!("sha256:{}", hex(&hash.finalize())))
}

pub fn selection_inputs(selection: &Selection) -> Vec<String> {
    let mut inputs: Vec<String> = INPUTS.iter().map(|s| s.to_string()).collect();
    if !selection.shells.is_empty() {
        inputs.push("shells/packages.toml".into());
    }
    if selection.machine != "generic" {
        inputs.extend([
            format!("machines/{}", selection.machine),
            "machines/Containerfile".into(),
        ]);
    }
    inputs.extend(selection.shells.iter().map(|name| format!("shells/{name}")));
    inputs
}

pub fn hash_selection(hash: &mut Sha256, selection: &Selection) {
    for (key, value) in selection.labels() {
        hash.update(format!("selection {key}={value}\n"));
    }
}

// User selection affects the public catalogue, never the three OCI selection
// labels or publication names. Hash it separately so changing only the list
// cannot reuse a root with a different catalogue.
pub fn hash_users(ws: &Workspace, hash: &mut Sha256) -> Result<()> {
    let manifest = crate::config::manifest(&ws.root.join("image.toml"))?;
    let users = manifest
        .get("users")
        .map(ToString::to_string)
        .unwrap_or_else(|| "[]".into());
    hash.update(format!("users {users}\n"));
    Ok(())
}

pub fn hash_inputs(ws: &Workspace, hash: &mut Sha256, inputs: Vec<String>) -> Result<()> {
    let source = crate::source::manifest(ws)?;
    // Preserve the published clean-tree encoding in both executors. The shallow
    // snapshot includes the original HEAD tree, so its Git object IDs are equal.
    for path in &inputs {
        let tree = git::tree_hash(&ws.root, path).with_context(|| format!("hashing {path}"))?;
        eprintln!("  {path}: {tree}");
        hash.update(format!("tree {path} {tree}\n"));
    }
    if source.dirty {
        hash.update(b"dirty\n");
        hash.update(b"working-inputs v1\n");
        // The same canonical manifest encoding covers dirty working bytes,
        // modes, links and deletions on native Linux and in a remote snapshot.
        for entry in &source.entries {
            if inputs
                .iter()
                .any(|p| entry.path == *p || entry.path.starts_with(&format!("{p}/")))
            {
                hash.update(serde_json::to_vec(entry)?);
                hash.update(b"\n");
            }
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
