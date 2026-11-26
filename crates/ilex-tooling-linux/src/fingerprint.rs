//! Shared fingerprint/cache key inputs. Preserve phase 0's publication rules.
use crate::{Workspace, cmd, config::Selection, git};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
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

/// The declaration of what the root image builds, and the script that builds it
/// from that declaration. The Containerfile runs the script and nothing else of
/// the Rust toolchain, the fingerprint reads the same declaration, and the
/// guard ([`check_containerfile`], [`check_build_script`]) fails when the
/// Containerfile or the script names the toolchain anywhere else: one list, no
/// parsing of shell.
const DECLARATION: &str = "build/image-crates.toml";
const BUILD_SCRIPT: &str = "build/build-image-crates.py";

/// What one `cargo build` of the root image asks for: one `[[build]]` of the
/// declaration.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct CargoBuild {
    packages: Vec<String>,
    #[serde(default)]
    no_default_features: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Declaration {
    build: Vec<CargoBuild>,
}

/// The same rule `build-image-crates.py` applies: a plain package name, never
/// a flag, a path or a word with a space, quote or separator.
fn plain_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The `[[build]]` entries of the declaration, in file order.
fn parse_declaration(text: &str) -> Result<Vec<CargoBuild>> {
    let declaration: Declaration = toml::from_str(text).context("parsing the declaration")?;
    ensure!(
        !declaration.build.is_empty(),
        "the declaration has no [[build]]"
    );
    for build in &declaration.build {
        ensure!(
            !build.packages.is_empty(),
            "a [[build]] names no package; its `packages` must be a non-empty list"
        );
        for package in &build.packages {
            ensure!(
                plain_package_name(package),
                "`{package}` is not a plain package name"
            );
        }
    }
    Ok(declaration.build)
}

/// Logical lines of a Containerfile with their first physical line: `\`
/// continuations are joined (comment lines inside one are dropped, as the
/// builders do) and full-line comments are removed.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        let (body, continued) = match trimmed.strip_suffix('\\') {
            Some(body) => (body.trim_end(), true),
            None => (trimmed, false),
        };
        match current.as_mut() {
            Some((_, line)) => {
                line.push(' ');
                line.push_str(body);
            }
            None => current = Some((index + 1, body.to_owned())),
        }
        if !continued && let Some(done) = current.take() {
            lines.push(done);
        }
    }
    lines.extend(current);
    lines
}

/// The words that start the Rust toolchain or name its environment
/// (`CARGO_*`, `RUSTC_WRAPPER`, `RUSTUP_TOOLCHAIN`, `RUSTDOCFLAGS`), matched
/// as case-insensitive substrings. A spelling that never writes one of them
/// is not detected: this finds drift, not an adversary.
const TOOLCHAIN_WORDS: [&str; 4] = ["cargo", "rustc", "rustup", "rustdoc"];

fn mentions_toolchain(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    TOOLCHAIN_WORDS.iter().any(|word| lower.contains(word))
}

/// A word that cannot start or hide anything: no quote, `$`, backtick, `;`,
/// `&`, `|`, `<`, `>`, bracket, brace, parenthesis or backslash.
fn plain_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_./:=+@,*-".contains(c))
}

/// The one line of the Containerfile that runs the declared builds, apart from
/// its `--mount=…` flags.
const BUILD_COMMAND: [&str; 5] = ["python3", "-I", "-S", BUILD_SCRIPT, DECLARATION];

#[derive(Debug, PartialEq, Eq)]
enum Mention {
    /// `COPY`/`ADD` of plain words: a file name, nothing runs.
    Copy,
    /// `RUN dnf -y install` of plain words: the package is installed, not run.
    Install,
    /// The single invocation of [`BUILD_SCRIPT`] on [`DECLARATION`].
    Build,
}

