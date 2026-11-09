//! `cargo xtask iso <image@digest>`: build the installer ISO for a published
//! digest and push it to `ghcr.io/<owner>/ilex-iso` (CI only).
//!
//! The ISO is built by `image-builder` from the installer container (a
//! fedora-bootc image with Anaconda in it) with the ilex image embedded as the
//! payload in the installer's container storage. The kickstart defaults make
//! Anaconda install that image (`--source-imgref containers-storage:…`) and
//! track `ghcr.io/<owner>/ilex:latest` afterwards (`--target-imgref`). The ISO
//! is pinned to the digest you pass: that is the image that gets embedded,
//! under a local tag derived from the digest.
//!
//! The installer container is two layers: `installer/base/` (Anaconda and the
//! initramfs, which take most of the build) is built once per content hash and
//! Fedora release and cached as `ghcr.io/<owner>/ilex-installer-base:<tag>`;
//! `installer/Containerfile` adds the kickstart for one payload on top.
//!
//! An ISO carrying a ~2 GB image can exceed GitHub's 2 GiB release asset
//! limit, so it is pushed to ghcr as an OCI artifact (`podman artifact`) as
//! `:latest` and `:YYYYMMDD`. Pull it with `podman artifact pull` or `oras
//! pull`. `--no-push` builds the ISO and stops, which works outside CI.
//!
//! `--due` builds nothing: it says whether a new ISO is worth building (and
//! writes `iso_due=` to `$GITHUB_OUTPUT`). Installed machines update
//! themselves to `:latest`, so the ISO may lag the image by up to a week.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd};
use sha2::{Digest, Sha256};

use crate::build::image_exists;
use crate::registry::{self, Auth};

/// Media type of the ISO layer in the OCI artifact.
const ISO_MEDIA_TYPE: &str = "application/x-iso9660-image";
/// What installed machines update from when nothing else is asked for; the
/// installer Containerfile's `TARGET_IMGREF` default matches.
pub const DEFAULT_TARGET: &str = "ghcr.io/reidond/ilex:latest";
/// Basename of the ISO that is built and pushed.
const ISO_NAME: &str = "ilex-installer";
/// Repo paths (relative to the checkout) that make one ISO differ from the
/// next apart from its payload.
const INSTALLER_INPUTS: &[&str] = &["installer", "crates/xtask/src/iso.rs"];
/// Annotations on the pushed ISO: what the installer was built from, and
/// which image it embeds. `--due` compares them with the current ones.
const FINGERPRINT_ANNOTATION: &str = "ilex.installer-fingerprint";
const PAYLOAD_ANNOTATION: &str = "ilex.payload-digest";
/// Repository name (under the owner) of the cached installer base.
const BASE_REPOSITORY: &str = "ilex-installer-base";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// ilex image to install, pinned by digest: `ghcr.io/<owner>/ilex@sha256:…`
    /// (not needed with `--due`)
    pub image: Option<String>,
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
    /// Don't build: say whether a new ISO is due. It is when no ISO is
    /// published, when `installer/` (or this module) changed since the
    /// published one, or when `:latest` has moved on from the image the
    /// published ISO embeds and that ISO is at least `--max-age-days` old.
    #[arg(long)]
    pub due: bool,
    /// With `--due`: how long an ISO may lag the published image
    #[arg(long, default_value_t = 7)]
    pub max_age_days: u64,
    /// With `--due`: repository of the published image (default
    /// `ghcr.io/<owner>/ilex`)
    #[arg(long)]
    pub image_repository: Option<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    if args.due {
        return due(ws, &args);
    }
    let Some(image) = args.image.as_deref() else {
        bail!(
            "give the image to install, pinned by digest (`ghcr.io/<owner>/ilex@sha256:…`), or --due"
        );
    };
    let Some((image_repo, digest)) = image.split_once('@') else {
        bail!("expected an image pinned by digest (`ghcr.io/<owner>/ilex@sha256:…`), got {image}");
    };
    if !registry::is_digest(digest) {
        bail!("{digest:?} is not a sha256 digest");
    }
    let payload = image;
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

    // The installer base is cached in the registry's namespace when we know
    // it (CI); only a run that is allowed to publish writes to the cache.
    let base_repository = registry::repository(None, BASE_REPOSITORY).ok();
    let base_cache = base_repository.as_deref().map(|repository| BaseCache {
        repository,
        auth: &auth,
        push: registry::is_ci() && std::env::var("ILEX_INSTALLER_BASE_PUSH").as_deref() == Ok("1"),
    });

    let iso = build(
        ws,
        &Build {
            source: payload,
            digest,
            target: &target,
            output_dir: args.output_dir.as_deref(),
            base_cache,
        },
    )?;

    let Some(iso_repo) = iso_repo else {
        println!("{}", iso.display());
        return Ok(());
    };
    push(&iso, &iso_repo, &auth, &installer_fingerprint(ws)?, digest)?;
    registry::github_output("iso", &format!("{iso_repo}:latest"))?;
    Ok(())
}

