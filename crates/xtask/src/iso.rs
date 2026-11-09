//! `cargo xtask iso <image@digest>`: build the installer ISO for a published
//! digest and push it to `ghcr.io/<owner>/ilex-iso` (CI only).
//!
//! The ISO is built by `image-builder` from `installer/Containerfile` (a
//! fedora-bootc image with Anaconda in it) with the ilex image embedded as the
//! payload in the installer's container storage. The kickstart defaults make
//! Anaconda install that image (`--source-imgref containers-storage:…`) and
//! track `ghcr.io/<owner>/ilex:latest` afterwards (`--target-imgref`). The ISO
//! is pinned to the digest you pass: that is the image that gets embedded,
//! under a local tag derived from the digest.
//!
//! An ISO carrying a ~2 GB image can exceed GitHub's 2 GiB release asset
//! limit, so it is pushed to ghcr as an OCI artifact (`podman artifact`) as
//! `:latest` and `:YYYYMMDD`. Pull it with `podman artifact pull` or `oras
//! pull`. `--no-push` builds the ISO and stops, which works outside CI.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd};

use crate::build::image_exists;
use crate::registry::{self, Auth};

/// Media type of the ISO layer in the OCI artifact.
const ISO_MEDIA_TYPE: &str = "application/x-iso9660-image";
/// Basename of the ISO that is built and pushed.
const ISO_NAME: &str = "ilex-installer";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// ilex image to install, pinned by digest: `ghcr.io/<owner>/ilex@sha256:…`
    pub image: String,
    /// Build the ISO but don't push it (allowed outside CI)
    #[arg(long)]
    pub no_push: bool,
    /// Directory the ISO is written to (default: `<state dir>/iso`)
    #[arg(long)]
    pub output_dir: Option<PathBuf>,
    /// Registry repository for the ISO artifact (default `ghcr.io/<owner>/ilex-iso`)
    #[arg(long)]
    pub repository: Option<String>,
    /// Image the installed machine updates from (default: the image's
    /// repository with `:latest`)
    #[arg(long)]
    pub target_imgref: Option<String>,
    /// Outside CI, allow pushing to a registry on this machine
    #[arg(long)]
    pub allow_local_registry: bool,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let Some((image_repo, digest)) = args.image.split_once('@') else {
        bail!(
            "expected an image pinned by digest (`ghcr.io/<owner>/ilex@sha256:…`), got {}",
            args.image
        );
    };
    if !registry::is_digest(digest) {
        bail!("{digest:?} is not a sha256 digest");
    }
    let payload = args.image.as_str();
    let target = args
        .target_imgref
        .clone()
        .unwrap_or_else(|| format!("{image_repo}:latest"));

    let iso_repo = if args.no_push {
        None
    } else {
        let repo = registry::repository(args.repository.as_deref(), "ilex-iso")?;
        registry::require_ci("xtask iso", &repo, args.allow_local_registry)?;
        Some(repo)
    };
    cmd::require("podman")?;
    cmd::require("image-builder")?;
    let auth = Auth::login(iso_repo.as_deref().unwrap_or(image_repo))?;

    // The payload has to be in the local container storage: image-builder
    // copies it from there into the installer's own storage.
    if !image_exists(payload) {
        let mut pull = Command::new("podman");
        pull.args(["pull", "--quiet"]);
        if registry::is_local_registry(image_repo) {
            pull.arg("--tls-verify=false");
        }
        pull.arg(payload);
        registry::with_auth(&auth, &mut pull);
        cmd::run(&mut pull)?;
    }

    // The ISO embeds exactly this digest, but under a local tag: image-builder
    // copies the payload between container storages, which can't reproduce
    // the compressed layers a `…@sha256:` destination name would require
    // ("Destination specifies a digest").
    let hex = digest.strip_prefix("sha256:").unwrap_or(digest);
    let payload_ref = format!("localhost/ilex-payload:{}", &hex[..12.min(hex.len())]);
    cmd::run(Command::new("podman").args(["tag", payload, &payload_ref]))?;

    // The installer container, with the kickstart pointing at the payload.
    let installer = format!("localhost/ilex-installer:{}", ws.slug);
    let context = ws.root.join("installer");
    cmd::run(
        Command::new("podman")
            // Host networking: in CI this runs podman inside a container,
            // where the nested bridge network has no DNS.
            .args(["build", "--network=host", "--file"])
            .arg(context.join("Containerfile"))
            .args(["--build-arg", &format!("PAYLOAD_REF={payload_ref}")])
            .args(["--build-arg", &format!("TARGET_IMGREF={target}")])
            .args(["--tag", &installer])
            .arg(&context),
    )?;

    // image-builder turns the installer container into the ISO.
    let out_dir = args
        .output_dir
        .unwrap_or_else(|| ws.state_dir().join("iso"));
    if out_dir.exists() {
        fs::remove_dir_all(&out_dir).with_context(|| format!("clearing {}", out_dir.display()))?;
    }
    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    cmd::run(
        Command::new("image-builder")
            .arg("build")
            .args(["--bootc-ref", &installer])
            .args(["--bootc-installer-payload-ref", &payload_ref])
            // image-builder wants a default root filesystem for the installer
            // container; it isn't used for the machines Anaconda installs.
            .args(["--bootc-default-fs", "ext4"])
            .args(["--output-name", ISO_NAME])
            .arg("--output-dir")
            .arg(&out_dir)
            // `generic-iso` needs image-builder >= 89 (Fedora 45 updates-testing
            // at the time of writing). 87's `bootc-installer` is a different
            // design: it builds Anaconda itself and ignores the container's
            // kickstart, installing unattended and wiping all disks.
            .arg("generic-iso"),
    )?;
    // Only a tag; the image stays in storage for the next run.
    let _ = Command::new("podman").args(["rmi", &payload_ref]).output();
    let iso = find_iso(&out_dir)?;
    let size = fs::metadata(&iso)?.len();
    eprintln!(
        "built {} ({:.2} GiB)",
        iso.display(),
        size as f64 / (1u64 << 30) as f64
    );

    let Some(iso_repo) = iso_repo else {
        println!("{}", iso.display());
        return Ok(());
    };
    push(&iso, &iso_repo, &auth)?;
    registry::github_output("iso", &format!("{iso_repo}:latest"))?;
    Ok(())
}