/// What a logical line of the Containerfile is, when it is one of the three
/// shapes that may name the toolchain. Every shape is plain words only, so it
/// is as inert inside a heredoc body or any other context as in a `RUN`.
fn classify(line: &str) -> Option<Mention> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let (instruction, rest) = words.split_first()?;
    if instruction.eq_ignore_ascii_case("COPY") || instruction.eq_ignore_ascii_case("ADD") {
        return rest
            .iter()
            .all(|word| plain_word(word))
            .then_some(Mention::Copy);
    }
    if !instruction.eq_ignore_ascii_case("RUN") {
        return None;
    }
    let flags = rest
        .iter()
        .take_while(|word| word.starts_with("--mount="))
        .count();
    let (flags, command) = rest.split_at(flags);
    if !flags.iter().all(|word| plain_word(word)) {
        return None;
    }
    if command == BUILD_COMMAND {
        return Some(Mention::Build);
    }
    (command.starts_with(&["dnf", "-y", "install"]) && command.iter().all(|word| plain_word(word)))
        .then_some(Mention::Install)
}

/// Fails unless the Containerfile runs the Rust toolchain only through
/// [`BUILD_SCRIPT`]: every logical line (`\` continuations joined, full-line
/// comments dropped, every stage) that names `cargo`, `rustc`, `rustup` or
/// `rustdoc` must be a `COPY`/`ADD` of plain words, a `dnf -y install` of
/// plain words (the toolchain's own package) or the one invocation of the
/// script; any line that names the script must be exactly that invocation, and
/// there must be exactly one. A heredoc body, an exec-form `RUN`, `ENV`/`ARG`,
/// a substitution, a redirection, a chain: none of them is one of the shapes,
/// so each is refused without being understood.
fn check_containerfile(text: &str) -> Result<()> {
    let mut invocations = 0;
    for (number, line) in logical_lines(text) {
        let mention = classify(&line);
        if mention == Some(Mention::Build) {
            invocations += 1;
        }
        ensure!(
            mention.is_some()
                || !(mentions_toolchain(&line) || line.contains("build-image-crates")),
            "Containerfile line {number}: `{line}` names cargo, rustc, rustup, rustdoc or {BUILD_SCRIPT} outside the toolchain's `dnf -y install`, a COPY and the one `RUN [--mount=…] {}`; declare the package in {DECLARATION} and let the script build it",
            BUILD_COMMAND.join(" ")
        );
    }
    ensure!(
        invocations == 1,
        "the Containerfile must run `RUN [--mount=…] {}` exactly once, found {invocations}",
        BUILD_COMMAND.join(" ")
    );
    Ok(())
}

/// The only lines of the build script that name the toolchain, besides
/// comments: the wrapper every compile goes through and the one build command.
const SCRIPT_TOOLCHAIN_LINES: [&str; 2] = [
    r#"env = dict(os.environ, RUSTC_WRAPPER="kache")"#,
    r#"command = ["cargo", "build", "--release", "--locked"]"#,
];

/// Fails unless the script names the toolchain on those two lines only (each
/// exactly once), so a second build hidden in it cannot escape the
/// declaration.
fn check_build_script(text: &str) -> Result<()> {
    let mut seen = [0usize; SCRIPT_TOOLCHAIN_LINES.len()];
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || !mentions_toolchain(line) {
            continue;
        }
        match SCRIPT_TOOLCHAIN_LINES.iter().position(|want| *want == line) {
            Some(position) => seen[position] += 1,
            None => bail!(
                "{BUILD_SCRIPT} line {}: `{line}` names cargo, rustc, rustup or rustdoc; only the wrapper line and the one build command may, so that it builds exactly what {DECLARATION} declares",
                index + 1
            ),
        }
    }
    ensure!(
        seen.iter().all(|count| *count == 1),
        "{BUILD_SCRIPT} must contain each of {SCRIPT_TOOLCHAIN_LINES:?} exactly once (found {seen:?})"
    );
    Ok(())
}

/// The packages the root image builds: the declaration, after the guard has
/// checked that the Containerfile and the script build nothing else.
fn image_builds(root: &Path) -> Result<Vec<CargoBuild>> {
    let read = |path: &str| {
        std::fs::read_to_string(root.join(path)).with_context(|| format!("reading {path}"))
    };
    check_containerfile(&read("Containerfile")?).context("the root Containerfile")?;
    check_build_script(&read(BUILD_SCRIPT)?).with_context(|| BUILD_SCRIPT.to_owned())?;
    parse_declaration(&read(DECLARATION)?).with_context(|| DECLARATION.to_owned())
}

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

