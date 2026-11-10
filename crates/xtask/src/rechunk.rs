//! `cargo xtask rechunk`: split the image into reproducible chunks and add
//! the `ilex.fingerprint` label.
//!
//! A plain `podman build` writes a new tar stream with new timestamps every
//! time, so clients would download a "new" image even when nothing changed.
//! `bootc-base-imagectl rechunk --chunkah`, run from the base image, rebuilds
//! the image from its final merged filesystem into layers split by RPM
//! package, with normalised timestamps: the same content always produces the
//! same layers. It reads the image through a podman image mount and writes an
//! OCI archive to stdout, so it needs no root and no access to container
//! storage. The result replaces the input tag in place, or goes to its own
//! tag with `--output` (CI rechunks while it tests the input).
//!
//! The fingerprint label is the hash that `xtask changed` compares with the
//! published image: the sorted `rpm -qa` of the image plus the git tree
//! hashes of everything in the repo that ends up in the image. It is the
//! same before and after the rechunk, so `xtask changed` can decide on the
//! image as built.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd, git};
use sha2::{Digest, Sha256};

use crate::build::{BASE_IMAGE, image_exists};
use crate::registry::FINGERPRINT_LABEL;

/// Repo paths whose content ends up in the image. `Cargo.toml`/`Cargo.lock`
/// pin the dependencies of the shipped binaries.
const INPUTS: &[&str] = &[
    "rootfs",
    "crates",
    "Containerfile",
    "packages.toml",
    "Cargo.toml",
    "Cargo.lock",
];

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Image to rechunk: tag (e.g. `candidate`) or full reference (defaults to
    /// this checkout's image). Replaced in place unless `--output` is given.
    pub image: Option<String>,
    /// Write the rechunked image to this tag or reference and leave the input
    /// as it is, so the input can be tested while this runs
    #[arg(long)]
    pub output: Option<String>,
    /// Write this fingerprint into the label instead of computing it (the
    /// `fingerprint=` that `xtask changed` printed)
    #[arg(long)]
    pub fingerprint: Option<String>,
    /// Maximum number of layers in the result
    #[arg(long)]
    pub max_layers: Option<u32>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    cmd::require("podman")?;
    let input = ws.resolve_image(args.image.as_deref());
    if !image_exists(&input) {
        bail!("{input} is not in local container storage; run `cargo xtask build` first");
    }
    // Where the result goes: a tag of its own, or over the input.
    let image = args
        .output
        .as_deref()
        .map_or_else(|| input.clone(), |o| ws.resolve_image(Some(o)));
    let in_place = image == input;

    // The same value `xtask changed` computes on the un-rechunked image
    // (rechunking keeps the RPM database, and the repo trees are the same);
    // CI hands it over so the label is exactly what `changed` compared.
    let fingerprint = match args.fingerprint {
        Some(given) => given,
        None => fingerprint(ws, &input)?,
    };
    eprintln!("fingerprint: {fingerprint}");

    // chunkah keeps the original image's config (labels, env, cmd) from the
    // `podman inspect` JSON we hand it, so the new label goes in there.
    let mut config: serde_json::Value = serde_json::from_str(&cmd::output(
        Command::new("podman").args(["image", "inspect", &input]),
    )?)
    .context("parsing `podman image inspect` output")?;
    let labels = &mut config[0]["Config"]["Labels"];
    if labels.is_null() {
        *labels = serde_json::json!({});
    }
    labels[FINGERPRINT_LABEL] = serde_json::Value::String(fingerprint.clone());
    let old_id = podman_id(&input)?;

    // podman run … rechunk --chunkah | podman load
    // `--log-driver=passthrough` hands our pipe to the container as its
    // stdout. By default conmon relays the stream and also writes all of it
    // (~4.7 GB of tar) to the container log: 188 s locally against 50 s, and
    // ~7.5 of the 9.5 minutes the rechunk took in CI.
    let mut chunk = Command::new("podman");
    chunk
        .args(["run", "--rm", "--log-driver=passthrough"])
        .arg(format!("--mount=type=image,src={input},dest=/chunkah"))
        .args(["--env", "CHUNKAH_CONFIG_STR", BASE_IMAGE])
        .args(["/usr/libexec/bootc-base-imagectl", "rechunk", "--chunkah"]);
    if let Some(n) = args.max_layers {
        chunk.arg(format!("--max-layers={n}"));
    }
    chunk.env("CHUNKAH_CONFIG_STR", config.to_string());
    let mut load = Command::new("podman");
    load.args(["load", "--quiet"]);
    eprintln!("$ {} | {}", cmd::display(&chunk), cmd::display(&load));

    let mut producer = chunk
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning `podman run … rechunk`")?;
    let loaded = load
        .stdin(producer.stdout.take().context("rechunk stdout")?)
        .stderr(Stdio::inherit())
        .output()
        .context("spawning `podman load`")?;
    let produced = producer.wait()?;
    if !produced.success() {
        bail!("`bootc-base-imagectl rechunk --chunkah` failed with {produced}");
    }
    if !loaded.status.success() {
        bail!("`podman load` failed with {}", loaded.status);
    }
    let out = String::from_utf8_lossy(&loaded.stdout);
    let new_ref = out
        .lines()
        .rev()
        .find_map(|l| {
            l.trim()
                .strip_prefix("Loaded image: ")
                .or_else(|| Some(l.trim()).filter(|l| !l.is_empty()))
        })
        .context("`podman load` printed no image")?
        .to_string();

    // Tag the rechunked image: over the input, or as its own image.
    cmd::run(Command::new("podman").args(["tag", &new_ref, &image]))?;
    let new_id = podman_id(&image)?;
    if in_place && new_id != old_id {
        // The superseded image is now untagged; drop it if nothing uses it.
        let _ = Command::new("podman")
            .args(["rmi", &old_id])
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .status();
    }

    let got = local_label(&image, FINGERPRINT_LABEL)?;
    if got.as_deref() != Some(fingerprint.as_str()) {
        bail!("rechunked {image} lost the {FINGERPRINT_LABEL} label (got {got:?})");
    }
    eprintln!("rechunked {image} ({new_id})");
    println!("{image}");
    Ok(())
}

