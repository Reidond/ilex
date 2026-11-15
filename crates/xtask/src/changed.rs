//! `cargo xtask changed <ref>`: has the image's content changed since the
//! published `<ref>` (usually `ghcr.io/<owner>/ilex:latest`)?
//!
//! Compares the candidate's fingerprint with the `ilex.fingerprint` label on
//! the published image. The candidate can be the image as built (the
//! fingerprint is computed from it) or a rechunked one (its label is read);
//! both are the same value. A published image that doesn't exist yet counts
//! as changed. Prints `changed=true|false` and `exists=true|false` and writes
//! both to `$GITHUB_OUTPUT` when it is set. The nightly run stops here when
//! nothing changed, so machines don't download an update every night for
//! nothing, and without paying for the rechunk.

use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, bcvk, config, machine};

use crate::rechunk;
use crate::registry::{self, Auth, FINGERPRINT_LABEL};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Published image to compare against, e.g. `ghcr.io/reidond/ilex:latest`
    pub reference: String,
    /// Candidate image: tag or reference (defaults to this checkout's image)
    #[arg(long)]
    pub image: Option<String>,
    /// Compare this machine (validated against machines/<name>/machine.toml)
    #[arg(long)]
    pub machine: Option<String>,
    /// Compare only machine inputs; no local image or base is needed
    #[arg(long, requires = "machine", conflicts_with = "image")]
    pub inputs_only: bool,
    /// Also compare the published machine's base identity with this local
    /// generic base; prints base_changed= (recovers a missed machine publish)
    #[arg(long, requires = "inputs_only", conflicts_with = "base_id")]
    pub base: Option<String>,
    /// Registry manifest's config digest (image ID), without pulling the base
    #[arg(long, requires = "inputs_only")]
    pub base_id: Option<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    if let Some(name) = &args.machine {
        config::machine_manifest(&ws.root, name)?;
        if name == "generic" {
            bail!("generic uses the base image's change detection, without --machine");
        }
    }
    let label = if args.inputs_only { "ilex.machine-inputs" } else { FINGERPRINT_LABEL };
    let candidate_ref = ws.resolve_image(args.image.as_deref());
    // The image as built has no label yet; its fingerprint is computed from it
    // (the same value `xtask rechunk` will write).
    let candidate = if args.inputs_only {
        machine::inputs(ws, &ws.root.join("machines").join(args.machine.as_ref().unwrap()), &[])?
    } else {
        if let Some(name) = &args.machine
            && machine::image_machine(&candidate_ref)? != *name {
            bail!("{candidate_ref} is not machine {name}");
        }
        rechunk::candidate_fingerprint(ws, &candidate_ref)?
    };

    let repository = repo_of(&args.reference);
    let auth = Auth::login(repository)?;
    let strict = args.machine.is_some();
    let published = published_fingerprint(&args.reference, repository, &auth, label, strict)?;
    let base_id = if let Some(base) = &args.base {
        if machine::image_machine(base)? != "generic" {
            bail!("machine base must be generic: {base}");
        }
        Some(bcvk::image_id(base)?)
    } else if let Some(id) = &args.base_id {
        if !registry::is_digest(id) {
            bail!("--base-id must be a sha256 config digest, got {id:?}");
        }
        Some(id.strip_prefix("sha256:").unwrap().to_string())
    } else {
        None
    };
    if let Some(id) = base_id {
        let prefix = format!("{id}-");
        let key = published_fingerprint(&args.reference, repository, &auth, "ilex.machine-key", strict)?;
        let same_base = matches!(key, Published::Fingerprint(key) if key.starts_with(&prefix));
        registry::github_output("base_changed", &(!same_base).to_string())?;
    }

    let (exists, changed) = match &published {
        Published::Missing => (false, true),
        Published::Unlabelled => (true, true),
        Published::Fingerprint(f) => (true, *f != candidate),
    };
    eprintln!("candidate fingerprint: {candidate}");
    match &published {
        Published::Missing => eprintln!("published fingerprint: (no image at {})", args.reference),
        Published::Unlabelled => {
            eprintln!("published fingerprint: (image has no {label} label)")
        }
        Published::Fingerprint(f) => eprintln!("published fingerprint: {f}"),
    }
    registry::github_output(if args.inputs_only { "inputs_changed" } else { "changed" }, &changed.to_string())?;
    registry::github_output("exists", &exists.to_string())?;
    // `xtask rechunk --fingerprint` writes exactly this value into the label,
    // however the working tree looks by then.
    registry::github_output("fingerprint", &candidate)?;
    Ok(())
}

enum Published {
    /// Nothing there yet: the first publish.
    Missing,
    /// An image without the label, e.g. one that predates fingerprints.
    Unlabelled,
    Fingerprint(String),
}

/// `ghcr.io/o/ilex:latest` → `ghcr.io/o/ilex`
fn repo_of(reference: &str) -> &str {
    let name = reference.split('@').next().unwrap_or(reference);
    match name.rsplit_once(':') {
        // A `:` before the last `/` is a registry port, not a tag.
        Some((repo, tag)) if !tag.contains('/') => repo,
        _ => name,
    }
}

fn published_fingerprint(
    reference: &str,
    repository: &str,
    auth: &Option<Auth>,
    label: &str,
    strict: bool,
) -> Result<Published> {
    let mut skopeo = Command::new("skopeo");
    skopeo.args(["inspect", "--config"]);
    if registry::is_local_registry(repository) {
        skopeo.arg("--tls-verify=false");
    }
    skopeo.arg(format!("docker://{reference}"));
    registry::with_auth(auth, &mut skopeo);
    let out = registry::capture(&mut skopeo)?;

    if !out.ok {
        // Machines must not rebuild/publish on every private-package lookup
        // failure. Only explicit manifest/repository absence is a first publish.
        // Preserve the existing generic bootstrap behaviour separately.
        let error = out.stderr.to_ascii_lowercase();
        let missing = if strict {
            !error.contains("denied") && !error.contains("unauthorized")
                && (error.contains("manifest unknown") || error.contains("name unknown"))
        } else {
            registry::looks_missing(&out.stderr)
        };
        if missing {
            eprintln!(
                "xtask: {reference} not found ({}); treating as unpublished",
                first_line(&out.stderr)
            );
            return Ok(Published::Missing);
        }
        bail!("skopeo could not inspect {reference}: {}", out.stderr);
    }

    let config: serde_json::Value =
        serde_json::from_str(&out.stdout).context("parsing `skopeo inspect --config` output")?;
    Ok(
        match config["config"]["Labels"][label].as_str() {
            Some(f) => Published::Fingerprint(f.to_string()),
            None => Published::Unlabelled,
        },
    )
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