/// The closure of one build, from `cargo metadata`. Only this function and
/// [`fetch_dependencies`] start cargo, and both refuse once
/// [`use_recorded_image_crates`] has run.
fn closure(ws: &Workspace, build: &CargoBuild) -> Result<BTreeSet<String>> {
    ensure!(
        !CARGO_FORBIDDEN.load(Ordering::SeqCst),
        "internal error: this process holds signing credentials and must not run cargo; the root image's crates come from the recorded closure"
    );
    let mut metadata = Command::new("cargo");
    metadata
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--offline",
            "--filter-platform",
            "x86_64-unknown-linux-gnu",
        ])
        .current_dir(&ws.root);
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
    cmd::run(
        Command::new("cargo")
            .args(["fetch", "--locked", "--target", "x86_64-unknown-linux-gnu"])
            .current_dir(&ws.root),
    )
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
/// script builds from; the Containerfile and the script are checked to build
/// nothing else). An edit to any other crate (the installer, the e2e harness,
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
    let builds = image_builds(&ws.root)?;
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

/// A Containerfile in the shape of the real one: the toolchain's `dnf` install,
/// a `COPY` of the manifests and the one `RUN` of the build script, in a stage
/// of its own, with comments that name the toolchain.
const GOOD_CONTAINERFILE: &str = r#"
# Build with `cargo ilexctl build`; rustup configures the host, not this stage.
FROM quay.io/fedora/fedora:45 AS build
RUN --mount=type=cache,id=x-dnf,target=/var/cache/libdnf5,sharing=locked \
    dnf -y install --setopt=keepcache=1 cargo gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY build/build-image-crates.py build/image-crates.toml build/
RUN --mount=type=cache,id=x-cargo,target=/root/.cargo/registry \
    --mount=type=cache,id=x-kache,target=/root/.cache/kache,sharing=locked \
    python3 -I -S build/build-image-crates.py build/image-crates.toml

FROM quay.io/fedora/fedora-bootc:45
COPY --from=build /out/ /
RUN echo "${SUM}  x" | sha256sum -c -
"#;

/// `(name, Containerfile, None to accept or Some(text) the refusal must
/// contain)`. Every bypass the reviews found in the old shell parser, and the
/// plain ways to run the toolchain, are refused by the one rule.
type GuardFixture = (&'static str, String, Option<&'static str>);

