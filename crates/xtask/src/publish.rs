//! `cargo xtask publish`: push by digest, tag and sign (CI only).
//!
//! 1. Push the rechunked image once as `:sha-<short>` and read back the
//!    digest the registry stored. In CI the image is already there: the build
//!    job pushed it as `:candidate` (`--stage`) so the upgrade tier could pull
//!    it on its own runner, and the publish job hands that digest over
//!    (`--digest`), so what is signed is exactly what was tested.
//! 2. Sign that digest with the cosign key pair (`COSIGN_PRIVATE_KEY` +
//!    `COSIGN_PASSWORD`), in the legacy `sha256-<digest>.sig` tag format that
//!    bootc / containers-image verify, and check the signature against the
//!    public key shipped in the image.
//! 3. Only then point `:latest` and `:YYYYMMDD` at the digest, so a machine
//!    never sees a moving tag whose image has no signature yet.
//!
//! Signing is key based on purpose: containers-image can't verify keyless
//! signatures at pull time. cosign must be v2 (v3 defaults to a different
//! signature format).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, bcvk, cmd, config, git, machine, workspace::check_tag_length};

use crate::rechunk;
use crate::registry::{self, Auth, FINGERPRINT_LABEL};

/// The public key baked into the image; the signature must verify against it.
const PUBKEY: &str = "rootfs/etc/pki/containers/ilex.pub";

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Image to publish: tag or reference (defaults to this checkout's image)
    pub image: Option<String>,
    /// Registry repository to publish to (default `ghcr.io/<owner>/ilex`)
    #[arg(long)]
    pub repository: Option<String>,
    /// Outside CI, allow publishing to a registry on this machine
    /// (`--repository localhost:5000/ilex`). Never reaches ghcr.
    #[arg(long)]
    pub allow_local_registry: bool,
    /// Only push the image as `:candidate`, unsigned, for tests on other
    /// runners, and print its `digest=`. Nothing is signed and `:latest`
    /// doesn't move; machines only accept signed images.
    #[arg(long, conflicts_with = "digest")]
    pub stage: bool,
    /// Publish the image already in the repository at this digest (what
    /// `--stage` pushed) instead of pushing a local one
    #[arg(long, value_name = "sha256:…")]
    pub digest: Option<String>,
    /// Publish a machine as :<name>, :<name>-YYYYMMDD and :<name>-sha-<short>
    #[arg(long, requires = "tested_image_id", conflicts_with_all = ["stage", "digest"])]
    pub machine: Option<String>,
    /// Immutable ID recorded before this machine's capability and VM tests
    #[arg(long, requires = "machine")]
    pub tested_image_id: Option<String>,
    /// Throwaway public key for local-registry verification only
    #[arg(long, requires = "allow_local_registry")]
    pub verification_key: Option<PathBuf>,
}