/// The fingerprint of a candidate image: its `ilex.fingerprint` label when it
/// has one (a rechunked image), otherwise computed from the image as built.
/// Both are the same value, so `xtask changed` can decide right after the
/// build and a run that changed nothing never pays for the rechunk.
pub fn candidate_fingerprint(ws: &Workspace, image: &str) -> Result<String> {
    match local_label(image, FINGERPRINT_LABEL)? {
        Some(fingerprint) => Ok(fingerprint),
        None => fingerprint(ws, image),
    }
}

/// Content fingerprint of `image`: `sha256:<hex>` over the sorted `rpm -qa`
/// output of the image and the git tree hashes of [`INPUTS`] at `HEAD`.
pub fn fingerprint(ws: &Workspace, image: &str) -> Result<String> {
    let rpm = cmd::output(Command::new("podman").args([
        "run",
        "--rm",
        "--network=none",
        "--entrypoint",
        "rpm",
        image,
        "-qa",
    ]))?;
    let mut packages: Vec<&str> = rpm
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    packages.sort_unstable();

    let mut hash = Sha256::new();
    hash.update(b"ilex-fingerprint v1\n");
    for p in &packages {
        hash.update(format!("rpm {p}\n"));
    }
    for path in INPUTS {
        let tree = git::tree_hash(&ws.root, path).with_context(|| format!("hashing {path}"))?;
        eprintln!("  {path}: {tree}");
        hash.update(format!("tree {path} {tree}\n"));
    }
    eprintln!("  {} rpm packages", packages.len());
    if git::is_dirty(&ws.root)? {
        // Only HEAD's trees are hashed; make a dirty build's fingerprint
        // differ from the clean one so it can never match a published image.
        eprintln!("xtask: warning: working tree is dirty; the fingerprint covers HEAD only");
        hash.update(b"dirty\n");
    }
    Ok(format!("sha256:{}", hex(&hash.finalize())))
}

/// Value of label `key` on a local image, if set.
pub fn local_label(image: &str, key: &str) -> Result<Option<String>> {
    let labels = cmd::output(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        image,
    ]))?;
    let labels: serde_json::Value =
        serde_json::from_str(&labels).context("parsing image labels")?;
    Ok(labels[key].as_str().map(str::to_string))
}

fn podman_id(image: &str) -> Result<String> {
    cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Id}}", image]))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
