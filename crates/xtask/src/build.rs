//! `cargo xtask build`: podman build tagged with the checkout's slug.
//!
//! The base tag floats (`fedora-bootc:45`), so the nightly build picks up
//! base updates. The digest it resolved to is recorded as the
//! `ilex.base-digest` label, next to the git revision.
//!
//! The builder stage compiles through kache. When this machine has a kache
//! store of its own, the build mounts it read-only at [`KACHE_HOST_MOUNT`] so
//! crates compiled on the host (or by another build) are restored instead of
//! compiled again.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use ilex_devkit::{Workspace, cmd, git};

/// Where the Containerfile's builder stage looks for the host's kache store.
const KACHE_HOST_MOUNT: &str = "/kache-host";

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
    /// Don't mount the host's kache store into the build
    #[arg(long)]
    pub no_host_kache: bool,
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
        .args(["--label", "org.opencontainers.image.source=https://github.com/Reidond/ilex"]);
    if let Some(store) = host_kache_store().filter(|_| !args.no_host_kache) {
        build.args(["--volume", &format!("{}:{KACHE_HOST_MOUNT}:ro", store.display())]);
    }
    build.args(&args.podman_args).arg(".");
    cmd::run(&mut build)?;
    eprintln!("built {image} (base {base_digest}, revision {revision})");
    Ok(image)
}

/// This machine's kache store, if it has one a build can read: the directory
/// kache itself uses (`KACHE_CACHE_DIR`, else `$XDG_CACHE_HOME/kache`, else
/// `~/.cache/kache`), which counts once it holds an `index.db`.
fn host_kache_store() -> Option<PathBuf> {
    let dir = [
        std::env::var_os("KACHE_CACHE_DIR").map(PathBuf::from),
        std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("kache")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("kache")),
    ]
    .into_iter()
    .flatten()
    .find(|d| !d.as_os_str().is_empty())?;
    dir.join("index.db").is_file().then_some(dir)
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