/// Where the prebuilt installer base is cached.
pub struct BaseCache<'a> {
    /// `ghcr.io/<owner>/ilex-installer-base`
    pub repository: &'a str,
    pub auth: &'a Option<Auth>,
    /// Push a base that had to be built.
    pub push: bool,
}

/// What to build an ISO from.
pub struct Build<'a> {
    /// A reference to the payload podman can tag: it must be in local
    /// container storage.
    pub source: &'a str,
    /// Its manifest digest (`sha256:…`); names the output directory and the
    /// payload's tag inside the ISO.
    pub digest: &'a str,
    /// The image the installed machine updates from (`--target-imgref`).
    pub target: &'a str,
    /// Where the ISO goes (default `<state dir>/iso/<digest prefix>`).
    pub output_dir: Option<&'a Path>,
    /// Where to look for (and put) a prebuilt installer base.
    pub base_cache: Option<BaseCache<'a>>,
}

/// Build the installer container and the ISO for `spec` and return the path
/// of the ISO. Nothing is pushed (but a freshly built installer base may be
/// cached, see [`BaseCache`]).
pub fn build(ws: &Workspace, spec: &Build) -> Result<PathBuf> {
    // The ISO embeds exactly this digest, but under a local tag: image-builder
    // copies the payload between container storages, which can't reproduce
    // the compressed layers a `…@sha256:` destination name would require
    // ("Destination specifies a digest").
    let hex = spec.digest.strip_prefix("sha256:").unwrap_or(spec.digest);
    let short = &hex[..12.min(hex.len())];
    let payload_ref = format!("localhost/ilex-payload:{short}");
    cmd::run(Command::new("podman").args(["tag", spec.source, &payload_ref]))?;

    // The installer base (Anaconda, the initramfs), then the kickstart for
    // this payload on top of it. The installer reports the Fedora release
    // the payload is built on.
    let version = product_version(spec.source).unwrap_or_else(|| "45".to_string());
    let base = installer_base(ws, &version, spec.base_cache.as_ref())?;
    let installer = format!("localhost/ilex-installer:{}", ws.slug);
    let context = ws.root.join("installer");
    cmd::run(
        Command::new("podman")
            // Host networking: in CI this runs podman inside a container,
            // where the nested bridge network has no DNS.
            .args(["build", "--network=host", "--file"])
            .arg(context.join("Containerfile"))
            .args(["--build-arg", &format!("BASE_IMAGE={base}")])
            .args(["--build-arg", &format!("PAYLOAD_REF={payload_ref}")])
            .args(["--build-arg", &format!("TARGET_IMGREF={}", spec.target)])
            .args(["--tag", &installer])
            .arg(&context),
    )?;

    // image-builder turns the installer container into the ISO.
    let out_dir = match spec.output_dir {
        Some(dir) => dir.to_path_buf(),
        None => ws.state_dir().join("iso").join(short),
    };
    if out_dir.exists() {
        fs::remove_dir_all(&out_dir).with_context(|| format!("clearing {}", out_dir.display()))?;
    }
    fs::create_dir_all(&out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    let out_dir = std::path::absolute(&out_dir)?;
    run_image_builder(ws, &installer, &payload_ref, &out_dir)?;
    // Only a tag; the image stays in storage for the next run.
    let _ = Command::new("podman").args(["rmi", &payload_ref]).output();

    let iso = find_iso(&out_dir)?;
    let size = fs::metadata(&iso)?.len();
    eprintln!(
        "built {} ({:.2} GiB)",
        iso.display(),
        size as f64 / (1u64 << 30) as f64
    );
    Ok(iso)
}

/// The installer base as a local image: already there, pulled from the cache
/// repository, or built from `installer/base/`. Its tag is the Fedora release
/// plus a hash of everything in `installer/base/`, so a change to either is a
/// miss and nothing stale is ever reused.
fn installer_base(ws: &Workspace, version: &str, cache: Option<&BaseCache>) -> Result<String> {
    let context = ws.root.join("installer").join("base");
    let mut hash = Sha256::new();
    hash_tree(&context, &context, &mut hash)?;
    let digest = crate::rechunk::hex(&hash.finalize());
    let tag = format!("{version}-{}", &digest[..12]);
    let local = format!("localhost/ilex-installer-base:{tag}");
    if image_exists(&local) {
        eprintln!("installer base {tag}: already in local storage");
        return Ok(local);
    }

    let remote = cache.map(|c| format!("{}:{tag}", c.repository));
    if let (Some(cache), Some(remote)) = (cache, &remote) {
        let mut pull = Command::new("podman");
        pull.args(["pull", "--quiet"]);
        if registry::is_local_registry(cache.repository) {
            pull.arg("--tls-verify=false");
        }
        pull.arg(remote);
        registry::with_auth(cache.auth, &mut pull);
        let pulled = registry::capture(&mut pull)?;
        if pulled.ok {
            cmd::run(Command::new("podman").args(["tag", remote, &local]))?;
            eprintln!("installer base {tag}: pulled from {}", cache.repository);
            return Ok(local);
        }
        eprintln!(
            "installer base {tag}: not cached ({}); building it",
            pulled.stderr.lines().next().unwrap_or("")
        );
    }

    cmd::run(
        Command::new("podman")
            .args(["build", "--network=host", "--file"])
            .arg(context.join("Containerfile"))
            .args(["--build-arg", &format!("PRODUCT_VERSION={version}")])
            .args(["--tag", &local])
            .arg(&context),
    )?;
    if let (Some(cache), Some(remote)) = (cache, &remote)
        && cache.push
    {
        let mut push = Command::new("podman");
        push.arg("push");
        if registry::is_local_registry(cache.repository) {
            push.arg("--tls-verify=false");
        }
        push.arg(&local).arg(format!("docker://{remote}"));
        registry::with_auth(cache.auth, &mut push);
        // A cache that can't be written is a slower next run, not a failure.
        if let Err(e) = cmd::run(&mut push) {
            eprintln!("xtask: warning: could not cache the installer base: {e:#}");
        }
    }
    Ok(local)
}

/// Hash the names and contents of everything below `path`, in sorted order.
fn hash_tree(root: &Path, path: &Path, hash: &mut Sha256) -> Result<()> {
    if path.is_dir() {
        let mut entries = fs::read_dir(path)
            .with_context(|| format!("reading {}", path.display()))?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            hash_tree(root, &entry, hash)?;
        }
    } else {
        let name = path.strip_prefix(root).unwrap_or(path);
        hash.update(name.to_string_lossy().replace('\\', "/").as_bytes());
        hash.update(b"\0");
        hash.update(fs::read(path).with_context(|| format!("reading {}", path.display()))?);
        hash.update(b"\0");
    }
    Ok(())
}

