//! Shared fingerprint/cache key inputs. Preserve phase 0's publication rules.
use crate::image_crates_guard::{self, CargoBuild, Scratch, derived_image_builds};
use crate::{Workspace, cmd, config::Selection, git};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Repo paths whose content ends up in the image, besides the crates
/// ([`image_crates`] adds the ones that are built).
///
/// `Containerfile` and `build` (whole) cover the declaration of the built
/// packages (`build/image-crates.toml`) and the script that builds them
/// (`build/build-image-crates.py`): editing either changes the key.
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

/// Directories (repo-relative) of the packages in the dependency closure of one
/// build that are not fetched from a registry or git: the workspace members it
/// names and depends on, and every other path package they pull in. Normal and
/// build dependencies for Linux x86_64 with the build's features count; dev
/// dependencies are not built. A local package outside the checkout cannot be
/// hashed from it and is an error. Registry and git packages are pinned by
/// `Cargo.lock`, which is hashed whole.
fn closure_of(
    root: &Path,
    metadata: &serde_json::Value,
    build: &CargoBuild,
) -> Result<BTreeSet<String>> {
    let mut directory = BTreeMap::new();
    let mut name = BTreeMap::new();
    let mut local = BTreeSet::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        let (Some(id), Some(package_name), Some(manifest)) = (
            package["id"].as_str(),
            package["name"].as_str(),
            package["manifest_path"].as_str(),
        ) else {
            continue;
        };
        name.insert(id, package_name);
        // A package with no source is a path package (a workspace member or a
        // `path =` dependency); registry and git packages name theirs.
        if package["source"].is_null() {
            local.insert(id);
        }
        if let Ok(manifest) = std::fs::canonicalize(manifest)
            && let Some(dir) = manifest.parent().and_then(|d| d.strip_prefix(root).ok())
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
    for node in metadata["resolve"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let Some(id) = node["id"].as_str() else {
            continue;
        };
        let built = node["deps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|dep| {
                dep["dep_kinds"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|kind| {
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
        if local.contains(id) || members.contains(id) {
            let dir = directory.get(id).cloned().with_context(|| {
                format!(
                    "local package {id} is outside the checkout; its source cannot be fingerprinted"
                )
            })?;
            ensure!(
                !dir.is_empty(),
                "local package {id} is the checkout root itself; hashing it would hash everything"
            );
            crates.insert(dir);
        }
        pending.extend(dependencies.get(id).into_iter().flatten().copied());
    }
    Ok(crates)
}

/// Set by the signing process: from then on nothing in it may start cargo.
static CARGO_FORBIDDEN: AtomicBool = AtomicBool::new(false);

/// The variables of the caller's environment that change the cfgs or the
/// target cargo resolves dependencies for, and so the closure
/// (`RUSTFLAGS` and `CARGO_ENCODED_RUSTFLAGS` with a `--cfg` enable `cfg(...)`
/// path dependencies). The image build never sees them (its script starts
/// cargo with a scrubbed environment), so the derivation must not either, or
/// a developer's shell would hash another closure than CI's.
fn changes_resolution(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.ends_with("RUSTFLAGS") || upper.ends_with("RUSTDOCFLAGS") || upper == "CARGO_BUILD_TARGET"
}

/// A cargo command in `root` with every variable of `names` that
/// [`changes_resolution`] removed from its environment.
fn scrubbed_cargo(root: &Path, names: impl IntoIterator<Item = OsString>) -> Command {
    let mut command = Command::new("cargo");
    command.current_dir(root);
    for name in names {
        if name.to_str().is_some_and(changes_resolution) {
            command.env_remove(name);
        }
    }
    command
}

/// The only place that creates a cargo command, so the signing process's ban
/// holds for every caller, now and later.
fn cargo(root: &Path) -> Result<Command> {
    ensure!(
        !CARGO_FORBIDDEN.load(Ordering::SeqCst),
        "internal error: this process holds signing credentials and must not run cargo"
    );
    Ok(scrubbed_cargo(
        root,
        std::env::vars_os().map(|(name, _)| name),
    ))
}

/// The closure of one build, from `cargo metadata`. Only this function and
/// [`fetch_dependencies`] start cargo, and both refuse once
/// [`use_recorded_image_crates`] has run.
fn closure(ws: &Workspace, build: &CargoBuild) -> Result<BTreeSet<String>> {
    ensure!(
        !CARGO_FORBIDDEN.load(Ordering::SeqCst),
        "internal error: this process holds signing credentials and must not run cargo; the root image's crates come from the recorded closure"
    );
    let mut metadata = cargo(&ws.root)?;
    metadata.args([
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--offline",
        "--filter-platform",
        "x86_64-unknown-linux-gnu",
    ]);
    if build.no_default_features {
        metadata.arg("--no-default-features");
    }
    let output = cmd::output(&mut metadata).context(
        "deriving the root image's crates with `cargo metadata --offline --locked` (run `cargo fetch --locked` first)",
    )?;
    let metadata: serde_json::Value =
        serde_json::from_str(&output).context("parsing `cargo metadata`")?;
    closure_of(&std::fs::canonicalize(&ws.root)?, &metadata, build)
}

/// The one step that may reach the network: download what `cargo metadata
/// --offline` needs, before it runs.
fn fetch_dependencies(ws: &Workspace) -> Result<()> {
    ensure!(
        !CARGO_FORBIDDEN.load(Ordering::SeqCst),
        "internal error: this process holds signing credentials and must not run cargo"
    );
    cmd::run(cargo(&ws.root)?.args(["fetch", "--locked", "--target", "x86_64-unknown-linux-gnu"]))
        .context("fetching crates with `cargo fetch --locked`")
}

type Cache = Mutex<BTreeMap<PathBuf, Vec<String>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// The crates whose source can change a binary of the root image: the
/// dependency closure, derived from `cargo metadata`, of every package
/// `build/image-crates.toml` declares (the file the Containerfile's build
/// script builds from; the Containerfile, the script and `build/` are checked
/// by [`crate::image_crates_guard`] to build nothing else). An edit to any other crate (the installer, the e2e harness,
/// the development tooling) cannot reach the image and no longer changes its
/// fingerprint.
///
/// Runs cargo, so it is for keyless processes. The signing process reads the
/// closure a keyless step recorded ([`image_crates_record`],
/// [`use_recorded_image_crates`]).
pub fn image_crates(ws: &Workspace) -> Result<Vec<String>> {
    let cached = cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("crate closure cache poisoned"))?
        .get(&ws.root)
        .cloned();
    if let Some(crates) = cached {
        return Ok(crates);
    }
    let crates = derive_image_crates(ws)?;
    eprintln!("  root image crates: {}", crates.join(" "));
    cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("crate closure cache poisoned"))?
        .insert(ws.root.clone(), crates.clone());
    Ok(crates)
}

fn derive_image_crates(ws: &Workspace) -> Result<Vec<String>> {
    ensure!(
        !CARGO_FORBIDDEN.load(Ordering::SeqCst),
        "internal error: this process holds signing credentials and does not derive the root image's crates (that runs the build script and cargo); they come from the recorded closure"
    );
    let builds = derived_image_builds(&ws.root)?;
    fetch_dependencies(ws)?;
    let mut crates = BTreeSet::new();
    for build in builds {
        crates.extend(closure(ws, &build)?);
    }
    Ok(crates.into_iter().collect())
}

const RECORD_HEADER: &str = "ilex-image-crates v1";

/// The root image's crate closure as a file for a process that must not run
/// cargo, bound to the checkout's source digest: derived by a keyless step
/// (`ilexctl info --image-crates`), read by the signing publisher.
pub fn image_crates_record(ws: &Workspace) -> Result<String> {
    let crates = derive_image_crates(ws)?;
    let mut record = format!(
        "{RECORD_HEADER}\nsource {}\n",
        crate::source::manifest(ws)?.digest
    );
    for path in &crates {
        record.push_str(&format!("crate {path}\n"));
    }
    Ok(record)
}

fn parse_record(root: &Path, current_source: &str, record: &str) -> Result<Vec<String>> {
    let mut lines = record.lines();
    ensure!(
        lines.next() == Some(RECORD_HEADER),
        "not an {RECORD_HEADER} record"
    );
    let source = lines
        .next()
        .and_then(|line| line.strip_prefix("source "))
        .context("record names no source digest")?;
    ensure!(
        source == current_source,
        "the recorded closure was derived from source {source}, but this checkout is {current_source}; derive it again in the keyless step"
    );
    let mut crates = Vec::new();
    for line in lines {
        let path = line
            .strip_prefix("crate ")
            .with_context(|| format!("unexpected record line {line:?}"))?;
        let safe = !path.is_empty()
            && !path.starts_with('/')
            && !path.contains(['\\', '\0'])
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != "..");
        ensure!(
            safe,
            "recorded crate path {path:?} is not a checkout-relative path"
        );
        ensure!(
            root.join(path).is_dir(),
            "recorded crate {path} is not a directory of this checkout"
        );
        crates.push(path.to_owned());
    }
    ensure!(!crates.is_empty(), "the recorded closure names no crate");
    ensure!(
        crates.windows(2).all(|pair| pair[0] < pair[1]),
        "the recorded closure is not sorted and unique"
    );
    Ok(crates)
}

