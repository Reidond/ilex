//! Resolving a tag of the ilex repository to the manifest the registry answers
//! for it right now: its digest, download size and version. The installer
//! pins the digest, so what the review page shows is what is installed. This
//! reads metadata only; the signature is enforced when the image is pulled.

use anyhow::{Context, Result};
use ilex_installer_model::plan::{ErrorCode, PlanError, PlannedImage};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::Duration;

use crate::exec;

const TIMEOUT: Duration = Duration::from_secs(90);

/// What `skopeo inspect --raw` printed, as a manifest.
pub struct Manifest {
    pub digest: String,
    pub download_bytes: u64,
    pub annotations_version: Option<String>,
}

fn size_of(manifest: &Value) -> u64 {
    manifest["config"]["size"].as_u64().unwrap_or(0)
        + manifest["layers"].as_array().map_or(0, |layers| {
            layers.iter().filter_map(|l| l["size"].as_u64()).sum()
        })
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

/// An image manifest or an image index from its raw bytes.
pub fn parse_manifest(raw: &[u8]) -> Result<Parsed> {
    let digest = format!("sha256:{:x}", Sha256::digest(raw));
    let value: Value = serde_json::from_slice(raw).context("reading the manifest")?;
    if let Some(children) = value["manifests"].as_array() {
        let amd64 = children
            .iter()
            .find(|m| m["platform"]["os"] == "linux" && m["platform"]["architecture"] == "amd64")
            .and_then(|m| m["digest"].as_str())
            .context("the image index has no linux/amd64 image")?;
        return Ok(Parsed::Index {
            digest,
            amd64: amd64.to_owned(),
        });
    }
    Ok(Parsed::Image(Manifest {
        digest,
        download_bytes: size_of(&value),
        annotations_version: value["annotations"]["org.opencontainers.image.version"]
            .as_str()
            .map(str::to_owned),
    }))
}

fn failure(reference: &str, stderr: &str) -> PlanError {
    let missing = ["manifest unknown", "name unknown", "not found"]
        .iter()
        .any(|needle| stderr.contains(needle));
    PlanError::new(
        if missing {
            ErrorCode::ImageNotFound
        } else {
            ErrorCode::Registry
        },
        Some("composition"),
        format!(
            "{reference}: {}",
            stderr
                .lines()
                .last()
                .unwrap_or("the registry did not answer")
        ),
    )
}

fn inspect(args: &[&str], reference: &str) -> Result<exec::Output, PlanError> {
    let output = exec::run("skopeo", args, TIMEOUT).map_err(|e| {
        PlanError::new(
            ErrorCode::Registry,
            Some("composition"),
            format!("{reference}: {e:#}"),
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
    // Hash exactly the bytes returned: a second lookup of the tag could move.
    let bad = |e: anyhow::Error| {
        PlanError::new(
            ErrorCode::Registry,
            Some("composition"),
            format!("{reference}: {e:#}"),
        )
    };
    let (pinned, manifest) = match parse_manifest(&raw.stdout).map_err(bad)? {
        Parsed::Image(manifest) => (manifest.digest.clone(), manifest),
        Parsed::Index { digest, amd64 } => {
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
            format!("{source}: {e}"),
        )
    })?;
    let label = |key: &str| config["config"]["Labels"][key].as_str().map(str::to_owned);
    Ok(PlannedImage {
        target: target.into(),
        source,
        digest: pinned,
        download_bytes: manifest.download_bytes,
        version: label("org.opencontainers.image.version").or(manifest.annotations_version),
        revision: label("org.opencontainers.image.revision"),
    })
}