/// What the installer is built from, apart from its payload: a hash of
/// [`INSTALLER_INPUTS`].
fn installer_fingerprint(ws: &Workspace) -> Result<String> {
    let mut hash = Sha256::new();
    for input in INSTALLER_INPUTS {
        hash.update(input.as_bytes());
        hash_tree(&ws.root, &ws.root.join(input), &mut hash)?;
    }
    Ok(format!("sha256:{}", crate::rechunk::hex(&hash.finalize())))
}

/// `--due`: is a new ISO worth building? Prints the answer and the reason and
/// writes `iso_due=true|false`.
fn due(ws: &Workspace, args: &Args) -> Result<()> {
    let iso_repo = registry::repository(args.repository.as_deref(), "ilex-iso")?;
    let image_repo = registry::repository(args.image_repository.as_deref(), "ilex")?;
    let auth = Auth::login(&iso_repo)?;
    let (is_due, reason) = iso_due(ws, &iso_repo, &image_repo, args.max_age_days, &auth)?;
    eprintln!("installer ISO due: {is_due} ({reason})");
    registry::github_output("iso_due", &is_due.to_string())
}

fn iso_due(
    ws: &Workspace,
    iso_repo: &str,
    image_repo: &str,
    max_age_days: u64,
    auth: &Option<Auth>,
) -> Result<(bool, String)> {
    let skopeo = |repo: &str, args: &[&str]| -> Result<registry::Captured> {
        let mut c = Command::new("skopeo");
        c.arg("inspect").args(args);
        if registry::is_local_registry(repo) {
            c.arg("--tls-verify=false");
        }
        c.arg(format!("docker://{repo}:latest"));
        registry::with_auth(auth, &mut c);
        registry::capture(&mut c)
    };

    let published = skopeo(iso_repo, &["--raw"])?;
    if !published.ok {
        if registry::looks_missing(&published.stderr) {
            return Ok((true, "no ISO is published yet".into()));
        }
        bail!(
            "skopeo could not inspect {iso_repo}:latest: {}",
            published.stderr
        );
    }
    let manifest: serde_json::Value =
        serde_json::from_str(&published.stdout).context("parsing the ISO's manifest")?;
    let annotation = |key: &str| manifest["annotations"][key].as_str();

    let current = installer_fingerprint(ws)?;
    eprintln!(
        "installer fingerprint: {current} (published ISO: {})",
        annotation(FINGERPRINT_ANNOTATION).unwrap_or("none")
    );
    if annotation(FINGERPRINT_ANNOTATION) != Some(current.as_str()) {
        return Ok((true, "the installer changed since the published ISO".into()));
    }

    let latest = skopeo(image_repo, &["--format", "{{.Digest}}"])?;
    if !latest.ok {
        if registry::looks_missing(&latest.stderr) {
            return Ok((
                false,
                format!("no image is published at {image_repo}:latest"),
            ));
        }
        bail!(
            "skopeo could not inspect {image_repo}:latest: {}",
            latest.stderr
        );
    }
    if annotation(PAYLOAD_ANNOTATION) == Some(latest.stdout.as_str()) {
        return Ok((false, "the published ISO already embeds :latest".into()));
    }

    let Some(created) =
        annotation("org.opencontainers.image.created").and_then(registry::parse_rfc3339_utc)
    else {
        return Ok((true, "the published ISO has no creation date".into()));
    };
    let age_days = registry::now_secs().saturating_sub(created) / 86_400;
    Ok(if age_days >= max_age_days {
        (
            true,
            format!("the published ISO is {age_days} days old and :latest has moved on"),
        )
    } else {
        (
            false,
            format!(
                "the published ISO is {age_days} days old; :latest has moved on, the weekly rebuild waits"
            ),
        )
    })
}