/// The signing process's way to the closure: read what the keyless step
/// recorded at `path`, check it belongs to this exact checkout, seed
/// [`image_crates`] with it and forbid every later cargo invocation in this
/// process. Prints the line CI logs as evidence.
pub fn use_recorded_image_crates(ws: &Workspace, path: &Path) -> Result<()> {
    let record = std::fs::read_to_string(path)
        .with_context(|| format!("reading the recorded image crates {}", path.display()))?;
    let source = crate::source::manifest(ws)?.digest;
    let crates = parse_record(&std::fs::canonicalize(&ws.root)?, &source, &record)?;
    CARGO_FORBIDDEN.store(true, Ordering::SeqCst);
    eprintln!(
        "image crates (signing process): {} crates read from {} for source {source}; cargo, rustc and kache are not run by this process",
        crates.len(),
        path.display()
    );
    cache()
        .lock()
        .map_err(|_| anyhow::anyhow!("crate closure cache poisoned"))?
        .insert(ws.root.clone(), crates);
    Ok(())
}

fn build(packages: &[&str], no_default_features: bool) -> CargoBuild {
    CargoBuild {
        packages: packages.iter().map(|p| (*p).to_owned()).collect(),
        no_default_features,
    }
}

/// The variables `scrubbed_cargo` removes for a typical environment: the
/// closure derivation must resolve for the default cfgs and target, as the
/// image build does.
fn check_scrubbed_cargo(root: &Path) -> Result<usize> {
    let names = [
        "RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "rustflags",
        // Left alone: cargo needs them, and they do not change resolution.
        "PATH",
        "HOME",
        "CARGO_HOME",
        "CARGO_NET_OFFLINE",
        "RUSTC_WRAPPER",
        "RUSTUP_TOOLCHAIN",
    ];
    let command = scrubbed_cargo(root, names.iter().map(|name| OsString::from(*name)));
    let removed: BTreeSet<String> = command
        .get_envs()
        .filter(|(_, value)| value.is_none())
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    let want: BTreeSet<String> = names[..7].iter().map(|name| (*name).to_owned()).collect();
    ensure!(
        removed == want,
        "the closure derivation scrubs {removed:?} from cargo's environment; it must scrub exactly {want:?}"
    );
    Ok(removed.len())
}