/// The unsigned tag `--stage` pushes.
const STAGE_TAG: &str = "candidate";

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let repository = registry::repository(args.repository.as_deref(), "ilex")?;
    registry::require_ci("xtask publish", &repository, args.allow_local_registry)?;
    if args.verification_key.is_some() && !registry::is_local_registry(&repository) {
        bail!("--verification-key is only allowed with a registry on this machine");
    }
    if let Some(name) = &args.machine {
        config::machine_manifest(&ws.root, name)?;
        if name == "generic" {
            bail!("generic is published as :latest, without --machine");
        }
    }
    cmd::require("podman")?;
    cmd::require("skopeo")?;
    if args.stage {
        return stage(ws, &args, &repository);
    }
    cmd::require("cosign")?;
    for var in ["COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD"] {
        if std::env::var_os(var).is_none_or(|v| v.is_empty()) {
            bail!("{var} is not set (repository secret in CI)");
        }
    }

    let short = git::short_sha(&ws.root)?;
    let date = registry::utc_date();
    let insecure = registry::is_local_registry(&repository);
    let auth = Auth::login(&repository)?;

    // 1. The digest everything else uses, tagged `sha-<short>`: pushed now,
    //    or already in the repository (`--digest`, staged by the build job).
    let (moving_tag, date_tag, sha_tag) = match &args.machine {
        Some(name) => (name.clone(), format!("{name}-{date}"), format!("{name}-sha-{short}")),
        None => ("latest".into(), date.clone(), format!("sha-{short}")),
    };
    for tag in [&moving_tag, &date_tag, &sha_tag] {
        check_tag_length(&format!("{repository}:{tag}"))?;
    }
    let (digest, fingerprint) = match &args.digest {
        Some(digest) => {
            if !registry::is_digest(digest) {
                bail!("--digest {digest:?} is not a sha256 digest");
            }
            let pinned = format!("{repository}@{digest}");
            let config = skopeo_json(&["inspect", "--config"], &pinned, insecure, &auth)?
                .with_context(|| format!("{pinned} is not in the registry"))?;
            if config["config"]["Labels"]["ilex.machine"].as_str() != Some("generic") {
                bail!("{pinned} is not a generic base image; refusing to move :latest");
            }
            let fingerprint = config["config"]["Labels"][FINGERPRINT_LABEL]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("{pinned} has no {FINGERPRINT_LABEL} label"))?;
            copy(&pinned, &format!("{repository}:{sha_tag}"), insecure, &auth)?;
            (digest.clone(), fingerprint)
        }
        None => {
            let image = ws.resolve_image(args.image.as_deref());
            require_machine(&image, args.machine.as_deref().unwrap_or("generic"))?;
            let (source, fingerprint) = if let Some(tested) = &args.tested_image_id {
                let expected = tested.strip_prefix("sha256:").unwrap_or(tested);
                if !registry::is_digest(&format!("sha256:{expected}")) {
                    bail!("--tested-image-id {tested:?} is not a sha256 image ID");
                }
                let actual = bcvk::image_id(&image)?;
                if actual != expected {
                    bail!("{image} is image {actual}, but tests recorded {expected}; refusing to publish a replacement image");
                }
                let pinned = format!("sha256:{actual}");
                let computed = rechunk::fingerprint(ws, &pinned)?;
                let label = local_fingerprint(&pinned)?;
                if label != computed {
                    bail!("{image} fingerprint label {label} differs from recomputed {computed}; run prepare-publish and test that image ID first");
                }
                eprintln!("publishing tested machine image ID {actual} (no build)");
                (pinned, computed)
            } else {
                (image.clone(), local_fingerprint(&image)?)
            };
            let digest = push(&source, &format!("{repository}:{sha_tag}"), insecure, &auth)?;
            (digest, fingerprint)
        }
    };
    let pinned = format!("{repository}@{digest}");
    eprintln!("{pinned} is :{sha_tag} (fingerprint {fingerprint})");

    // 2. Sign the digest, then verify it the way we'll rely on it.
    let mut sign = Command::new("cosign");
    // No transparency log: containers-image checks only the key, and the log
    // would be a public record of every nightly build.
    sign.args([
        "sign",
        "--yes",
        "--key",
        "env://COSIGN_PRIVATE_KEY",
        "--tlog-upload=false",
    ]);
    sign.arg(&pinned);
    registry::with_auth(&auth, &mut sign);
    cmd::run(&mut sign)?;
    verify(ws, &pinned, &auth, args.verification_key.as_deref())?;

    // 3. Move the tags now that the digest is signed.
    for tag in [&moving_tag, &date_tag] {
        copy(&pinned, &format!("{repository}:{tag}"), insecure, &auth)?;
    }

    eprintln!("published {pinned} as :{sha_tag}, :{date_tag}, :{moving_tag}");
    registry::github_output("digest", &digest)?;
    registry::github_output("image", &pinned)?;
    registry::github_output("tags", &format!("{moving_tag},{date_tag},{sha_tag}"))?;
    Ok(())
}

