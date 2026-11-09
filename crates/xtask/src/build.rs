//! `cargo xtask build`: podman build tagged with the checkout's slug.
//!
//! The base tag floats (`fedora-bootc:45`), so the nightly build picks up
//! base updates. The digest it resolved to is recorded as the
//! `ilex.base-digest` label, next to the git revision.

use std::process::Command;

use anyhow::{Context, Result};
use ilex_devkit::{Workspace, cmd, git};

/// The image every ilex build layers onto.
pub const BASE_IMAGE: &str = "quay.io/fedora/fedora-bootc:45";
/// The builder stage compiles the Rust binaries against the same glibc.
pub const BUILDER_IMAGE: &str = "quay.io/fedora/fedora:45";

#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Tag (e.g. `candidate`) or full reference to build instead of this checkout's tag
    #[arg(long)]
    pub tag: Option<String>,
    /// Don't pull the base images first; build from local storage
    #[arg(long)]
    pub no_pull: bool,
    /// Extra arguments passed through to `podman build`
    #[arg(last = true)]
    pub podman_args: Vec<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let image = build(ws, &args)?;
    println!("{image}");
    Ok(())
}

/// Build the image and return its full reference.
pub fn build(ws: &Workspace, args: &Args) -> Result<String> {
    cmd::require("podman")?;
    let image = ws.resolve_image(args.tag.as_deref());

    if !args.no_pull {
        for base in [BASE_IMAGE, BUILDER_IMAGE] {
            cmd::run(Command::new("podman").args(["pull", "--quiet", base]))?;
        }
    }
    let base_digest = image_digest(BASE_IMAGE)
        .with_context(|| format!("resolving the digest of {BASE_IMAGE}"))?;
    let revision = git::revision(&ws.root)?;

    let mut build = Command::new("podman");
    build
        .current_dir(&ws.root)
        .args(["build", "--layers", "--file", "Containerfile", "--tag", &image])
        .args(["--label", &format!("ilex.base-digest={base_digest}")])
        .args(["--label", &format!("org.opencontainers.image.revision={revision}")])
        .args(["--label", "org.opencontainers.image.title=ilex"])
        .args(["--label", "org.opencontainers.image.source=https://github.com/Reidond/ilex"])
        .args(&args.podman_args)
        .arg(".");
    cmd::run(&mut build)?;
    eprintln!("built {image} (base {base_digest}, revision {revision})");
    Ok(image)
}

/// Digest of a local image, e.g. `sha256:…`.
pub fn image_digest(image: &str) -> Result<String> {
    cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Digest}}", image]))
}

/// True if `image` exists in local container storage.
#[allow(dead_code)]
pub fn image_exists(image: &str) -> bool {
    Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .is_ok_and(|s| s.success())
}