const SELF_CHECK_SOURCE: &str = "sha256:selfcheck";

fn fixture_metadata(tmp: &Path, extra_member: Option<&str>) -> serde_json::Value {
    use serde_json::json;
    let manifest = |dir: &str| {
        tmp.join(dir)
            .join("Cargo.toml")
            .to_string_lossy()
            .into_owned()
    };
    let package = |name: &str, dir: &str, source: Option<&str>| json!({"id": name, "name": name, "manifest_path": manifest(dir), "source": source});
    let dep = |pkg: &str, kind: Option<&str>| json!({"pkg": pkg, "dep_kinds": [{"kind": kind}]});
    let mut packages = vec![
        package("app", "root/crates/app", None),
        package("lib", "root/crates/lib", None),
        package("devonly", "root/crates/devonly", None),
        package("shim", "root/vendor/shim", None),
        package(
            "reg",
            "registry/reg",
            Some("registry+https://example.invalid/index"),
        ),
        package(
            "gitdep",
            "registry/gitdep",
            Some("git+https://example.invalid/x#abc"),
        ),
    ];
    let mut app_deps = vec![
        dep("lib", None),
        dep("shim", Some("build")),
        dep("devonly", Some("dev")),
        dep("reg", None),
        dep("gitdep", None),
    ];
    let mut members = vec!["app", "lib", "devonly"];
    if let Some(extra) = extra_member {
        packages.push(package(
            "extra",
            // A path package outside the checkout, or one that is the
            // checkout's own manifest.
            if extra == "outside" {
                "outside/ext"
            } else {
                "root"
            },
            None,
        ));
        app_deps.push(dep("extra", None));
        members.push("extra");
    }
    json!({
        "packages": packages,
        "workspace_members": members,
        "resolve": {"nodes": [{"id": "app", "deps": app_deps}]},
    })
}

