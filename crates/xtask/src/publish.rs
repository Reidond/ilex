//! `cargo xtask publish`: push by digest, tag and sign (CI only).
//!
//! 1. Push a tested composition once as `:<tag>_sha-<short>` and read back the
//!    digest the registry stored. In CI the image is already there: the build
//!    job pushed it as `:candidate.<tag>` (`--stage`) so the upgrade tier could pull
//!    it on its own runner, and the publish job hands that digest over
//!    (`--digest`), so what is signed is exactly what was tested.
//! 2. Sign that digest with the cosign key pair (`COSIGN_PRIVATE_KEY` +
//!    `COSIGN_PASSWORD`), in the legacy `sha256-<digest>.sig` tag format that
//!    bootc / containers-image verify, and check the signature against the
//!    public key shipped in the image.
//! 3. Only then point `:<tag>` and `:<tag>_YYYYMMDD` at the digest, so a machine
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
const REVISION_LABEL: &str = "org.opencontainers.image.revision";

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
    /// Push unsigned as `:candidate.<tag>` for tests and print its `digest=`.
    /// Published composition tags do not move.
    #[arg(long, conflicts_with = "digest")]
    pub stage: bool,
    /// Publish the image already in the repository at this digest (what
    /// `--stage` pushed) instead of pushing a local one
    #[arg(long, value_name = "sha256:…")]
    pub digest: Option<String>,
    /// Publish only this image.toml composition, under its derived tag
    #[arg(long = "as")]
    pub as_tag: String,
    /// Immutable ID recorded before this machine's capability and VM tests
    #[arg(long, conflicts_with_all = ["stage", "digest"])]
    pub tested_image_id: Option<String>,
    /// Root fingerprint recorded by --stage before testing this digest
    #[arg(long, requires = "digest", value_name = "sha256:…")]
    pub tested_fingerprint: Option<String>,
    /// Throwaway public key for local-registry verification only
    #[arg(long, requires = "allow_local_registry")]
    pub verification_key: Option<PathBuf>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let repository = registry::repository(args.repository.as_deref(), "ilex")?;
    registry::require_ci("xtask publish", &repository, args.allow_local_registry)?;
    if args.allow_local_registry && !registry::is_local_registry(&repository) {
        bail!(
            "--allow-local-registry only works with a registry on this machine, not {repository}"
        );
    }
    let publication = publication(ws, &args)?;
    if args.verification_key.is_some() && !registry::is_local_registry(&repository) {
        bail!("--verification-key is only allowed with a registry on this machine");
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
    let moving_tag = &args.as_tag;
    let date_tag = format!("{moving_tag}_{date}");
    let sha_tag = format!("{moving_tag}_sha-{short}");
    for tag in [moving_tag, &date_tag, &sha_tag] {
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
            let labels = serde_json::from_value(config["config"]["Labels"].clone())
                .context("reading staged image's configuration labels")?;
            require_composition(&pinned, &labels, &publication)?;
            require_ci_revision(&pinned, config["config"]["Labels"][REVISION_LABEL].as_str())?;
            let fingerprint = config["config"]["Labels"][FINGERPRINT_LABEL]
                .as_str()
                .map(str::to_string)
                .with_context(|| format!("{pinned} has no {FINGERPRINT_LABEL} label"))?;
            {
                require_fingerprint(&pinned, &fingerprint)?;
                let tested = args.tested_fingerprint.as_ref().unwrap();
                if &fingerprint != tested {
                    bail!(
                        "{pinned} fingerprint label {fingerprint} differs from tested {tested}; refusing to sign a different staged root"
                    );
                }
            }
            copy(&pinned, &format!("{repository}:{sha_tag}"), insecure, &auth)?;
            (digest.clone(), fingerprint)
        }
        None => {
            let image = ws.resolve_image(args.image.as_deref());
            let tested = args
                .tested_image_id
                .as_ref()
                .context("layer requires --tested-image-id")?;
            let expected = tested.strip_prefix("sha256:").unwrap_or(tested);
            if !registry::is_digest(&format!("sha256:{expected}")) {
                bail!("--tested-image-id {tested:?} is not a sha256 image ID");
            }
            let actual = bcvk::image_id(&image)?;
            if actual != expected {
                bail!(
                    "{image} is image {actual}, but tests recorded {expected}; refusing to publish a replacement image"
                );
            }
            let pinned = format!("sha256:{actual}");
            let inspect = require_local_composition(&pinned, &publication)?;
            require_ci_revision(
                &pinned,
                inspect["Config"]["Labels"][REVISION_LABEL].as_str(),
            )?;
            let parent =
                require_layer_parent(ws, &pinned, &inspect, &publication, &repository, &auth)?;
            verify(ws, &parent, &auth, args.verification_key.as_deref())
                .context("verifying the declared published parent before signing its layer")?;
            let computed = rechunk::fingerprint(ws, &pinned)?;
            let label = local_fingerprint(&pinned)?;
            require_fingerprint(&pinned, &label)?;
            if label != computed {
                bail!(
                    "{image} fingerprint label {label} differs from recomputed {computed}; run prepare-publish and test that image ID first"
                );
            }
            eprintln!("publishing tested machine image ID {actual} (no build)");
            let (source, fingerprint) = (pinned, computed);
            let digest = push(&source, &format!("{repository}:{sha_tag}"), insecure, &auth)?;
            check_pushed_image(&format!("{repository}@{digest}"), &source, insecure, &auth)?;
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
    for tag in [moving_tag, &date_tag] {
        copy(&pinned, &format!("{repository}:{tag}"), insecure, &auth)?;
    }

    eprintln!("published {pinned} as :{sha_tag}, :{date_tag}, :{moving_tag}");
    registry::github_output("digest", &digest)?;
    registry::github_output("image", &pinned)?;
    registry::github_output("tags", &format!("{moving_tag},{date_tag},{sha_tag}"))?;
    Ok(())
}

/// A signed CI image must record the exact clean commit of this run. Each
/// machine checks its own layer, never the possibly older generic base.
fn require_ci_revision(image: &str, revision: Option<&str>) -> Result<()> {
    if !registry::is_ci() {
        return Ok(());
    }
    let expected = std::env::var("GITHUB_SHA").context("CI publication requires GITHUB_SHA")?;
    if expected.len() != 40
        || !expected
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("CI publication requires GITHUB_SHA to be 40 lowercase hex digits, got {expected:?}");
    }
    if revision != Some(expected.as_str()) {
        bail!(
            "CI publication of {image} requires {REVISION_LABEL}={expected}, got {}; refusing to sign",
            revision.unwrap_or("(missing)")
        );
    }
    eprintln!("CI revision check: {image} records {expected}, matching GITHUB_SHA");
    Ok(())
}

