//! `cargo xtask changed <ref>`: has the image's content changed since the
//! published `<ref>` (usually `ghcr.io/<owner>/ilex:latest`)?
//!
//! Compares the candidate's `ilex.fingerprint` label (written by `xtask
//! rechunk`) with the label on the published image. A published image that
//! doesn't exist yet counts as changed. Prints `changed=true|false` and
//! `exists=true|false` and writes both to `$GITHUB_OUTPUT` when it is set.
//! The nightly run stops here when nothing changed, so machines don't
//! download an update every night for nothing.

use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::Workspace;

use crate::rechunk;
use crate::registry::{self, Auth, FINGERPRINT_LABEL};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Published image to compare against, e.g. `ghcr.io/reidond/ilex:latest`
    pub reference: String,
    /// Candidate image: tag or reference (defaults to this checkout's image)
    #[arg(long)]
    pub image: Option<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let candidate_ref = ws.resolve_image(args.image.as_deref());
    let candidate = rechunk::local_label(&candidate_ref, FINGERPRINT_LABEL)?.with_context(|| {
        format!("{candidate_ref} has no {FINGERPRINT_LABEL} label; run `cargo xtask rechunk` on it first")
    })?;

    let repository = repo_of(&args.reference);
    let auth = Auth::login(repository)?;
    let published = published_fingerprint(&args.reference, repository, &auth)?;

    let (exists, changed) = match &published {
        Published::Missing => (false, true),
        Published::Unlabelled => (true, true),
        Published::Fingerprint(f) => (true, *f != candidate),
    };
    eprintln!("candidate fingerprint: {candidate}");
    match &published {
        Published::Missing => eprintln!("published fingerprint: (no image at {})", args.reference),
        Published::Unlabelled => {
            eprintln!("published fingerprint: (image has no {FINGERPRINT_LABEL} label)")
        }
        Published::Fingerprint(f) => eprintln!("published fingerprint: {f}"),
    }
    registry::github_output("changed", &changed.to_string())?;
    registry::github_output("exists", &exists.to_string())?;
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
        let err = out.stderr.to_ascii_lowercase();
        // A repository that was never pushed to answers "name unknown" or
        // 404; ghcr answers "denied"/"unauthorized" for a package that
        // doesn't exist (or isn't public yet). Publishing is idempotent and
        // fails loudly on a real permission problem, so treat all of these
        // as "nothing published".
        let missing = [
            "manifest unknown",
            "name unknown",
            "not found",
            "404",
            "denied",
            "unauthorized",
        ];
        if missing.iter().any(|m| err.contains(m)) {
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
        match config["config"]["Labels"][FINGERPRINT_LABEL].as_str() {
            Some(f) => Published::Fingerprint(f.to_string()),
            None => Published::Unlabelled,
        },
    )
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
