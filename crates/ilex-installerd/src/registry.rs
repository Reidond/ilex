//! Resolving a tag of the ilex repository to the manifest the registry answers
//! for it right now: its digest, download size and version. The installer
//! pins the digest, so what the review page shows is what is installed. This
//! reads metadata only; the signature is enforced when the image is pulled, so
//! everything here is untrusted input: digests are checked for their shape
//! before they reach a command line, a child manifest must hash to the digest
//! that named it, sizes add up with checked arithmetic, and what is shown to a
//! person is cleaned of control characters.

use anyhow::{Context, Result, ensure};
use ilex_installer_model::plan::{ErrorCode, PlanError, PlannedImage};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::exec::{self, Limits};

const TIMEOUT: Duration = Duration::from_secs(90);

/// What `skopeo inspect --raw` printed, as a manifest.
pub struct Manifest {
    pub digest: String,
    pub download_bytes: u64,
    pub annotations_version: Option<String>,
}

/// `sha256:` and 64 lower-case hex digits: the only digest the installer
/// accepts from a registry, and the only shape that reaches skopeo.
pub fn is_digest(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// A descriptor's `size` and `digest`, both required and well formed.
fn descriptor(value: &Value, what: &str) -> Result<u64> {
    let size = value["size"]
        .as_u64()
        .with_context(|| format!("the {what} has no size"))?;
    ensure!(
        value["digest"].as_str().is_some_and(is_digest),
        "the {what} has no valid digest"
    );
    Ok(size)
}

/// The compressed download of an image manifest: its config and every layer.
/// An image manifest has both, and at least one layer.
fn size_of(manifest: &Value) -> Result<u64> {
    let mut total = descriptor(&manifest["config"], "config")?;
    let layers = manifest["layers"]
        .as_array()
        .context("the image manifest has no layers")?;
    ensure!(!layers.is_empty(), "the image manifest has no layers");
    for layer in layers {
        total = total
            .checked_add(descriptor(layer, "layer")?)
            .context("the layer sizes overflow")?;
    }
    Ok(total)
}

/// What a raw manifest is.
pub enum Parsed {
    Image(Manifest),
    /// An index: the digest it is pinned by, and its linux/amd64 image's.
    Index {
        digest: String,
        amd64: String,
    },
}

/// A label or annotation a person will read: bounded, with no control
/// characters (it came from a registry that has not been verified yet).
fn clean_label(text: &str) -> Option<String> {
    (!text.is_empty() && text.chars().count() <= 128 && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

/// An image manifest or an image index from its raw bytes.
pub fn parse_manifest(raw: &[u8]) -> Result<Parsed> {
    let digest = digest_of(raw);
    let value: Value = serde_json::from_slice(raw).context("reading the manifest")?;
    ensure!(
        value["schemaVersion"] == 2,
        "the manifest is not schema version 2"
    );
    if let Some(children) = value.get("manifests") {
        let children = children.as_array().context("the index has no manifests")?;
        let amd64 = children
            .iter()
            .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64")
            .context("the image index has no linux/amd64 image")?;
        let amd64 = amd64["digest"]
            .as_str()
            .filter(|d| is_digest(d))
            .context("the linux/amd64 image of the index has no valid digest")?;
        return Ok(Parsed::Index {
            digest,
            amd64: amd64.to_owned(),
        });
    }
    Ok(Parsed::Image(Manifest {
        digest,
        download_bytes: size_of(&value)?,
        annotations_version: value["annotations"]["org.opencontainers.image.version"]
            .as_str()
            .and_then(clean_label),
    }))
}

/// Only explicit registry absence is "not found": an unknown manifest or
/// repository, as the registry words it, in any case. Authentication failures,
/// generic 404s and transport errors are the registry not answering.
pub fn looks_missing(stderr: &str) -> bool {
    let err = stderr.to_ascii_lowercase();
    err.contains("manifest unknown") || err.contains("name unknown")
}

/// A line of a tool's output made safe to show: no control characters, bounded.
fn clean(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(200).collect()
}

/// The refusal for a failed registry call.
pub fn failure(reference: &str, stderr: &str) -> PlanError {
    PlanError::new(
        if looks_missing(stderr) {
            ErrorCode::ImageNotFound
        } else {
            ErrorCode::Registry
        },
        Some("composition"),
        format!(
            "{}: {}",
            clean(reference),
            clean(
                stderr
                    .lines()
                    .last()
                    .unwrap_or("the registry did not answer")
            )
        ),
    )
}

fn inspect(args: &[&str], reference: &str) -> Result<exec::Output, PlanError> {
    let output = exec::run_limited("skopeo", args, TIMEOUT, Limits::MANIFEST).map_err(|e| {
        PlanError::new(
            ErrorCode::Registry,
            Some("composition"),
            format!("{}: {}", clean(reference), clean(&format!("{e:#}"))),
        )
    })?;
    if output.success() {
        Ok(output)
    } else {
        Err(failure(reference, &output.stderr))
    }
}

/// Resolve `<repository>:<tag>`.
pub fn resolve(repository: &str, tag: &str, target: &str) -> Result<PlannedImage, PlanError> {
    let reference = format!("{repository}:{tag}");
    let raw = inspect(
        &[
            "inspect",
            "--raw",
            "--retry-times",
            "2",
            &format!("docker://{reference}"),
        ],
        &reference,
    )?;
    let bad = |e: anyhow::Error| {
        PlanError::new(
            ErrorCode::Registry,
            Some("composition"),
            format!("{}: {}", clean(&reference), clean(&format!("{e:#}"))),
        )
    };
    // Hash exactly the bytes returned: a second lookup of the tag could move.
    let (pinned, manifest) = match parse_manifest(&raw.stdout).map_err(bad)? {
        Parsed::Image(manifest) => (manifest.digest.clone(), manifest),
        Parsed::Index { digest, amd64 } => {
            // `amd64` has the shape of a digest (parse_manifest), so it is a
            // value, never an option, in the reference below.
            let child = inspect(
                &[
                    "inspect",
                    "--raw",
                    "--retry-times",
                    "2",
                    &format!("docker://{repository}@{amd64}"),
                ],
                &reference,
            )?;
            // Asked for by digest, so it must hash to it.
            if digest_of(&child.stdout) != amd64 {
                return Err(bad(anyhow::anyhow!(
                    "the registry returned a different manifest than the one asked for"
                )));
            }
            match parse_manifest(&child.stdout).map_err(bad)? {
                Parsed::Image(manifest) => (digest, manifest),
                Parsed::Index { .. } => {
                    return Err(bad(anyhow::anyhow!("an index of indexes")));
                }
            }
        }
    };
    let source = format!("{repository}@{pinned}");
    let config = inspect(
        &[
            "inspect",
            "--config",
            "--retry-times",
            "2",
            &format!("docker://{repository}@{}", manifest.digest),
        ],
        &source,
    )?;
    let config: Value = serde_json::from_slice(&config.stdout).map_err(|e| {
        PlanError::new(
            ErrorCode::Registry,
            Some("composition"),
            format!("{}: {}", clean(&source), clean(&e.to_string())),
        )
    })?;
    let label = |key: &str| {
        config["config"]["Labels"][key]
            .as_str()
            .and_then(clean_label)
    };
    Ok(PlannedImage {
        target: target.into(),
        source,
        digest: pinned,
        download_bytes: manifest.download_bytes,
        version: label("org.opencontainers.image.version").or(manifest.annotations_version),
        revision: label("org.opencontainers.image.revision"),
    })
}