/// Stage this root unsigned and report the update size against its own tag.
fn stage(ws: &Workspace, args: &Args, repository: &str) -> Result<()> {
    let image = ws.resolve_image(args.image.as_deref());
    let image_id = format!("sha256:{}", bcvk::image_id(&image)?);
    let publication = publication(ws, args)?;
    let inspect = require_local_composition(&image_id, &publication)?;
    require_ci_revision(
        &image_id,
        inspect["Config"]["Labels"][REVISION_LABEL].as_str(),
    )?;
    let stage_tag = format!("candidate.{}", args.as_tag);
    check_tag_length(&format!("{repository}:{stage_tag}"))?;
    let fingerprint = local_fingerprint(&image_id)?;
    {
        require_fingerprint(&image_id, &fingerprint)?;
        let computed = rechunk::fingerprint(ws, &image_id)?;
        if fingerprint != computed {
            bail!(
                "{image_id} fingerprint label {fingerprint} differs from recomputed {computed}; rechunk and test that image first"
            );
        }
    }
    let insecure = registry::is_local_registry(repository);
    let auth = Auth::login(repository)?;
    let digest = push(
        &image_id,
        &format!("{repository}:{stage_tag}"),
        insecure,
        &auth,
    )?;
    let pinned = format!("{repository}@{digest}");
    check_pushed_image(&pinned, &image_id, insecure, &auth)?;
    eprintln!("staged {pinned} as :{stage_tag} (fingerprint {fingerprint}; unsigned)");

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
    let comparison = &args.as_tag;
    match layers(&format!("{repository}:{comparison}"))? {
        Some(latest) => {
            let new: Vec<_> = staged
                .iter()
                .filter(|(d, _)| !latest.iter().any(|(l, _)| l == d))
                .collect();
            let bytes: u64 = new.iter().map(|(_, s)| s).sum();
            eprintln!(
                "{} of {} layers are new against :{comparison}: an update downloads {} MiB",
                new.len(),
                staged.len(),
                bytes / (1024 * 1024)
            );
        }
        None => eprintln!("no :{comparison} to compare with"),
    }
    registry::github_output("fingerprint", &fingerprint)?;
    registry::github_output("digest", &digest)
}