fn containerfile_fixtures() -> Vec<GuardFixture> {
    const OUTSIDE: Option<&str> = Some("outside the toolchain's `dnf -y install`");
    let with = |extra: &str| format!("{GOOD_CONTAINERFILE}{extra}");
    let replacing = |from: &str, to: &str| GOOD_CONTAINERFILE.replacen(from, to, 1);
    const INVOCATION: &str = "python3 -I -S build/build-image-crates.py build/image-crates.toml";
    vec![
        ("the shape of the real Containerfile", with(""), None),
        (
            "more toolchain installs, copies of the binary and no mention at all",
            with(
                "RUN dnf -y install rustup\nCOPY --from=build /usr/bin/cargo /usr/local/bin/cargo\nRUN echo \"$(python3 -c 'print(1)')\" && true\n",
            ),
            None,
        ),
        (
            "a second `cargo build`",
            with("RUN cargo build --release --locked -p evil\n"),
            OUTSIDE,
        ),
        (
            "another stage runs it",
            with("FROM quay.io/fedora/fedora:45 AS other\nRUN cargo install --locked evil\n"),
            OUTSIDE,
        ),
        (
            "process substitution in a redirection",
            with("RUN cargo build -p ilexctl 2> >(tee build.log) --workspace\n"),
            OUTSIDE,
        ),
        (
            "here-string",
            with("RUN sh <<< 'cargo build -p evil'\n"),
            OUTSIDE,
        ),
        (
            "heredoc body",
            with("RUN <<EOF\ncargo build -p evil\nEOF\n"),
            OUTSIDE,
        ),
        (
            "heredoc body that starts like an instruction",
            with("RUN <<EOF\nCOPY x; cargo build -p evil\nEOF\n"),
            OUTSIDE,
        ),
        (
            "heredoc body that starts like the toolchain install",
            with("RUN <<EOF\nRUN dnf -y install x; cargo build -p evil\nEOF\n"),
            OUTSIDE,
        ),
        (
            "substitution nested in a parameter expansion",
            with("RUN echo \"${x:-$(echo $((1 + 2)); cargo build -p evil)}\"\n"),
            OUTSIDE,
        ),
        (
            "substitution with a closing parenthesis inside (arithmetic)",
            with("RUN echo \"$(( (1 + 2) ))$(cargo build -p evil)\"\n"),
            OUTSIDE,
        ),
        (
            "substitution with a closing parenthesis inside (subshell)",
            with("RUN echo \"$( (cargo build -p evil) )\"\n"),
            OUTSIDE,
        ),
        (
            "substitution with a closing parenthesis inside (case)",
            with("RUN echo \"$(case x in x) cargo build -p evil;; esac)\"\n"),
            OUTSIDE,
        ),
        (
            "an appended environment variable",
            with("RUN CARGO_BUILD_TARGET+=-musl true\n"),
            OUTSIDE,
        ),
        (
            "a quoted export",
            with("RUN export \"CARGO_BUILD_TARGET=x86_64-unknown-linux-musl\"\n"),
            OUTSIDE,
        ),
        (
            "ENV",
            with("ENV CARGO_BUILD_TARGET=x86_64-unknown-linux-musl\n"),
            OUTSIDE,
        ),
        ("ARG", with("ARG CARGO_BUILD_JOBS=1\n"), OUTSIDE),
        (
            "the wrapper variable",
            with("ENV RUSTC_WRAPPER=/evil\n"),
            OUTSIDE,
        ),
        (
            "a toolchain selector",
            with("ENV RUSTUP_TOOLCHAIN=nightly\n"),
            OUTSIDE,
        ),
        (
            "exec form",
            with("RUN [\"/usr/bin/cargo\", \"build\", \"-p\", \"evil\"]\n"),
            OUTSIDE,
        ),
        (
            "a lower-case instruction",
            with("run cargo build -p evil\n"),
            OUTSIDE,
        ),
        (
            "ONBUILD",
            with("ONBUILD RUN cargo build -p evil\n"),
            OUTSIDE,
        ),
        (
            "a continuation",
            with("RUN true \\\n    && cargo build -p evil\n"),
            OUTSIDE,
        ),
        (
            "a trailing comment",
            with("RUN true # cargo build -p evil\n"),
            OUTSIDE,
        ),
        ("rustup", with("RUN rustup run nightly true\n"), OUTSIDE),
        (
            "the toolchain install chained with a build",
            with("RUN dnf -y install cargo && cargo build -p evil\n"),
            OUTSIDE,
        ),
        (
            "the toolchain install, then a command",
            with("RUN dnf -y install cargo; true\n"),
            OUTSIDE,
        ),
        (
            "the toolchain install with a substitution",
            with("RUN dnf -y install $(cargo build -p evil)\n"),
            OUTSIDE,
        ),
        (
            "a COPY with a command after it",
            with("COPY x /y; cargo build -p evil\n"),
            OUTSIDE,
        ),
        (
            "the script's own invocation, chained",
            replacing(INVOCATION, &format!("{INVOCATION} && cargo build -p evil")),
            OUTSIDE,
        ),
        (
            "the script on another declaration",
            replacing("build/image-crates.toml\n", "build/other.toml\n"),
            OUTSIDE,
        ),
        (
            "the script with another interpreter",
            replacing("python3 -I -S", "python3 -S"),
            OUTSIDE,
        ),
        (
            "the script run twice",
            with(&format!("RUN {INVOCATION}\n")),
            Some("exactly once, found 2"),
        ),
        (
            "the script not run at all",
            {
                let start = GOOD_CONTAINERFILE
                    .find("RUN --mount=type=cache,id=x-cargo")
                    .expect("the fixture's build step");
                let end = start
                    + GOOD_CONTAINERFILE[start..]
                        .find(INVOCATION)
                        .expect("the fixture's invocation")
                    + INVOCATION.len();
                format!(
                    "{}RUN true{}",
                    &GOOD_CONTAINERFILE[..start],
                    &GOOD_CONTAINERFILE[end..]
                )
            },
            Some("exactly once, found 0"),
        ),
    ]
}

