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

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd, git};

/// Where the Containerfile's builder stage looks for the host's kache store.
const KACHE_HOST_MOUNT: &str = "/kache-host";

/// The image every ilex build layers onto.
pub const BASE_IMAGE: &str = "quay.io/fedora/fedora-bootc:45";
/// The builder stage compiles the Rust binaries against the same glibc.
pub const BUILDER_IMAGE: &str = "quay.io/fedora/fedora:45";
/// Everything beyond fedora-bootc in a root image with no shell.
const BASE_BUDGET: u64 = 128 * 1024 * 1024;

#[derive(clap::Args, Debug, Default)]
pub struct Args {
    #[command(flatten)]
    pub selection: crate::select::Args,
    /// Tag (e.g. `candidate`) or full reference to build instead of this checkout's tag
    #[arg(long)]
    pub tag: Option<String>,
    /// Build only a machine layer on this existing image (tag or full reference).
    #[arg(long)]
    pub on: Option<String>,
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
    let resolved = args.selection.resolve(ws, args.on.as_deref(), args.tag.as_deref())?;
    build_resolved(ws, args, resolved)
}

/// `test` already resolved the selection while deciding whether to rebuild.
pub fn build_resolved(ws: &Workspace, args: &Args, resolved: crate::select::Resolved) -> Result<String> {
    cmd::require("podman")?;
    if resolved.machine != "generic" {
        let base = &resolved.base_image;
        if args.on.is_none() {
            build(ws, &Args {
                selection: crate::select::Args { shells: args.selection.shells.clone(), machine: Some("generic".into()) },
                tag: Some(base.clone()),
                no_pull: args.no_pull,
                no_host_kache: args.no_host_kache,
                ..Args::default()
            })?;
        }
        resolved.require_base_match()?;
        return ilex_devkit::machine::build(ws, base, &ws.root.join("machines").join(&resolved.machine), args.tag.as_deref(), &args.podman_args);
    }
    if args.on.is_some() { bail!("--on requires a machine other than generic"); }
    let image = args.tag.as_deref().map_or(resolved.image, |tag| ws.resolve_image(Some(tag)));
    ilex_devkit::workspace::check_tag_length(&image)?;
    // Let the compose stage report malformed manifests too. This loose read
    // supplies arguments and labels; Python remains the schema authority.
    let selection = resolved.selection;
    let bare_base = selection.as_ref().is_some_and(|s| s.shells.is_empty());

    if !args.no_pull {
        for base in [BASE_IMAGE, BUILDER_IMAGE] {
            cmd::run(Command::new("podman").args(["pull", "--quiet", base]).stdout(std::io::stderr()))?;
        }
    }
    let base_digest = image_digest(BASE_IMAGE)
        .with_context(|| format!("resolving the digest of {BASE_IMAGE}"))?;
    let base_size = if bare_base { Some(image_size(BASE_IMAGE)?) } else { None };
    let revision = git::revision(&ws.root)?;

    let mut build = Command::new("podman");
    build
        .current_dir(&ws.root)
        .args(["build", "--layers", "--file", "Containerfile", "--tag", &image])
        .args(["--label", &format!("ilex.base-digest={base_digest}")])
        .args(["--label", &format!("org.opencontainers.image.revision={revision}")])
        .args(["--label", "org.opencontainers.image.title=ilex"])
        .args(["--label", "org.opencontainers.image.source=https://github.com/Reidond/ilex"]);
    if let Some(selection) = selection {
        build.args([
            "--build-arg",
            &format!(
                "ILEX_SHELLS={}",
                if selection.shells.is_empty() {
                    "none".into()
                } else {
                    selection.shells.join(",")
                }
            ),
        ]);
        build.args([
            "--build-arg",
            &format!(
                "ILEX_DEFAULT_SHELL={}",
                selection.default_shell.as_deref().unwrap_or("")
            ),
        ]);
        for (key, value) in selection.labels() {
            build.args(["--label", &format!("{key}={value}")]);
        }
    }
    if let Some(store) = host_kache_store().filter(|_| !args.no_host_kache) {
        build.args(["--volume", &format!("{}:{KACHE_HOST_MOUNT}:ro", store.display())]);
    }
    build.args(&args.podman_args).arg(".");
    cmd::run(build.stdout(std::io::stderr()))?;
    if let Some(base_size) = base_size {
        let size = image_size(&image)?;
        let growth = size.saturating_sub(base_size);
        if growth > BASE_BUDGET {
            bail!(
                "base size budget exceeded: {image} is {size} bytes ({:.2} MiB), \
                 fedora-bootc is {base_size} bytes ({:.2} MiB); \
                 growth {:.2} MiB exceeds the 128 MiB budget. \
                 Move packages to a shell or machine, or justify a budget change in a commit",
                size as f64 / 1048576.0, base_size as f64 / 1048576.0, growth as f64 / 1048576.0
            );
        }
        eprintln!("base size: {:.2} MiB, fedora-bootc {:.2} MiB, growth {:.2} MiB (budget 128 MiB)",
            size as f64 / 1048576.0, base_size as f64 / 1048576.0, growth as f64 / 1048576.0);
    }
    eprintln!("built {image} (base {base_digest}, revision {revision})");
    Ok(image)
}

fn image_size(image: &str) -> Result<u64> {
    cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Size}}", image]))?
        .parse().with_context(|| format!("reading the size of {image}"))
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
pub fn image_exists(image: &str) -> bool {
    Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .is_ok_and(|s| s.success())
}