/// `--stage`: push the local image as `:candidate`, unsigned, and report how
/// much of it machines on `:latest` would have to download.
fn stage(ws: &Workspace, args: &Args, repository: &str) -> Result<()> {
    let image = ws.resolve_image(args.image.as_deref());
    require_machine(&image, "generic")?;
    let fingerprint = local_fingerprint(&image)?;
    let insecure = registry::is_local_registry(repository);
    let auth = Auth::login(repository)?;
    let digest = push(
        &image,
        &format!("{repository}:{STAGE_TAG}"),
        insecure,
        &auth,
    )?;
    let pinned = format!("{repository}@{digest}");
    eprintln!("staged {pinned} as :{STAGE_TAG} (fingerprint {fingerprint}; unsigned)");

    // Reproducible layers only help machines if the registry gets the same
    // compressed blobs for them as last time.
    let layers = |reference: &str| -> Result<Option<Vec<(String, u64)>>> {
        Ok(
            skopeo_json(&["inspect", "--raw"], reference, insecure, &auth)?.map(|m| {
                m["layers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| Some((l["digest"].as_str()?.to_string(), l["size"].as_u64()?)))
                    .collect()
            }),
        )
    };
    let staged = layers(&pinned)?.unwrap_or_default();
    match layers(&format!("{repository}:latest"))? {
        Some(latest) => {
            let new: Vec<_> = staged
                .iter()
                .filter(|(d, _)| !latest.iter().any(|(l, _)| l == d))
                .collect();
            let bytes: u64 = new.iter().map(|(_, s)| s).sum();
            eprintln!(
                "{} of {} layers are new against :latest: an update downloads {} MiB",
                new.len(),
                staged.len(),
                bytes / (1024 * 1024)
            );
        }
        None => eprintln!("no :latest to compare with"),
    }
    registry::github_output("digest", &digest)
}

fn local_fingerprint(image: &str) -> Result<String> {
    rechunk::local_label(image, FINGERPRINT_LABEL)?.with_context(|| {
        format!("{image} has no {FINGERPRINT_LABEL} label; run `cargo xtask rechunk` for a generic image or `cargo xtask prepare-publish` for a machine, then test it first")
    })
}

/// `podman push` to `dest`; returns the digest the registry stored.
fn push(image: &str, dest: &str, insecure: bool, auth: &Option<Auth>) -> Result<String> {
    let digestfile = std::env::temp_dir().join(format!("ilex-digest-{}", std::process::id()));
    let mut push = Command::new("podman");
    push.arg("push").arg("--digestfile").arg(&digestfile);
    if insecure {
        push.arg("--tls-verify=false");
    }
    push.arg(image).arg(format!("docker://{dest}"));
    registry::with_auth(auth, &mut push);
    cmd::run(&mut push)?;
    let digest = fs::read_to_string(&digestfile)
        .with_context(|| format!("reading {}", digestfile.display()))?
        .trim()
        .to_string();
    let _ = fs::remove_file(&digestfile);
    if !registry::is_digest(&digest) {
        bail!("podman push wrote an unexpected digest: {digest:?}");
    }
    Ok(digest)
}

/// Point `dest` at `pinned` inside the registry: same manifest, same digest,
/// no blobs move.
fn copy(pinned: &str, dest: &str, insecure: bool, auth: &Option<Auth>) -> Result<()> {
    let mut copy = Command::new("skopeo");
    copy.args(["copy", "--preserve-digests"]);
    if insecure {
        copy.args(["--src-tls-verify=false", "--dest-tls-verify=false"]);
    }
    copy.arg(format!("docker://{pinned}"))
        .arg(format!("docker://{dest}"));
    registry::with_auth(auth, &mut copy);
    cmd::run(&mut copy)
}

/// `skopeo <args> docker://reference` as JSON; `None` when it isn't there.
fn skopeo_json(
    args: &[&str],
    reference: &str,
    insecure: bool,
    auth: &Option<Auth>,
) -> Result<Option<serde_json::Value>> {
    let mut skopeo = Command::new("skopeo");
    skopeo.args(args);
    if insecure {
        skopeo.arg("--tls-verify=false");
    }
    skopeo.arg(format!("docker://{reference}"));
    registry::with_auth(auth, &mut skopeo);
    let out = registry::capture(&mut skopeo)?;
    if !out.ok {
        if registry::looks_missing(&out.stderr) {
            return Ok(None);
        }
        bail!("skopeo could not inspect {reference}: {}", out.stderr);
    }
    serde_json::from_str(&out.stdout)
        .map(Some)
        .with_context(|| format!("parsing `skopeo {}` output", args.join(" ")))
}

/// `cosign verify` against the public key committed in `rootfs/`.
fn verify(ws: &Workspace, pinned: &str, auth: &Option<Auth>, local_key: Option<&std::path::Path>) -> Result<()> {
    let pubkey = local_key.map(PathBuf::from).unwrap_or_else(|| ws.root.join(PUBKEY));
    let mut verify = Command::new("cosign");
    verify
        .arg("verify")
        .arg("--key")
        .arg(&pubkey)
        .arg("--insecure-ignore-tlog=true");
    verify.arg(pinned);
    registry::with_auth(auth, &mut verify);
    let out = registry::capture(&mut verify)?;
    if !out.ok {
        bail!(
            "the signature on {pinned} does not verify against {PUBKEY} \
             (does COSIGN_PRIVATE_KEY match the committed public key?):\n{}",
            out.stderr
        );
    }
    eprintln!("signature on {pinned} verifies against {}", pubkey.display());
    Ok(())
}

fn require_machine(image: &str, expected: &str) -> Result<()> {
    let actual = machine::image_machine(image)?;
    if actual != expected {
        bail!("{image} is machine {actual}, expected {expected}; refusing to publish under another machine's tags");
    }
    Ok(())
}