/// `(name, build script, None to accept or Some(text) the refusal must
/// contain)`, derived from the real script so that it stays the one accepted.
fn script_fixtures(script: &str) -> Vec<(&'static str, String, Option<&'static str>)> {
    let wrapper = SCRIPT_TOOLCHAIN_LINES[0];
    let command = SCRIPT_TOOLCHAIN_LINES[1];
    vec![
        ("the real script", script.to_owned(), None),
        (
            "a comment that names the toolchain",
            format!("{script}# cargo build -p evil\n"),
            None,
        ),
        (
            "a second build",
            format!(
                "{script}subprocess.run([\"cargo\", \"build\", \"-p\", \"evil\"], check=True)\n"
            ),
            Some("names cargo, rustc, rustup or rustdoc"),
        ),
        (
            "a build with a trailing comment",
            script.replace(command, &format!("{command}  # -p evil")),
            Some("names cargo, rustc, rustup or rustdoc"),
        ),
        (
            "another wrapper",
            script.replace(wrapper, "env = dict(os.environ, RUSTC_WRAPPER=\"sccache\")"),
            Some("names cargo, rustc, rustup or rustdoc"),
        ),
        (
            "no wrapper",
            script.replace(wrapper, "env = dict(os.environ)"),
            Some("exactly once"),
        ),
        (
            "the build twice",
            script.replace(command, &format!("{command}\n        {command}")),
            Some("exactly once"),
        ),
        (
            "an environment variable",
            format!("{script}os.environ[\"CARGO_BUILD_TARGET\"] = \"x\"\n"),
            Some("names cargo, rustc, rustup or rustdoc"),
        ),
    ]
}

/// `(name, declaration, None to accept or Some(text) the refusal must
/// contain)`.
fn declaration_fixtures() -> Vec<(&'static str, &'static str, Option<&'static str>)> {
    vec![
        (
            "one build and flags",
            "[[build]]\npackages = [\"a\", \"b-c_d\"]\nno-default-features = true\n[[build]]\npackages = [\"e\"]\n",
            None,
        ),
        ("an empty file", "", Some("missing field")),
        ("no builds", "build = []\n", Some("no [[build]]")),
        (
            "no packages",
            "[[build]]\npackages = []\n",
            Some("no package"),
        ),
        (
            "packages missing",
            "[[build]]\nno-default-features = true\n",
            Some("packages"),
        ),
        (
            "an unknown key",
            "[[build]]\npackages = [\"a\"]\nfeatures = [\"x\"]\n",
            Some("unknown field"),
        ),
        (
            "an unknown table",
            "[[build]]\npackages = [\"a\"]\n[other]\nx = 1\n",
            Some("unknown field"),
        ),
        (
            "a flag as a package",
            "[[build]]\npackages = [\"--workspace\"]\n",
            Some("not a plain package name"),
        ),
        (
            "a package with a space",
            "[[build]]\npackages = [\"a --workspace\"]\n",
            Some("not a plain package name"),
        ),
        (
            "a package with a separator",
            "[[build]]\npackages = [\"a;b\"]\n",
            Some("not a plain package name"),
        ),
        (
            "a package that is a version spec",
            "[[build]]\npackages = [\"a@1\"]\n",
            Some("not a plain package name"),
        ),
        (
            "packages as a string",
            "[[build]]\npackages = \"a\"\n",
            Some("parsing the declaration"),
        ),
        (
            "a flag as a string",
            "[[build]]\npackages = [\"a\"]\nno-default-features = \"yes\"\n",
            Some("parsing the declaration"),
        ),
    ]
}

