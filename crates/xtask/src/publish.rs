//! `cargo xtask publish`: push by digest, tag and sign (CI only).
//!
//! 1. Push the rechunked image once as `:sha-<short>` and read back the
//!    digest the registry stored.
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
use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd, git};

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
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let repository = registry::repository(args.repository.as_deref(), "ilex")?;
    registry::require_ci("xtask publish", &repository, args.allow_local_registry)?;
    cmd::require("podman")?;
    cmd::require("skopeo")?;
    cmd::require("cosign")?;
    for var in ["COSIGN_PRIVATE_KEY", "COSIGN_PASSWORD"] {
        if std::env::var_os(var).is_none_or(|v| v.is_empty()) {
            bail!("{var} is not set (repository secret in CI)");
        }
    }

    let image = ws.resolve_image(args.image.as_deref());
    let fingerprint = rechunk::local_label(&image, FINGERPRINT_LABEL)?.with_context(|| {
        format!("{image} has no {FINGERPRINT_LABEL} label; run `cargo xtask rechunk` on it first")
    })?;
    let short = git::short_sha(&ws.root)?;
    let date = registry::utc_date();
    let insecure = registry::is_local_registry(&repository);
    let auth = Auth::login(&repository)?;

    // 1. Push by tag `sha-<short>`; the digest is what everything else uses.
    let sha_tag = format!("sha-{short}");
    let digestfile = std::env::temp_dir().join(format!("ilex-digest-{}", std::process::id()));
    let mut push = Command::new("podman");
    push.arg("push").arg("--digestfile").arg(&digestfile);
    if insecure {
        push.arg("--tls-verify=false");
    }
    push.arg(&image)
        .arg(format!("docker://{repository}:{sha_tag}"));
    registry::with_auth(&auth, &mut push);
    cmd::run(&mut push)?;
    let digest = fs::read_to_string(&digestfile)
        .with_context(|| format!("reading {}", digestfile.display()))?
        .trim()
        .to_string();
    let _ = fs::remove_file(&digestfile);
    if !registry::is_digest(&digest) {
        bail!("podman push wrote an unexpected digest: {digest:?}");
    }
    let pinned = format!("{repository}@{digest}");
    eprintln!("pushed {pinned} (fingerprint {fingerprint})");

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
    verify(ws, &pinned, &auth)?;

    // 3. Move the tags now that the digest is signed.
    for tag in ["latest", date.as_str()] {
        let mut copy = Command::new("skopeo");
        copy.args(["copy", "--preserve-digests"]);
        if insecure {
            copy.args(["--src-tls-verify=false", "--dest-tls-verify=false"]);
        }
        copy.arg(format!("docker://{pinned}"))
            .arg(format!("docker://{repository}:{tag}"));
        registry::with_auth(&auth, &mut copy);
        cmd::run(&mut copy)?;
    }

    eprintln!("published {pinned} as :{sha_tag}, :{date}, :latest");
    registry::github_output("digest", &digest)?;
    registry::github_output("image", &pinned)?;
    registry::github_output("tags", &format!("latest,{date},{sha_tag}"))?;
    Ok(())
}

/// `cosign verify` against the public key committed in `rootfs/`.
fn verify(ws: &Workspace, pinned: &str, auth: &Option<Auth>) -> Result<()> {
    let pubkey = ws.root.join(PUBKEY);
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
    eprintln!("signature on {pinned} verifies against {PUBKEY}");
    Ok(())
}