/// The Fedora release a payload is built on: the first component of its
/// `org.opencontainers.image.version` label (`45.20261108.0` → `45`).
fn product_version(image: &str) -> Option<String> {
    let version = crate::rechunk::local_label(image, "org.opencontainers.image.version")
        .ok()
        .flatten()?;
    let major = version.split('.').next()?;
    major
        .chars()
        .all(|c| c.is_ascii_digit())
        .then(|| major.to_string())
}

/// The `generic-iso` image type (Anaconda inside the installer container,
/// kickstart from the container) needs image-builder 89; Fedora 45's stable
/// repository has 87, whose only installer type is the unattended,
/// disk-wiping `bootc-installer`.
const MIN_IMAGE_BUILDER: u32 = 89;
/// Where a too-old or missing host image-builder is replaced by one in a
/// container (Fedora 45 `updates-testing`).
const IMAGE_BUILDER_IMAGE: &str = "localhost/ilex-image-builder:latest";

/// Major version of the image-builder on `PATH`, if there is one.
fn host_image_builder() -> Option<u32> {
    let out = cmd::probe(Command::new("image-builder").arg("version"))?;
    out.lines()
        .find_map(|l| l.trim().strip_prefix("version:"))
        .and_then(|v| v.trim().split('.').next()?.parse().ok())
}

