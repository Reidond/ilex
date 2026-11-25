//! Shared fingerprint/cache key inputs. Preserve phase 0's publication rules.
use crate::{Workspace, cmd, config::Selection, git};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// Repo paths whose content ends up in the image, besides the workspace crates
/// ([`image_crates`] adds the ones that are built).
///
/// `Cargo.toml` and `Cargo.lock` pin the dependencies of the shipped binaries.
/// They are hashed whole, so the lockfile stays the one input through which a
/// crate that is not shipped can still change the fingerprint: any dependency
/// change in any workspace member (the installer's GTK stack, say) edits
/// `Cargo.lock`, and every root image is then rebuilt, re-signed and
/// republished once although its binaries are unchanged. The same lockfile is an
/// input of the Anaconda installer ISO (`xtask/src/iso.rs`), which falls due the
/// same way.
const INPUTS: &[&str] = &[
    "users",
    "rootfs",
    "Containerfile",
    "packages.toml",
    "Cargo.toml",
    "Cargo.lock",
    "build",
    "machines/generic",
];

/// What `cargo build` lines of the root `Containerfile` ask for.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone)]
struct CargoBuild {
    packages: Vec<String>,
    no_default_features: bool,
    /// `--features` or `--all-features`: widened to all features, which can
    /// only add crates to the closure, never drop one.
    all_features: bool,
}

/// The `cargo build -p …` commands of a Containerfile, one per line starting
/// with `cargo build`. Fails when there is none, rather than hash nothing, and
/// on a flag that would make the package set depend on something this does not
/// model.
fn cargo_builds(containerfile: &str) -> Result<Vec<CargoBuild>> {
    let mut builds = Vec::new();
    for line in containerfile.lines() {
        let words: Vec<&str> = line.split_whitespace().collect();
        if !words.starts_with(&["cargo", "build"]) {
            continue;
        }
        let mut build = CargoBuild {
            packages: Vec::new(),
            no_default_features: false,
            all_features: false,
        };
        let mut words = words.into_iter().skip(2);
        while let Some(word) = words.next() {
            match word {
                "-p" | "--package" => {
                    build.packages.push(
                        words
                            .next()
                            .with_context(|| format!("`{line}` ends after {word}"))?
                            .to_owned(),
                    );
                }
                "--no-default-features" => build.no_default_features = true,
                "--all-features" | "-F" | "--features" => {
                    build.all_features = true;
                    if word != "--all-features" {
                        words.next();
                    }
                }
                "--workspace" | "--all" | "--exclude" | "--bins" | "--bin" | "--lib" => {
                    bail!(
                        "the Containerfile's `{line}` selects targets the fingerprint's crate closure does not model; teach `cargo_builds` first"
                    )
                }
                other => {
                    if let Some(package) = other.strip_prefix("--package=") {
                        build.packages.push(package.to_owned());
                    } else if other.starts_with("--features=") {
                        build.all_features = true;
                    }
                }
            }
        }
        ensure!(
            !build.packages.is_empty(),
            "the Containerfile's `{line}` names no package with -p; the root image's crates cannot be derived"
        );
        builds.push(build);
    }
    ensure!(
        !builds.is_empty(),
        "the Containerfile has no `cargo build -p …` line; the root image's crates cannot be derived"
    );
    builds.sort();
    builds.dedup();
    Ok(builds)
}

/// Workspace crates of the dependency closure of one build: the packages it
/// names and every workspace package they depend on (normal and build
/// dependencies for Linux x86_64 with the build's features; dev dependencies
/// are not built), as repo-relative directories.
fn closure(ws: &Workspace, build: &CargoBuild) -> Result<BTreeSet<String>> {
    let mut metadata = Command::new("cargo");
    metadata
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            "x86_64-unknown-linux-gnu",
        ])
        .current_dir(&ws.root);
    if build.all_features {
        metadata.arg("--all-features");
    } else if build.no_default_features {
        metadata.arg("--no-default-features");
    }
    let output = cmd::output(&mut metadata)
        .context("deriving the root image's crates with `cargo metadata`")?;
    let metadata: serde_json::Value =
        serde_json::from_str(&output).context("parsing `cargo metadata`")?;

    let root = std::fs::canonicalize(&ws.root)?;
    let mut directory = BTreeMap::new();
    let mut name = BTreeMap::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let (Some(id), Some(package_name), Some(manifest)) = (
            package["id"].as_str(),
            package["name"].as_str(),
            package["manifest_path"].as_str(),
        ) else {
            continue;
        };
        name.insert(id, package_name);
        if let Ok(manifest) = std::fs::canonicalize(manifest)
            && let Some(dir) = manifest.parent().and_then(|d| d.strip_prefix(&root).ok())
        {
            directory.insert(id, dir.to_string_lossy().replace('\\', "/"));
        }
    }
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|id| id.as_str())
        .collect();
    let mut dependencies: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for node in metadata["resolve"]["nodes"].as_array().into_iter().flatten() {
        let Some(id) = node["id"].as_str() else {
            continue;
        };
        let built = node["deps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|dep| {
                dep["dep_kinds"].as_array().into_iter().flatten().any(|kind| {
                    // null is a normal dependency; "build" a build script's.
                    kind["kind"].as_str() != Some("dev")
                })
            })
            .filter_map(|dep| dep["pkg"].as_str())
            .collect();
        dependencies.insert(id, built);
    }

    let mut pending: Vec<&str> = Vec::new();
    for wanted in &build.packages {
        let id = members
            .iter()
            .copied()
            .find(|id| name.get(id) == Some(&wanted.as_str()))
            .with_context(|| {
                format!("the Containerfile builds `{wanted}`, which is not a workspace package")
            })?;
        pending.push(id);
    }
    let mut seen = BTreeSet::new();
    let mut crates = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        if members.contains(id) {
            crates.insert(
                directory
                    .get(id)
                    .cloned()
                    .with_context(|| format!("workspace package {id} is outside the checkout"))?,
            );
        }
        pending.extend(dependencies.get(id).into_iter().flatten().copied());
    }
    Ok(crates)
}