/// Negative and positive fixtures for the root image's Rust build guards
/// ([`crate::image_crates_guard`]), the closure walk (local path packages,
/// registry/git packages, dev edges), the derivation's environment scrub and
/// the record the signing process reads. Run by `ilexctl doctor --self-check`;
/// no unit tests, as everywhere in this repository.
pub fn self_check(ws: &Workspace) -> Result<serde_json::Value> {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    std::fs::create_dir_all(std::env::temp_dir())?;
    let tmp = Scratch(std::env::temp_dir().join(format!(
        "ilex-fingerprint-selfcheck-{}-{unique}",
        std::process::id()
    )));
    std::fs::create_dir_all(&tmp.0)?;
    let base = std::fs::canonicalize(&tmp.0)?;
    let (_, mut report) = image_crates_guard::self_check(&ws.root, &base)?;
    let scrubbed = check_scrubbed_cargo(&ws.root)?;
    for dir in [
        "root/crates/app",
        "root/crates/lib",
        "root/crates/devonly",
        "root/vendor/shim",
        "registry/reg",
        "registry/gitdep",
        "outside/ext",
    ] {
        std::fs::create_dir_all(tmp.0.join(dir))?;
        std::fs::write(tmp.0.join(dir).join("Cargo.toml"), "")?;
    }
    std::fs::write(tmp.0.join("root/Cargo.toml"), "")?;
    let root = base.join("root");
    let wanted = build(&["app"], false);
    let ok = closure_of(&root, &fixture_metadata(&base, None), &wanted)?;
    ensure!(
        ok.iter().map(String::as_str).collect::<Vec<_>>()
            == ["crates/app", "crates/lib", "vendor/shim"],
        "closure fixture: got {ok:?}; want the member, its member dependency and the path package, without the dev dependency, registry or git packages"
    );
    for (extra, want) in [
        ("outside", "outside the checkout"),
        ("root", "checkout root itself"),
    ] {
        match closure_of(&root, &fixture_metadata(&base, Some(extra)), &wanted) {
            Err(error) if format!("{error:#}").contains(want) => {}
            other => {
                bail!("closure fixture `{extra}`: want an error containing `{want}`, got {other:?}")
            }
        }
    }
    ensure!(
        closure_of(
            &root,
            &fixture_metadata(&base, None),
            &build(&["nothere"], false)
        )
        .is_err(),
        "closure fixture: a package that is not a workspace member must be refused"
    );

    let good = format!(
        "{RECORD_HEADER}\nsource {SELF_CHECK_SOURCE}\ncrate crates/app\ncrate crates/lib\n"
    );
    ensure!(
        parse_record(&root, SELF_CHECK_SOURCE, &good)? == ["crates/app", "crates/lib"],
        "record fixture: a valid record must read back"
    );
    let mut records = 0;
    for (name, record) in [
        ("other header", good.replace("v1", "v2")),
        ("other source", good.replace("selfcheck", "other")),
        ("no source", format!("{RECORD_HEADER}\ncrate crates/app\n")),
        ("parent directory", good.replace("crates/lib", "../x")),
        ("absolute", good.replace("crates/lib", "/etc")),
        ("backslash", good.replace("crates/lib", "crates\\lib")),
        (
            "not a directory",
            good.replace("crates/lib", "crates/missing"),
        ),
        (
            "unsorted",
            "ilex-image-crates v1\nsource sha256:selfcheck\ncrate crates/lib\ncrate crates/app\n"
                .to_owned(),
        ),
        (
            "empty",
            format!("{RECORD_HEADER}\nsource {SELF_CHECK_SOURCE}\n"),
        ),
        ("junk line", format!("{good}cargo metadata\n")),
    ] {
        ensure!(
            parse_record(&root, SELF_CHECK_SOURCE, &record).is_err(),
            "record fixture `{name}` must be refused"
        );
        records += 1;
    }
    report["derivation_scrubs_variables"] = scrubbed.into();
    report["closure_fixtures"] = 4.into();
    report["record_fixtures_refused"] = records.into();
    Ok(report)
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