fn publication(ws: &Workspace, args: &Args) -> Result<config::Publication> {
    let tag = &args.as_tag;
    let row = config::publications(&ws.root)?
        .into_iter()
        .find(|row| &row.tag == tag)
        .with_context(|| format!("composition `{tag}` is not published by image.toml"))?;
    if row.kind == "root" {
        if args.tested_image_id.is_some() || (!args.stage && args.digest.is_none()) {
            bail!(
                "--as {tag} is a root composition: use --stage or --digest, not --tested-image-id"
            );
        }
        if args.digest.is_some() {
            let tested = args.tested_fingerprint.as_deref().context(
                "--digest --as requires --tested-fingerprint recorded by --stage before tests",
            )?;
            require_fingerprint("--tested-fingerprint", tested)?;
        }
    } else if args.stage || args.digest.is_some() || args.tested_image_id.is_none() {
        bail!(
            "--as {tag} is a layer composition: requires --tested-image-id and cannot use --stage or --digest"
        );
    }
    Ok(row)
}

fn require_local_composition(image: &str, row: &config::Publication) -> Result<serde_json::Value> {
    let inspect = cmd::output(Command::new("podman").args(["image", "inspect", image]))?;
    let inspect: serde_json::Value = serde_json::from_str(&inspect)?;
    let labels = serde_json::from_value(inspect[0]["Config"]["Labels"].clone())?;
    require_composition(image, &labels, row)?;
    Ok(inspect[0].clone())
}

fn require_composition(
    image: &str,
    labels: &std::collections::BTreeMap<String, String>,
    row: &config::Publication,
) -> Result<()> {
    if labels.contains_key("ilex.development") {
        bail!("{image} is a development composition and must never be published");
    }
    let expected = row.selection.labels();
    for key in config::LABELS {
        let actual = labels.get(key).map(String::as_str).unwrap_or("(missing)");
        if labels.get(key) != expected.get(key) {
            bail!(
                "{image} cannot be published as {}: {key}={actual:?}, expected {:?} from image.toml",
                row.tag,
                expected[key]
            );
        }
    }
    let selection = config::Selection::from_labels(labels)?;
    let actual = config::published_tag(&selection).with_context(|| {
        format!("{image} has no valid published composition (default shell must be first)")
    })?;
    if actual != row.tag || selection.machine != row.machine || selection.shells != row.shells {
        bail!(
            "{image} is composition {actual}, expected {}; refusing to publish under another composition's tags",
            row.tag
        );
    }
    Ok(())
}

fn require_fingerprint(image: &str, fingerprint: &str) -> Result<()> {
    if !registry::is_digest(fingerprint) {
        bail!("{image}: {FINGERPRINT_LABEL} must be a sha256 digest, got {fingerprint:?}");
    }
    Ok(())
}