/// The crates whose source can change a binary of the root image: the
/// dependency closure, derived from `cargo metadata`, of every package the root
/// `Containerfile` builds (not a list kept by hand, which would rot). An edit
/// to any other crate (the installer, the e2e harness, the development
/// tooling) cannot reach the image and no longer changes its fingerprint.
pub fn image_crates(ws: &Workspace) -> Result<Vec<String>> {
    static CACHE: OnceLock<Mutex<BTreeMap<PathBuf, Vec<String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(crates) = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("crate closure cache poisoned"))?
        .get(&ws.root)
    {
        return Ok(crates.clone());
    }
    let containerfile = std::fs::read_to_string(ws.root.join("Containerfile"))
        .context("reading the root Containerfile")?;
    let mut crates = BTreeSet::new();
    for build in cargo_builds(&containerfile)? {
        crates.extend(closure(ws, &build)?);
    }
    let crates: Vec<String> = crates.into_iter().collect();
    eprintln!("  root image crates: {}", crates.join(" "));
    cache
        .lock()
        .map_err(|_| anyhow::anyhow!("crate closure cache poisoned"))?
        .insert(ws.root.clone(), crates.clone());
    Ok(crates)
}

/// Early root cache key: selected source trees plus both freshly pulled build
/// images. The full RPM/content fingerprint still decides publication after a
/// cache miss. Missing keys on older publications deliberately force a build.
pub fn root_inputs(ws: &Workspace, selection: &Selection) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"ilex-root-inputs v1\n");
    hash_selection(&mut hash, selection);
    hash_users(ws, &mut hash)?;
    for image in [crate::image::BASE_IMAGE, crate::image::BUILDER_IMAGE] {
        let digest = cmd::output(Command::new("podman").args([
            "image",
            "inspect",
            "--format",
            "{{.Digest}}",
            image,
        ]))?;
        if !(digest.starts_with("sha256:")
            && digest.len() == 71
            && digest[7..].bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!("invalid upstream digest for {image}: {digest}");
        }
        hash.update(format!("upstream {image} {digest}\n"));
    }
    hash_inputs(ws, &mut hash, selection_inputs(ws, selection)?)?;
    Ok(format!("sha256:{}", hex(&hash.finalize())))
}

pub fn selection_inputs(ws: &Workspace, selection: &Selection) -> Result<Vec<String>> {
    let mut inputs: Vec<String> = INPUTS.iter().map(|s| s.to_string()).collect();
    inputs.extend(image_crates(ws)?);
    if !selection.shells.is_empty() {
        inputs.push("shells/packages.toml".into());
    }
    if selection.machine != "generic" {
        inputs.extend([
            format!("machines/{}", selection.machine),
            "machines/Containerfile".into(),
        ]);
    }
    inputs.extend(selection.shells.iter().map(|name| format!("shells/{name}")));
    Ok(inputs)
}

pub fn hash_selection(hash: &mut Sha256, selection: &Selection) {
    for (key, value) in selection.labels() {
        hash.update(format!("selection {key}={value}\n"));
    }
}

// User selection affects the public catalogue, never the three OCI selection
// labels or publication names. Hash it separately so changing only the list
// cannot reuse a root with a different catalogue.
pub fn hash_users(ws: &Workspace, hash: &mut Sha256) -> Result<()> {
    let manifest = crate::config::manifest(&ws.root.join("image.toml"))?;
    let users = manifest
        .get("users")
        .map(ToString::to_string)
        .unwrap_or_else(|| "[]".into());
    hash.update(format!("users {users}\n"));
    Ok(())
}

pub fn hash_inputs(ws: &Workspace, hash: &mut Sha256, inputs: Vec<String>) -> Result<()> {
    let source = crate::source::manifest(ws)?;
    // Preserve the published clean-tree encoding in both executors. The shallow
    // snapshot includes the original HEAD tree, so its Git object IDs are equal.
    for path in &inputs {
        let tree = match git::tree_hash(&ws.root, path) {
            Ok(tree) => tree,
            // A crate added to the build's dependencies in the working tree is
            // not at HEAD yet; its files are covered by the dirty entries.
            Err(_) if source.dirty && ws.root.join(path).exists() => "-".to_owned(),
            Err(error) => return Err(error).with_context(|| format!("hashing {path}")),
        };
        eprintln!("  {path}: {tree}");
        hash.update(format!("tree {path} {tree}\n"));
    }
    if source.dirty {
        hash.update(b"dirty\n");
        hash.update(b"working-inputs v1\n");
        // The same canonical manifest encoding covers dirty working bytes,
        // modes, links and deletions on native Linux and in a remote snapshot.
        for entry in &source.entries {
            if inputs
                .iter()
                .any(|p| entry.path == *p || entry.path.starts_with(&format!("{p}/")))
            {
                hash.update(serde_json::to_vec(entry)?);
                hash.update(b"\n");
            }
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