/// The `.iso` image-builder wrote somewhere below `dir`.
fn find_iso(dir: &Path) -> Result<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "iso") {
                return Ok(path);
            }
        }
    }
    bail!("image-builder produced no .iso under {}", dir.display())
}

/// Push the ISO as an OCI artifact tagged `:latest` and `:YYYYMMDD`.
fn push(iso: &Path, repository: &str, auth: &Option<Auth>) -> Result<()> {
    let insecure = registry::is_local_registry(repository);
    let date = registry::utc_date();
    let latest = format!("{repository}:latest");

    // `--replace` so a leftover artifact from an earlier run can't be appended to.
    cmd::run(
        Command::new("podman")
            .args([
                "artifact",
                "add",
                "--replace",
                "--file-type",
                ISO_MEDIA_TYPE,
            ])
            .args([
                "--annotation",
                "org.opencontainers.image.source=https://github.com/Reidond/ilex",
            ])
            .arg(&latest)
            .arg(iso),
    )?;
    let mut push = Command::new("podman");
    push.args(["artifact", "push"]);
    if insecure {
        push.arg("--tls-verify=false");
    }
    push.arg(&latest);
    registry::with_auth(auth, &mut push);
    let pushed = cmd::run(&mut push);
    // The local copy is another ~2 GB; drop it whether or not the push worked.
    let _ = Command::new("podman")
        .args(["artifact", "rm", &latest])
        .status();
    pushed?;

    // The dated tag points at the same manifest: copy it registry-side.
    let mut copy = Command::new("skopeo");
    copy.args(["copy", "--preserve-digests"]);
    if insecure {
        copy.args(["--src-tls-verify=false", "--dest-tls-verify=false"]);
    }
    copy.arg(format!("docker://{latest}"))
        .arg(format!("docker://{repository}:{date}"));
    registry::with_auth(auth, &mut copy);
    cmd::run(&mut copy)?;
    eprintln!("pushed {latest} and {repository}:{date}");
    Ok(())
}