/// Bind a layer to the current root named by its publication, independently of
/// its own fingerprint and self-reported parent/input labels. Root config and
/// diff IDs come from the registry by immutable digest; no pull or build runs.
fn require_layer_parent(
    ws: &Workspace,
    image: &str,
    inspect: &serde_json::Value,
    row: &config::Publication,
    repository: &str,
    auth: &Option<Auth>,
) -> Result<String> {
    let insecure = registry::is_local_registry(repository);
    let parent = row
        .parent
        .as_ref()
        .context("layer publication has no parent")?;
    let tag = format!("{repository}:{parent}");
    let metadata = skopeo_json(&["inspect"], &tag, insecure, auth)?
        .with_context(|| format!("declared parent {tag} is not published"))?;
    let digest = metadata["Digest"]
        .as_str()
        .filter(|digest| registry::is_digest(digest))
        .with_context(|| format!("{tag} has no valid manifest digest"))?;
    let pinned = format!("{repository}@{digest}");
    let manifest = skopeo_json(&["inspect", "--raw"], &pinned, insecure, auth)?
        .with_context(|| format!("published parent {pinned} is missing"))?;
    let config_id = manifest["config"]["digest"]
        .as_str()
        .filter(|id| registry::is_digest(id))
        .with_context(|| format!("{pinned} has no valid config digest"))?;
    let config = skopeo_json(&["inspect", "--config"], &pinned, insecure, auth)?
        .with_context(|| format!("published parent {pinned} has no config"))?;
    let mut expected_parent = row.clone();
    expected_parent.tag = parent.clone();
    expected_parent.machine = "generic".into();
    expected_parent.selection.machine = "generic".into();
    let labels = serde_json::from_value(config["config"]["Labels"].clone())?;
    require_composition(&pinned, &labels, &expected_parent)?;

    let inputs = machine::inputs(ws, &ws.root.join("machines").join(&row.machine), &[])?;
    let expected_key = format!("{}-{inputs}", config_id.strip_prefix("sha256:").unwrap());
    for (label, expected) in [
        ("ilex.machine-key", expected_key),
        ("ilex.machine-inputs", inputs),
    ] {
        let actual = inspect["Config"]["Labels"][label]
            .as_str()
            .unwrap_or("(missing)");
        if actual != expected {
            bail!(
                "{image}: {label}={actual:?}, expected {expected:?} for declared parent {tag} ({config_id}) and current machine inputs"
            );
        }
    }
    let parent_layers: Vec<String> =
        serde_json::from_value(config["rootfs"]["diff_ids"].clone())
            .with_context(|| format!("{pinned} has no root filesystem diff IDs"))?;
    let layers: Vec<String> = serde_json::from_value(inspect["RootFS"]["Layers"].clone())
        .with_context(|| format!("{image} has no root filesystem diff IDs"))?;
    if layers.len() != parent_layers.len() + 1 || !layers.starts_with(&parent_layers) {
        bail!(
            "{image} filesystem layers are not exactly one layer on declared parent {tag} ({config_id}); refusing a forged parent label"
        );
    }
    eprintln!(
        "{image} preserves every filesystem layer of {tag} ({config_id}) and adds one machine layer"
    );
    Ok(pinned)
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

/// Prove against the registry that pushing preserved the tested local config.
/// Before signing/signed moving tags; staging remains unsigned on failure too.
fn check_pushed_image(
    pinned: &str,
    image_id: &str,
    insecure: bool,
    auth: &Option<Auth>,
) -> Result<()> {
    let manifest = skopeo_json(&["inspect", "--raw"], pinned, insecure, auth)?
        .with_context(|| format!("pushed image {pinned} is missing from the registry"))?;
    let config = manifest["config"]["digest"]
        .as_str()
        .with_context(|| format!("{pinned} has no manifest config digest; refusing to sign"))?;
    if config != image_id {
        bail!(
            "{pinned} config digest {config} differs from local/tested image ID {image_id}; refusing to sign"
        );
    }
    eprintln!(
        "pushed manifest config {config} matches local/tested image ID {image_id}; identity preserved"
    );
    Ok(())
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
fn verify(
    ws: &Workspace,
    pinned: &str,
    auth: &Option<Auth>,
    local_key: Option<&std::path::Path>,
) -> Result<()> {
    let pubkey = local_key
        .map(PathBuf::from)
        .unwrap_or_else(|| ws.root.join(PUBKEY));
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
    eprintln!(
        "signature on {pinned} verifies against {}",
        pubkey.display()
    );
    Ok(())
}