/// Run `image-builder build … generic-iso`: directly when the one on `PATH`
/// is new enough (CI), otherwise inside a privileged Fedora 45 container that
/// shares this host's container storage, so a dev machine with an older
/// image-builder (or none) works the same.
fn run_image_builder(
    ws: &Workspace,
    installer: &str,
    payload_ref: &str,
    out_dir: &Path,
) -> Result<()> {
    let build_args = |out: &str| -> Vec<String> {
        [
            "build",
            "--bootc-ref",
            installer,
            "--bootc-installer-payload-ref",
            payload_ref,
            // image-builder wants a default root filesystem for the installer
            // container; it isn't used for the machines Anaconda installs.
            "--bootc-default-fs",
            "ext4",
            "--output-name",
            ISO_NAME,
            "--output-dir",
            out,
            "generic-iso",
        ]
        .map(String::from)
        .to_vec()
    };

    if host_image_builder().is_some_and(|v| v >= MIN_IMAGE_BUILDER) {
        return cmd::run(
            Command::new("image-builder").args(build_args(&out_dir.to_string_lossy())),
        );
    }
    eprintln!(
        "xtask: image-builder {} on PATH; running version {MIN_IMAGE_BUILDER}+ in a container",
        host_image_builder().map_or("is not".to_string(), |v| format!("{v} is too old:")),
    );

    // The image-builder container.
    let context = ws.root.join("installer").join("image-builder");
    cmd::run(
        Command::new("podman")
            .args(["build", "--network=host", "--file"])
            .arg(context.join("Containerfile"))
            .args(["--tag", IMAGE_BUILDER_IMAGE])
            .arg(&context),
    )?;

    // It reads the installer and payload from this host's container storage
    // and keeps osbuild's cache next to it; neither may sit on the
    // container's own overlayfs (podman can't nest overlay on overlay).
    let graph_root =
        cmd::output(Command::new("podman").args(["info", "--format", "{{.Store.GraphRoot}}"]))?;
    let storage = Path::new(&graph_root)
        .parent()
        .unwrap_or(Path::new("/var/lib/containers"));
    let cache = Path::new("/var/cache/image-builder");
    fs::create_dir_all(cache).with_context(|| format!("creating {}", cache.display()))?;
    let mut run = Command::new("podman");
    run.args(["run", "--rm", "--privileged", "--network=host"])
        .args(["--security-opt", "label=disable"])
        .arg("--volume")
        .arg(format!("{0}:{0}", storage.display()))
        .arg("--volume")
        .arg(format!("{0}:{0}", cache.display()))
        .arg("--volume")
        .arg(format!("{}:/out", out_dir.display()))
        .args(["--env", "TMPDIR=/var/cache/image-builder/tmp"])
        .arg(IMAGE_BUILDER_IMAGE)
        .args([
            "sh",
            "-c",
            "mkdir -p \"$TMPDIR\" && exec image-builder \"$@\"",
            "image-builder",
        ])
        .args(build_args("/out"));
    cmd::run(&mut run)
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

/// Push the ISO as an OCI artifact tagged `:latest` and `:YYYYMMDD`, with the
/// annotations `--due` reads back.
fn push(
    iso: &Path,
    repository: &str,
    auth: &Option<Auth>,
    installer_fingerprint: &str,
    payload_digest: &str,
) -> Result<()> {
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
            .args([
                "--annotation",
                &format!("{FINGERPRINT_ANNOTATION}={installer_fingerprint}"),
            ])
            .args([
                "--annotation",
                &format!("{PAYLOAD_ANNOTATION}={payload_digest}"),
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