/// The script and the fingerprint read the declaration independently; they
/// must agree on it.
fn script_agrees_with_declaration(root: &Path, builds: &[CargoBuild]) -> Result<()> {
    let output = cmd::output(
        Command::new("python3")
            .args(["-I", "-S"])
            .arg(root.join(BUILD_SCRIPT))
            .arg("--list")
            .arg(root.join(DECLARATION)),
    )
    .context("running the build script with --list (python3 is required)")?;
    let listed: serde_json::Value =
        serde_json::from_str(&output).context("reading the build script's --list output")?;
    let want = serde_json::Value::Array(
        builds
            .iter()
            .map(|build| {
                serde_json::json!({
                    "packages": build.packages,
                    "no-default-features": build.no_default_features,
                })
            })
            .collect(),
    );
    ensure!(
        listed == want,
        "{BUILD_SCRIPT} reads {DECLARATION} as {listed}, the fingerprint as {want}"
    );
    Ok(())
}

/// Runs `check` over fixtures that must be accepted (`None`) or refused with
/// the given text, and counts both.
fn run_guard_fixtures<'a>(
    kind: &str,
    fixtures: impl IntoIterator<Item = (&'a str, Result<()>, Option<&'a str>)>,
) -> Result<(usize, usize)> {
    let (mut accepted, mut refused) = (0, 0);
    for (name, outcome, expect) in fixtures {
        match (outcome, expect) {
            (Ok(()), None) => accepted += 1,
            (Err(error), Some(want)) => {
                let error = format!("{error:#}");
                ensure!(
                    error.contains(want),
                    "{kind} fixture `{name}` failed with `{error}`, which does not contain `{want}`"
                );
                refused += 1;
            }
            (Ok(()), Some(want)) => {
                bail!("{kind} fixture `{name}` was accepted; it must be refused (`{want}`)")
            }
            (Err(error), None) => bail!("{kind} fixture `{name}` was refused: {error:#}"),
        }
    }
    Ok((accepted, refused))
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

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Negative and positive fixtures for the Containerfile and build-script guard,
/// the declaration, the closure walk (local path packages, registry/git
/// packages, dev edges) and the record the signing process reads. Run by
/// `ilexctl doctor --self-check`; no unit tests, as everywhere in this
/// repository.
pub fn self_check(ws: &Workspace) -> Result<serde_json::Value> {
    let (containerfile_accepted, containerfile_refused) = run_guard_fixtures(
        "Containerfile",
        containerfile_fixtures()
            .iter()
            .map(|(name, text, expect)| (*name, check_containerfile(text), *expect)),
    )?;
    let script = std::fs::read_to_string(ws.root.join(BUILD_SCRIPT))
        .with_context(|| format!("reading {BUILD_SCRIPT}"))?;
    let (script_accepted, script_refused) = run_guard_fixtures(
        "build script",
        script_fixtures(&script)
            .iter()
            .map(|(name, text, expect)| (*name, check_build_script(text), *expect)),
    )?;
    let (declaration_accepted, declaration_refused) = run_guard_fixtures(
        "declaration",
        declaration_fixtures()
            .iter()
            .map(|(name, text, expect)| (*name, parse_declaration(text).map(|_| ()), *expect)),
    )?;
    // The checkout's own files pass the guard, and the script reads its
    // declaration the way the fingerprint does.
    let real = image_builds(&ws.root)?;
    script_agrees_with_declaration(&ws.root, &real)?;

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    std::fs::create_dir_all(std::env::temp_dir())?;
    let tmp = Scratch(std::env::temp_dir().join(format!(
        "ilex-fingerprint-selfcheck-{}-{unique}",
        std::process::id()
    )));
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
    let base = std::fs::canonicalize(&tmp.0)?;
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
    Ok(serde_json::json!({
        "containerfile_fixtures_accepted": containerfile_accepted,
        "containerfile_fixtures_refused": containerfile_refused,
        "build_script_fixtures_accepted": script_accepted,
        "build_script_fixtures_refused": script_refused,
        "declaration_fixtures_accepted": declaration_accepted,
        "declaration_fixtures_refused": declaration_refused,
        "declared_builds": real.len(),
        "script_reads_the_declaration_as_the_fingerprint_does": true,
        "closure_fixtures": 4,
        "record_fixtures_refused": records,
    }))
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
