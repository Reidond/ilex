//! The root image's Rust build: what it builds, and the guards that fail when
//! anything in the Containerfile, the build script or `build/` could make it
//! build more than `build/image-crates.toml` declares. The fingerprint derives
//! its crate closure from the same declaration ([`crate::fingerprint`]).
use crate::{cmd, image::BUILDER_IMAGE};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The declaration of what the root image builds, and the script that builds it
/// from that declaration. The Containerfile's builder stage runs the script and
/// nothing else of the Rust toolchain, the fingerprint reads the same
/// declaration, and the guards ([`check_containerfile`], [`check_build_script`],
/// [`check_build_files`]) fail when anything else could start cargo or change
/// what it builds: one list, no parsing of shell.
const DECLARATION: &str = "build/image-crates.toml";
const BUILD_SCRIPT: &str = "build/build-image-crates.py";

/// What one `cargo build` of the root image asks for: one `[[build]]` of the
/// declaration.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub(crate) struct CargoBuild {
    pub(crate) packages: Vec<String>,
    #[serde(default)]
    pub(crate) no_default_features: bool,
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

/// The argv of every `cargo build` the declaration implies, and nothing else:
/// `--release --locked`, `-p` for each declared package, and
/// `--no-default-features` when declared. The script's `--commands` output must
/// equal this.
fn expected_commands(builds: &[CargoBuild]) -> Vec<Vec<String>> {
    builds
        .iter()
        .map(|build| {
            let mut command: Vec<String> = ["cargo", "build", "--release", "--locked"]
                .iter()
                .map(|word| (*word).to_owned())
                .collect();
            for package in &build.packages {
                command.extend(["-p".to_owned(), package.clone()]);
            }
            if build.no_default_features {
                command.push("--no-default-features".to_owned());
            }
            command
        })
        .collect()
}

/// Logical lines of a Containerfile with their first physical line: `\`
/// continuations are joined (comment lines inside one are dropped, as the
/// builders do) and full-line comments are removed.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut lines = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim();
        if trimmed.starts_with('#') || (trimmed.is_empty() && current.is_none()) {
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
/// (`CARGO_*`, `RUSTC_WRAPPER`, `RUSTUP_TOOLCHAIN`, `RUSTDOCFLAGS`), matched as
/// case-insensitive substrings; [`mentions_toolchain`] also matches `rust` at
/// the start of a word (`RUSTFLAGS`, `CARGO_BUILD_RUSTFLAGS`,
/// `rust-toolchain.toml`, but not `ca-trust`). A spelling that never writes one
/// of them is not detected: this finds drift, not an adversary.
const TOOLCHAIN_WORDS: [&str; 4] = ["cargo", "rustc", "rustup", "rustdoc"];

fn mentions_toolchain(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    TOOLCHAIN_WORDS.iter().any(|word| lower.contains(word))
        || lower.match_indices("rust").any(|(at, _)| {
            at == 0
                || lower
                    .as_bytes()
                    .get(at - 1)
                    .is_some_and(|before| !before.is_ascii_alphanumeric())
        })
}

fn names_declaration(text: &str) -> bool {
    text.to_ascii_lowercase().contains("image-crates")
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
/// its cache mounts.
const BUILD_COMMAND: [&str; 5] = ["python3", "-I", "-S", BUILD_SCRIPT, DECLARATION];

/// The lines, besides `FROM`, `ARG KACHE_*` and `RUN` of the shapes below, that
/// the stage which builds the root image's Rust packages may contain. `COPY`
/// is pinned, not a shape: a new or wider one could bring in a cargo
/// configuration, a `rust-toolchain` or a second declaration.
const BUILDER_STAGE_LINES: [&str; 4] = [
    "WORKDIR /src",
    "COPY Cargo.toml Cargo.lock ./",
    "COPY crates/ crates/",
    "COPY build/build-image-crates.py build/image-crates.toml build/",
];

/// The kache download and its sha256 check, as the one logical line the
/// Containerfile has (a version bump changes only its `ARG`s).
const KACHE_INSTALL: &str = concat!(
    "RUN curl -fsSL -o /tmp/kache.tar.gz ",
    "\"https://github.com/kunobi-ninja/kache/releases/download/v${KACHE_VERSION}/kache-x86_64-unknown-linux-musl.tar.gz\" && ",
    "echo \"${KACHE_SHA256}  /tmp/kache.tar.gz\" | sha256sum -c - && ",
    "tar -xzf /tmp/kache.tar.gz -C /usr/local/bin kache kache-engine && ",
    "rm /tmp/kache.tar.gz && ",
    "kache --version"
);

/// The only mount targets the builder stage's `RUN` lines may cache.
const BUILDER_MOUNT_TARGETS: [&str; 3] = [
    "/var/cache/libdnf5",
    "/root/.cargo/registry",
    "/root/.cache/kache",
];

/// A cache mount of one of [`BUILDER_MOUNT_TARGETS`]: never a bind mount,
/// which could lay a file over the declaration or a cargo configuration.
fn builder_cache_mount(flag: &str) -> bool {
    let Some(options) = flag.strip_prefix("--mount=") else {
        return false;
    };
    let parts: Vec<&str> = options.split(',').collect();
    parts.contains(&"type=cache")
        && parts.iter().all(|part| match part.split_once('=') {
            Some(("type", "cache")) | Some(("sharing", "locked")) => true,
            Some(("id", id)) => plain_word(id),
            Some(("target", target)) => BUILDER_MOUNT_TARGETS.contains(&target),
            _ => false,
        })
}

/// What a `RUN` of the builder stage is, when it is one of its three allowed
/// steps.
#[derive(Debug, PartialEq, Eq)]
enum BuilderRun {
    /// `RUN [--mount=type=cache…] dnf -y install` of plain words: the
    /// toolchain's own package.
    Install,
    /// The pinned kache download.
    Kache,
    /// The single invocation of [`BUILD_SCRIPT`] on [`DECLARATION`].
    Build,
}

fn instruction_upper(line: &str) -> String {
    match line.split_once(char::is_whitespace) {
        Some((first, rest)) => format!("{} {rest}", first.to_ascii_uppercase()),
        None => line.to_ascii_uppercase(),
    }
}

fn builder_run(line: &str) -> Option<BuilderRun> {
    let line = instruction_upper(line);
    if line == KACHE_INSTALL {
        return Some(BuilderRun::Kache);
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let rest = words.strip_prefix(&["RUN"])?;
    let mounts = rest
        .iter()
        .take_while(|word| word.starts_with("--mount="))
        .count();
    let (mounts, command) = rest.split_at(mounts);
    if !mounts.iter().all(|word| builder_cache_mount(word)) {
        return None;
    }
    if command == BUILD_COMMAND {
        return Some(BuilderRun::Build);
    }
    (command.starts_with(&["dnf", "-y", "install"]) && command.iter().all(|word| plain_word(word)))
        .then_some(BuilderRun::Install)
}

/// Whether a logical line may be in the builder stage. `FROM` must be the
/// builder image the root key hashes, `ARG` only `KACHE_*` (the pinned
/// download's version and checksum).
fn allowed_in_builder_stage(line: &str) -> bool {
    let upper = instruction_upper(line);
    if BUILDER_STAGE_LINES.contains(&upper.as_str()) || builder_run(line).is_some() {
        return true;
    }
    let words: Vec<&str> = upper.split_whitespace().collect();
    match words.as_slice() {
        ["FROM", image] => *image == BUILDER_IMAGE,
        ["FROM", image, keyword, name] => {
            *image == BUILDER_IMAGE && keyword.eq_ignore_ascii_case("AS") && plain_word(name)
        }
        ["ARG", name] => name.starts_with("KACHE_") && plain_word(name),
        _ => false,
    }
}

/// One stage of a Containerfile: the line of its `FROM` and its logical lines
/// (the lines before the first `FROM` form a stage of their own).
struct Stage {
    start: usize,
    lines: Vec<(usize, String)>,
}

fn stages(lines: Vec<(usize, String)>) -> Vec<Stage> {
    let mut stages: Vec<Stage> = Vec::new();
    for (number, line) in lines {
        let starts = line
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("FROM"));
        if starts || stages.is_empty() {
            stages.push(Stage {
                start: number,
                lines: Vec::new(),
            });
        }
        if let Some(stage) = stages.last_mut() {
            stage.lines.push((number, line));
        }
    }
    stages
}

/// Fails unless the Rust toolchain is installed and used in one stage that
/// has nothing else in it. A stage **builds the root image's Rust packages**
/// when any of its logical lines (`\` continuations joined, full-line comments
/// dropped) names `cargo`, `rustc`, `rustup`, `rustdoc` or a word starting
/// with `rust` (so `RUSTFLAGS` too), or names `image-crates`. Every line of
/// such a stage must be one of: its `FROM` of [`BUILDER_IMAGE`], `ARG
/// KACHE_*`, [`BUILDER_STAGE_LINES`] (a `WORKDIR` and three pinned `COPY`s),
/// the toolchain's `RUN [--mount=type=cache…] dnf -y install …`, the pinned
/// kache download, or the one `RUN [--mount=type=cache…] python3 -I -S
/// build/build-image-crates.py build/image-crates.toml`, which must occur
/// exactly once. A new `RUN`, `ENV`, `COPY`, `ADD` or a mount in that stage, a
/// mention of the toolchain or the declaration in any other stage, a heredoc
/// body, an exec-form `RUN`: none of them is allowed, so each is refused
/// without being understood.
fn check_containerfile(text: &str) -> Result<()> {
    let mut invocations = 0;
    for stage in stages(logical_lines(text)) {
        let builder = stage
            .lines
            .iter()
            .any(|(_, line)| mentions_toolchain(line) || names_declaration(line));
        if !builder {
            continue;
        }
        // Name the line that brought the stage here (the toolchain or the
        // declaration) rather than its `FROM`, when there is one.
        let rejected: Vec<&(usize, String)> = stage
            .lines
            .iter()
            .filter(|(_, line)| !allowed_in_builder_stage(line))
            .collect();
        if let Some((number, line)) = rejected
            .iter()
            .copied()
            .find(|(_, line)| mentions_toolchain(line) || names_declaration(line))
            .or(rejected.first().copied())
        {
            bail!(
                "Containerfile line {number}: `{line}` is not allowed in the stage that builds the Rust packages (from line {}; it names cargo, rust* or image-crates). That stage may contain only its `FROM {BUILDER_IMAGE}`, `ARG KACHE_*`, `{}`, the toolchain's `RUN [--mount=type=cache…] dnf -y install …`, the pinned kache download and the one `RUN [--mount=type=cache…] {}`: declare the package in {DECLARATION} and let the script build it",
                stage.start,
                BUILDER_STAGE_LINES.join("`, `"),
                BUILD_COMMAND.join(" ")
            );
        }
        invocations += stage
            .lines
            .iter()
            .filter(|(_, line)| builder_run(line) == Some(BuilderRun::Build))
            .count();
    }
    ensure!(
        invocations == 1,
        "the Containerfile must run `RUN [--mount=…] {}` exactly once, found {invocations}",
        BUILD_COMMAND.join(" ")
    );
    Ok(())
}

/// The build script's execution part, pinned verbatim: from the line below to
/// the end of the file. Whatever builds, stages or runs a program is in it, so
/// an edit there is an edit of this file too (comments and blank lines aside).
const PINNED_SCRIPT_TAIL: &str = include_str!("build-image-crates.pinned.py");
const SCRIPT_TAIL_START: &str = "def cargo_commands(builds):";

/// The only imports of the script before its pinned part: no `runpy`,
/// `importlib`, `ctypes`, `pty` or `shlex`, nothing that loads code or reaches
/// a shell.
const SCRIPT_IMPORTS: [&str; 6] = [
    "import json",
    "import os",
    "import re",
    "import subprocess",
    "import sys",
    "import tomllib",
];

/// What the script's part before the pinned one may not contain: another way
/// to run a program (`subprocess` itself is imported for the pinned part
/// only), to load or evaluate code, to change the environment or the working
/// directory cargo is started in, or a second statement on a line.
const SCRIPT_HEAD_FORBIDDEN: [&str; 24] = [
    "subprocess",
    "os.system",
    "os.exec",
    "os.spawn",
    "os.popen",
    "os.fork",
    "posix_spawn",
    "runpy",
    "importlib",
    "import ",
    "__import__",
    "exec(",
    "eval(",
    "compile(",
    "ctypes",
    "Popen",
    "environ",
    "putenv",
    "unsetenv",
    "chdir",
    "getattr(",
    "setattr(",
    "globals(",
    ";",
];

/// The lines that are code: not blank, not a full-line comment.
fn code_lines(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line.trim_end()))
        .filter(|(_, line)| {
            let line = line.trim_start();
            !line.is_empty() && !line.starts_with('#')
        })
        .collect()
}

/// Fails unless the script builds exactly what [`DECLARATION`] declares and
/// cannot run anything else:
///
/// - from `def cargo_commands` to the end it is [`PINNED_SCRIPT_TAIL`] (code
///   lines compared, so comments and blank lines are free). That part makes
///   every cargo argv in one function, starts cargo at one place, scrubs its
///   environment, refuses a visible cargo configuration, and holds every other
///   program the script runs (kache, `install`, the completions);
/// - before it, the script has only [`SCRIPT_IMPORTS`], no toolchain word
///   outside comments and none of [`SCRIPT_HEAD_FORBIDDEN`].
///
/// What these lines cannot see (a change to the declaration's reading that
/// still prints the right argv) is checked by running the script:
/// [`check_script_commands`], [`check_script_environment`] and
/// [`check_script_configs`].
fn check_build_script(text: &str) -> Result<()> {
    let lines: Vec<&str> = text.lines().collect();
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == SCRIPT_TAIL_START)
        .map(|(index, _)| index)
        .collect();
    ensure!(
        starts.len() == 1,
        "{BUILD_SCRIPT} must contain the line `{SCRIPT_TAIL_START}` exactly once (found {})",
        starts.len()
    );
    let split = starts[0];
    for (index, raw) in lines[..split].iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        if line.starts_with("import ") || line.starts_with("from ") {
            ensure!(
                SCRIPT_IMPORTS.contains(&line),
                "{BUILD_SCRIPT} line {number}: `{line}` is not one of its imports {SCRIPT_IMPORTS:?}"
            );
            continue;
        }
        ensure!(
            !mentions_toolchain(line),
            "{BUILD_SCRIPT} line {number}: `{line}` names cargo, rustc, rustup, rustdoc or rust*; only the pinned part (from `{SCRIPT_TAIL_START}`) may, so that it builds exactly what {DECLARATION} declares"
        );
        // `re.compile` is the regular expression, not the builtin.
        let checked = line.replace("re.compile(", "re.regex(");
        if let Some(word) = SCRIPT_HEAD_FORBIDDEN
            .iter()
            .find(|word| checked.contains(**word))
        {
            bail!(
                "{BUILD_SCRIPT} line {number}: `{line}` contains `{word}`; before the pinned part the script may only read the declaration, not run a program, load code or change the environment"
            );
        }
    }
    let tail = lines[split..].join("\n");
    let found = code_lines(&tail);
    let pinned = code_lines(PINNED_SCRIPT_TAIL);
    for index in 0..found.len().max(pinned.len()) {
        let (got, want) = (found.get(index), pinned.get(index));
        if got.map(|(_, line)| line) != want.map(|(_, line)| line) {
            bail!(
                "{BUILD_SCRIPT}: the part from `{SCRIPT_TAIL_START}` differs from the pinned copy (crates/ilex-tooling-linux/src/build-image-crates.pinned.py) at its code line {}: found `{}`, pinned `{}`. Review the change, then edit both files",
                index + 1,
                got.map_or("(end of file)", |(_, line)| *line),
                want.map_or("(end of file)", |(_, line)| *line),
            );
        }
    }
    Ok(())
}

/// What `build/` (outside the declaration and the script, which have their own
/// checks) may say about the toolchain besides comments: a message that names
/// the host command a developer should run.
const BUILD_DIR_ALLOWED: [(&str, &str); 1] = [(
    "build/image_tree.py",
    r#"error("inherited overlays", PROVENANCE, "missing registered overlay provenance; rebuild the base with cargo ilexctl build")"#,
)];

/// Fails when a file under `build/` other than the declaration and the script
/// names the toolchain outside a comment and [`BUILD_DIR_ALLOWED`]: the
/// Containerfile copies only those two files into the builder stage, but a
/// helper that runs cargo anywhere in `build/` is drift the guard should
/// refuse before anyone wires it in.
fn check_build_files(files: &[(String, String)]) -> Result<()> {
    for (path, text) in files {
        if path == DECLARATION || path == BUILD_SCRIPT {
            continue;
        }
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.starts_with('#') || !mentions_toolchain(line) {
                continue;
            }
            ensure!(
                BUILD_DIR_ALLOWED.contains(&(path.as_str(), line)),
                "{path} line {}: `{line}` names cargo, rustc, rustup, rustdoc or rust*; the root image builds Rust only through {BUILD_SCRIPT}, so no other helper in build/ may run the toolchain",
                index + 1
            );
        }
    }
    Ok(())
}

/// Every file under `build/` as (repo-relative path, text), sorted.
fn build_files(root: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    let mut pending = vec![root.join("build")];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file() {
                let name = path
                    .strip_prefix(root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
                files.push((name, String::from_utf8_lossy(&bytes).into_owned()));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// The packages the root image builds: the declaration, after the guards have
/// checked that the Containerfile, the script and `build/` build nothing else.
pub(crate) fn image_builds(root: &Path) -> Result<Vec<CargoBuild>> {
    let read = |path: &str| {
        std::fs::read_to_string(root.join(path)).with_context(|| format!("reading {path}"))
    };
    check_containerfile(&read("Containerfile")?).context("the root Containerfile")?;
    check_build_script(&read(BUILD_SCRIPT)?).with_context(|| BUILD_SCRIPT.to_owned())?;
    check_build_files(&build_files(root)?).context("build/")?;
    parse_declaration(&read(DECLARATION)?).with_context(|| DECLARATION.to_owned())
}

/// The builder stage of the fixtures: the real Containerfile's, with other
/// cache ids.
const FIXTURE_BUILD_STEP: &str = r#"RUN --mount=type=cache,id=x-cargo,target=/root/.cargo/registry \
    --mount=type=cache,id=x-kache,target=/root/.cache/kache,sharing=locked \
    python3 -I -S build/build-image-crates.py build/image-crates.toml
"#;

/// A Containerfile in the shape of the real one: a stage that never names the
/// toolchain, the builder stage with comments that name it, and the image.
fn good_containerfile() -> String {
    format!(
        r#"
# Build with `cargo ilexctl build`; rustup configures the host, not these stages.
FROM quay.io/fedora/fedora-bootc:45 AS compose
COPY --link --parents image.toml packages.toml build/ shells/ /ctx/
RUN python3 -I -S -c 'import runpy,sys; sys.path.insert(0,"/ctx/build"); runpy.run_path("/ctx/build/compose.py",run_name="__main__")' image --out /out

# Fedora's cargo is used directly.
FROM quay.io/fedora/fedora:45 AS build
RUN --mount=type=cache,id=x-dnf,target=/var/cache/libdnf5,sharing=locked \
    dnf -y install --setopt=keepcache=1 cargo gcc python3
ARG KACHE_VERSION=2.0.0
ARG KACHE_SHA256=6c59665faf1b14d0944391e6f2a1b510195e6be92c63060a9d1770ed00676824
RUN curl -fsSL -o /tmp/kache.tar.gz \
      "https://github.com/kunobi-ninja/kache/releases/download/v${{KACHE_VERSION}}/kache-x86_64-unknown-linux-musl.tar.gz" && \
    echo "${{KACHE_SHA256}}  /tmp/kache.tar.gz" | sha256sum -c - && \
    tar -xzf /tmp/kache.tar.gz -C /usr/local/bin kache kache-engine && \
    rm /tmp/kache.tar.gz && \
    kache --version
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY build/build-image-crates.py build/image-crates.toml build/
{FIXTURE_BUILD_STEP}
FROM quay.io/fedora/fedora-bootc:45
COPY --from=build /out/ /
RUN echo "${{SUM}}  x" | sha256sum -c -
"#
    )
}

/// `(name, Containerfile, None to accept or Some(text) the refusal must
/// contain)`. Every bypass the reviews found, in the shell parser's time and
/// since, and the plain ways to run the toolchain or change what it builds.
type GuardFixture = (&'static str, String, Option<String>);

/// The step of the fixtures' builder stage that runs the script, as the one
/// logical line the guard sees.
const FIXTURE_BUILD_RUN: &str = "RUN --mount=type=cache,id=x-cargo,target=/root/.cargo/registry --mount=type=cache,id=x-kache,target=/root/.cache/kache,sharing=locked";

fn containerfile_fixtures() -> Vec<GuardFixture> {
    const INVOCATION: &str = "python3 -I -S build/build-image-crates.py build/image-crates.toml";
    const LAST_STAGE: &str = "\nFROM quay.io/fedora/fedora-bootc:45\n";
    let good = good_containerfile();
    // Before the image's stage (the builder's) / at the end of the file (the
    // image's stage) / a different text for one piece of the real shape.
    let in_builder = |extra: &str| good.replacen(LAST_STAGE, &format!("{extra}\n{LAST_STAGE}"), 1);
    let with = |extra: &str| format!("{good}{extra}");
    let replacing = |from: &str, to: &str| {
        assert!(
            good.matches(from).count() == 1,
            "Containerfile fixture anchor `{from}` must occur once"
        );
        good.replacen(from, to, 1)
    };
    // The refusal names the logical line that does not belong in the stage.
    let refused = |line: &str| {
        Some(format!(
            "`{line}` is not allowed in the stage that builds the Rust packages"
        ))
    };
    let mut fixtures: Vec<GuardFixture> = vec![
        ("the shape of the real Containerfile", good.clone(), None),
        (
            "comments, other stages and toolchain-free lines",
            with("# cargo build -p evil\nRUN echo hi; true\nRUN update-ca-trust\nENV TRUSTED=1\n"),
            None,
        ),
        (
            "another KACHE_ argument in the builder stage",
            in_builder("ARG KACHE_EXTRA=1"),
            None,
        ),
        (
            "another toolchain package",
            in_builder("RUN dnf -y install rustup"),
            None,
        ),
    ];
    // (name, inserted before the image's stage, the line the refusal names)
    for (name, extra, culprit) in [
        (
            "a second `cargo build`",
            "RUN cargo build --release --locked -p evil",
            "RUN cargo build --release --locked -p evil",
        ),
        ("another helper", "RUN sh build/x.sh", "RUN sh build/x.sh"),
        ("any other command", "RUN true", "RUN true"),
        (
            "a heredoc RUN",
            "RUN <<EOF\nsh build/x.sh\nEOF",
            "RUN <<EOF",
        ),
        (
            "ENV RUSTFLAGS with a cfg",
            "ENV RUSTFLAGS=\"--cfg ilex_drift\"",
            "ENV RUSTFLAGS=\"--cfg ilex_drift\"",
        ),
        (
            "ARG RUSTFLAGS",
            "ARG RUSTFLAGS=\"--cfg ilex_drift\"",
            "ARG RUSTFLAGS=\"--cfg ilex_drift\"",
        ),
        (
            "an append to the declaration",
            "RUN echo '[[build]]' >> build/image-crates.toml",
            "RUN echo '[[build]]' >> build/image-crates.toml",
        ),
        (
            "a heredoc COPY that writes the declaration",
            "COPY <<EOF build/image-crates.toml\n[[build]]\npackages = [\"evil\"]\nEOF",
            "COPY <<EOF build/image-crates.toml",
        ),
        (
            "an edit of the declaration that does not name it",
            "RUN sed -i s/ilexctl/evil/ build/*.toml",
            "RUN sed -i s/ilexctl/evil/ build/*.toml",
        ),
        (
            "a COPY of a cargo configuration directory",
            "COPY .cargo/ .cargo/",
            "COPY .cargo/ .cargo/",
        ),
        (
            "a COPY of rust-toolchain.toml",
            "COPY rust-toolchain.toml ./",
            "COPY rust-toolchain.toml ./",
        ),
        (
            "a COPY of a config.toml into cargo's home",
            "COPY config.toml /root/.cargo/config.toml",
            "COPY config.toml /root/.cargo/config.toml",
        ),
        (
            "an ADD of a cargo configuration directory",
            "ADD .cargo /src/.cargo",
            "ADD .cargo /src/.cargo",
        ),
        (
            "a cargo configuration written by a command",
            "RUN printf '[build]\\ntarget=\"x\"\\n' > /root/.cargo/config.toml",
            "RUN printf '[build]\\ntarget=\"x\"\\n' > /root/.cargo/config.toml",
        ),
        (
            "the whole build/ directory",
            "COPY build/ build/",
            "COPY build/ build/",
        ),
        (
            "a bind mount",
            "RUN --mount=type=bind,source=/etc,target=/src/.cargo dnf -y install foo",
            "RUN --mount=type=bind,source=/etc,target=/src/.cargo dnf -y install foo",
        ),
        (
            "a cache mount over a cargo configuration directory",
            "RUN --mount=type=cache,id=x,target=/src/.cargo dnf -y install foo",
            "RUN --mount=type=cache,id=x,target=/src/.cargo dnf -y install foo",
        ),
        (
            "another working directory",
            "WORKDIR /elsewhere",
            "WORKDIR /elsewhere",
        ),
        (
            "ENV PATH",
            "ENV PATH=/evil:/usr/bin",
            "ENV PATH=/evil:/usr/bin",
        ),
        ("USER", "USER nobody", "USER nobody"),
        (
            "SHELL",
            "SHELL [\"/bin/sh\", \"-c\"]",
            "SHELL [\"/bin/sh\", \"-c\"]",
        ),
        ("an ARG that is not KACHE_*", "ARG EXTRA=1", "ARG EXTRA=1"),
        (
            "the toolchain install chained with a build",
            "RUN dnf -y install cargo && cargo build -p evil",
            "RUN dnf -y install cargo && cargo build -p evil",
        ),
        (
            "the toolchain install, then a command",
            "RUN dnf -y install cargo; true",
            "RUN dnf -y install cargo; true",
        ),
        (
            "the toolchain install with a substitution",
            "RUN dnf -y install $(cargo build -p evil)",
            "RUN dnf -y install $(cargo build -p evil)",
        ),
        (
            "a COPY with a command after it",
            "COPY x /y; cargo build -p evil",
            "COPY x /y; cargo build -p evil",
        ),
    ] {
        fixtures.push((name, in_builder(extra), refused(culprit)));
    }
    // (name, appended to the image's stage, the line the refusal names)
    for (name, extra, culprit) in [
        (
            "a second `cargo build` in the image's stage",
            "RUN cargo build --release --locked -p evil\n",
            "RUN cargo build --release --locked -p evil",
        ),
        (
            "another stage runs it",
            "FROM quay.io/fedora/fedora:45 AS other\nRUN cargo install --locked evil\n",
            "RUN cargo install --locked evil",
        ),
        (
            "another helper in a second stage that installs the toolchain",
            "FROM quay.io/fedora/fedora:45 AS other\nRUN dnf -y install cargo\nRUN sh build/x.sh\n",
            "RUN sh build/x.sh",
        ),
        (
            "process substitution in a redirection",
            "RUN cargo build -p ilexctl 2> >(tee build.log) --workspace\n",
            "RUN cargo build -p ilexctl 2> >(tee build.log) --workspace",
        ),
        (
            "here-string",
            "RUN sh <<< 'cargo build -p evil'\n",
            "RUN sh <<< 'cargo build -p evil'",
        ),
        (
            "heredoc body",
            "RUN <<EOF\ncargo build -p evil\nEOF\n",
            "cargo build -p evil",
        ),
        (
            "heredoc body that starts like an instruction",
            "RUN <<EOF\nCOPY x; cargo build -p evil\nEOF\n",
            "COPY x; cargo build -p evil",
        ),
        (
            "substitution nested in a parameter expansion",
            "RUN echo \"${x:-$(echo $((1 + 2)); cargo build -p evil)}\"\n",
            "RUN echo \"${x:-$(echo $((1 + 2)); cargo build -p evil)}\"",
        ),
        (
            "substitution with a closing parenthesis inside (case)",
            "RUN echo \"$(case x in x) cargo build -p evil;; esac)\"\n",
            "RUN echo \"$(case x in x) cargo build -p evil;; esac)\"",
        ),
        (
            "an appended environment variable",
            "RUN CARGO_BUILD_TARGET+=-musl true\n",
            "RUN CARGO_BUILD_TARGET+=-musl true",
        ),
        (
            "a quoted export",
            "RUN export \"CARGO_BUILD_TARGET=x86_64-unknown-linux-musl\"\n",
            "RUN export \"CARGO_BUILD_TARGET=x86_64-unknown-linux-musl\"",
        ),
        (
            "ENV CARGO_BUILD_TARGET",
            "ENV CARGO_BUILD_TARGET=x86_64-unknown-linux-musl\n",
            "ENV CARGO_BUILD_TARGET=x86_64-unknown-linux-musl",
        ),
        (
            "ARG CARGO_BUILD_JOBS",
            "ARG CARGO_BUILD_JOBS=1\n",
            "ARG CARGO_BUILD_JOBS=1",
        ),
        (
            "ENV RUSTDOCFLAGS",
            "ENV RUSTDOCFLAGS=\"--cfg x\"\n",
            "ENV RUSTDOCFLAGS=\"--cfg x\"",
        ),
        (
            "ENV CARGO_ENCODED_RUSTFLAGS",
            "ENV CARGO_ENCODED_RUSTFLAGS=x\n",
            "ENV CARGO_ENCODED_RUSTFLAGS=x",
        ),
        (
            "ENV CARGO_BUILD_RUSTFLAGS",
            "ENV CARGO_BUILD_RUSTFLAGS=x\n",
            "ENV CARGO_BUILD_RUSTFLAGS=x",
        ),
        (
            "a lower-case rustflags variable",
            "env rustflags=\"--cfg x\"\n",
            "env rustflags=\"--cfg x\"",
        ),
        (
            "RUSTFLAGS alone in a new stage",
            "FROM scratch AS elsewhere\nENV RUSTFLAGS=\"--cfg x\"\n",
            "ENV RUSTFLAGS=\"--cfg x\"",
        ),
        (
            "the wrapper variable",
            "ENV RUSTC_WRAPPER=/evil\n",
            "ENV RUSTC_WRAPPER=/evil",
        ),
        (
            "a toolchain selector",
            "ENV RUSTUP_TOOLCHAIN=nightly\n",
            "ENV RUSTUP_TOOLCHAIN=nightly",
        ),
        (
            "an append to the declaration, in the image's stage",
            "RUN echo >> build/image-crates.toml\n",
            "RUN echo >> build/image-crates.toml",
        ),
        (
            "a COPY from a stage into cargo's home",
            "COPY --from=compose /out/ /root/.cargo/\n",
            "COPY --from=compose /out/ /root/.cargo/",
        ),
        (
            "exec form",
            "RUN [\"/usr/bin/cargo\", \"build\", \"-p\", \"evil\"]\n",
            "RUN [\"/usr/bin/cargo\", \"build\", \"-p\", \"evil\"]",
        ),
        (
            "a lower-case instruction",
            "run cargo build -p evil\n",
            "run cargo build -p evil",
        ),
        (
            "ONBUILD",
            "ONBUILD RUN cargo build -p evil\n",
            "ONBUILD RUN cargo build -p evil",
        ),
        (
            "a continuation",
            "RUN true \\\n    && cargo build -p evil\n",
            "RUN true && cargo build -p evil",
        ),
        (
            "a trailing comment",
            "RUN true # cargo build -p evil\n",
            "RUN true # cargo build -p evil",
        ),
        (
            "rustup",
            "RUN rustup run nightly true\n",
            "RUN rustup run nightly true",
        ),
    ] {
        fixtures.push((name, with(extra), refused(culprit)));
    }
    let build_run = |command: &str| format!("{FIXTURE_BUILD_RUN} {command}");
    fixtures.extend([
        (
            "a builder stage on another image",
            replacing(
                "FROM quay.io/fedora/fedora:45 AS build",
                "FROM docker.io/library/rust:1 AS build",
            ),
            refused("FROM docker.io/library/rust:1 AS build"),
        ),
        (
            "the kache download with another step after it",
            replacing("kache --version", "kache --version && true"),
            refused(&format!("{KACHE_INSTALL} && true")),
        ),
        (
            "a bind mount over the declaration",
            replacing(
                "--mount=type=cache,id=x-kache,target=/root/.cache/kache,sharing=locked",
                "--mount=type=bind,source=/tmp/x,target=/src/build/image-crates.toml",
            ),
            refused(&format!(
                "RUN --mount=type=cache,id=x-cargo,target=/root/.cargo/registry --mount=type=bind,source=/tmp/x,target=/src/build/image-crates.toml {INVOCATION}"
            )),
        ),
        (
            "the script's own invocation, chained",
            replacing(INVOCATION, &format!("{INVOCATION} && cargo build -p evil")),
            refused(&build_run(&format!("{INVOCATION} && cargo build -p evil"))),
        ),
        (
            "the script on another declaration",
            replacing("build/image-crates.toml\n", "build/other.toml\n"),
            refused(&build_run(
                "python3 -I -S build/build-image-crates.py build/other.toml",
            )),
        ),
        (
            "the script with another interpreter",
            replacing("python3 -I -S build/build", "python3 -S build/build"),
            refused(&build_run(
                "python3 -S build/build-image-crates.py build/image-crates.toml",
            )),
        ),
        (
            "the script run twice",
            in_builder(&format!("RUN {INVOCATION}")),
            Some("exactly once, found 2".to_owned()),
        ),
        (
            "the script not run at all",
            replacing(FIXTURE_BUILD_STEP, ""),
            Some("exactly once, found 0".to_owned()),
        ),
    ]);
    fixtures
}

/// The one thing a text check cannot see, or a check that runs the script.
#[derive(Clone, Copy)]
enum Via {
    /// The text check must refuse it.
    Text,
    /// The text check accepts it, and `--commands` shows what it builds.
    Blind,
    /// `--commands` must show what the declaration does not imply.
    Commands,
    /// `--environment` must show a variable that reaches cargo.
    Environment,
    /// `--configs` must miss a configuration file that cargo would read.
    Configs,
}

struct ScriptFixture {
    name: &'static str,
    text: String,
    via: Via,
    expect: Option<String>,
}

fn fx(name: &'static str, text: String, via: Via, expect: Option<String>) -> ScriptFixture {
    ScriptFixture {
        name,
        text,
        via,
        expect,
    }
}

/// Fixtures for the build script, each a one-place change of the real script
/// (every anchor must occur exactly once, so a rewrite of the script that
/// moves one is a failed check, not a silently weaker one). A refusal names
/// the line that was changed.
fn script_fixtures(script: &str) -> Result<Vec<ScriptFixture>> {
    let swap = |from: &str, to: &str| -> Result<String> {
        ensure!(
            script.matches(from).count() == 1,
            "script fixture anchor `{from}` must occur exactly once in {BUILD_SCRIPT}; change the fixtures with the script"
        );
        Ok(script.replacen(from, to, 1))
    };
    // A line before the pinned part, and one after the command is built.
    let head = |line: &str| {
        swap(
            "def cargo_commands(builds):\n",
            &format!("{line}\n\n\ndef cargo_commands(builds):\n"),
        )
    };
    let in_argv = |line: &str| {
        swap(
            "        commands.append(command)\n",
            &format!("        {line}\n        commands.append(command)\n"),
        )
    };
    let found = |line: &str| Some(format!("found `{line}`"));
    let some = |text: &str| Some(text.to_owned());
    const COMMANDS: &str = "the declaration implies exactly";
    let mut fixtures = vec![
        fx("the real script", script.to_owned(), Via::Text, None),
        fx(
            "comments and blank lines that name the toolchain",
            swap(
                "    commands = []\n",
                "    # cargo build --workspace is not run here\n\n    commands = []\n",
            )?
            .replacen(
                "PACKAGE = ",
                "# cargo build -p evil (a comment)\n\n\nPACKAGE = ",
                1,
            ),
            Via::Text,
            None,
        ),
    ];

    // The argv, and the places that run it: the pinned part differs from its
    // copy, and for the argv the printed commands differ from the
    // declaration's.
    let workspace = in_argv("command += [\"--workspace\"]")?;
    let extra_package = swap(
        "            command += [\"-p\", package]\n",
        "            command += [\"-p\", package, \"-p\", \"evil\"]\n",
    )?;
    let target = in_argv("command += [\"--target\", \"x86_64-unknown-linux-musl\"]")?;
    for (name, text, line) in [
        (
            "a flag appended to the argv: --workspace",
            workspace.clone(),
            "        command += [\"--workspace\"]",
        ),
        (
            "an extra -p",
            extra_package.clone(),
            "            command += [\"-p\", package, \"-p\", \"evil\"]",
        ),
        (
            "--features",
            in_argv("command.append(\"--features=evil\")")?,
            "        command.append(\"--features=evil\")",
        ),
        (
            "--target",
            target.clone(),
            "        command += [\"--target\", \"x86_64-unknown-linux-musl\"]",
        ),
        (
            "--config",
            swap(
                "[\"cargo\", \"build\", \"--release\", \"--locked\"]",
                "[\"cargo\", \"--config\", \"build.rustflags=['--cfg=x']\", \"build\", \"--release\", \"--locked\"]",
            )?,
            "        command = [\"cargo\", \"--config\", \"build.rustflags=['--cfg=x']\", \"build\", \"--release\", \"--locked\"]",
        ),
        (
            "a second run of the argv with more on it",
            swap(
                "        subprocess.run(command, check=True, env=env)\n",
                "        subprocess.run(command, check=True, env=env)\n        subprocess.run(command + [\"--workspace\"], check=True, env=env)\n",
            )?,
            "        subprocess.run(command + [\"--workspace\"], check=True, env=env)",
        ),
        (
            "a build appended to the commands in main",
            swap(
                "    commands = planned(argv[1])\n",
                "    commands = planned(argv[1])\n    commands.append([\"cargo\", \"build\", \"--workspace\"])\n",
            )?,
            "    commands.append([\"cargo\", \"build\", \"--workspace\"])",
        ),
        (
            "another helper run in main",
            swap(
                "    env = cargo_environment()\n",
                "    env = cargo_environment()\n    subprocess.run([\"sh\", \"build/x.sh\"], check=True)\n",
            )?,
            "    subprocess.run([\"sh\", \"build/x.sh\"], check=True)",
        ),
        (
            "the cargo configuration check removed",
            swap("    if configs:\n", "    if False:\n")?,
            "    if False:",
        ),
    ] {
        fixtures.push(fx(name, text, Via::Text, found(line)));
    }
    for (name, text) in [
        (
            "a flag appended to the argv: --workspace (printed)",
            workspace,
        ),
        ("an extra -p (printed)", extra_package),
        ("--target (printed)", target),
    ] {
        fixtures.push(fx(name, text, Via::Commands, some(COMMANDS)));
    }

    // The text before the pinned part.
    for (name, line, word) in [
        ("os.system", "os.system(\"sh build/x.sh\")", "os.system"),
        (
            "subprocess",
            "subprocess.run([\"sh\", \"build/x.sh\"])",
            "subprocess",
        ),
        (
            "os.execvp",
            "os.execvp(\"sh\", [\"sh\", \"build/x.sh\"])",
            "os.exec",
        ),
        (
            "os.spawnlp",
            "os.spawnlp(os.P_WAIT, \"sh\", \"sh\", \"build/x.sh\")",
            "os.spawn",
        ),
        (
            "an import hidden on a line",
            "if True: import shlex",
            "import ",
        ),
        (
            "exec of another file",
            "exec(open(\"build/x.py\").read())",
            "exec(",
        ),
        ("eval", "eval(\"1\")", "eval("),
        (
            "__import__",
            "__import__(\"os\").system(\"true\")",
            "__import__",
        ),
        (
            "a setting of the environment",
            "os.putenv(\"X\", \"1\")",
            "putenv",
        ),
        ("a change of directory", "os.chdir(\"/\")", "chdir"),
        ("two statements on a line", "x = 1; y = 2", ";"),
    ] {
        fixtures.push(fx(
            name,
            head(line)?,
            Via::Text,
            Some(format!("`{line}` contains `{word}`")),
        ));
    }
    for (name, line) in [
        ("runpy", "import runpy"),
        ("importlib", "import importlib"),
        ("from-import of os.system", "from os import system"),
    ] {
        fixtures.push(fx(
            name,
            head(line)?,
            Via::Text,
            Some(format!("`{line}` is not one of its imports")),
        ));
    }
    let flags = "os.environ[\"RUSTFLAGS\"] = \"--cfg ilex_drift\"";
    fixtures.push(fx(
        "RUSTFLAGS set before the pinned part",
        head(flags)?,
        Via::Text,
        Some(format!("`{flags}` names cargo, rustc")),
    ));
    let keys = "KEYS = {\"packages\", \"no-default-features\"}";
    let commented = format!("{keys}  # cargo build -p evil");
    fixtures.push(fx(
        "the toolchain's name in a trailing comment",
        swap(&format!("{keys}\n"), &format!("{commented}\n"))?,
        Via::Text,
        Some(format!("`{commented}` names cargo, rustc")),
    ));
    fixtures.push(fx(
        "the pinned part's start line twice",
        format!("{script}\ndef cargo_commands(builds):\n"),
        Via::Text,
        some("exactly once"),
    ));

    // Invisible to the text: what the declaration reading does at run time.
    fixtures.push(fx(
        "a build appended while reading the declaration",
        swap(
            "    return builds\n",
            "    builds.append({\"packages\": [\"evil\"], \"no-default-features\": False})\n    return builds\n",
        )?,
        Via::Blind,
        some(COMMANDS),
    ));
    fixtures.push(fx(
        "the declaration's builds in another order",
        swap("    return builds\n", "    return builds[::-1]\n")?,
        Via::Blind,
        some(COMMANDS),
    ));

    // The environment.
    let scrub = "(\"CARGO\", \"RUST\", \"KACHE\")";
    for (name, text, expect) in [
        (
            "an unscrubbed environment",
            swap(
                &format!("        if not name.upper().startswith({scrub})\n"),
                "        if True\n",
            )?,
            "starts cargo with",
        ),
        (
            "an environment that keeps RUSTFLAGS",
            swap(scrub, "(\"CARGO\", \"KACHE\")")?,
            "starts cargo with RUSTC",
        ),
        (
            "an environment that keeps the caller's KACHE_ settings",
            swap(scrub, "(\"CARGO\", \"RUST\")")?,
            "starts cargo with KACHE_CACHE_DIR",
        ),
        (
            "another wrapper",
            swap(
                "    env[\"RUSTC_WRAPPER\"] = \"kache\"\n",
                "    env[\"RUSTC_WRAPPER\"] = \"sccache\"\n",
            )?,
            "starts cargo with RUSTC_WRAPPER=sccache",
        ),
        (
            "no wrapper",
            swap("    env[\"RUSTC_WRAPPER\"] = \"kache\"\n", "    pass\n")?,
            "does not start cargo with RUSTC_WRAPPER=kache",
        ),
    ] {
        fixtures.push(fx(name, text, Via::Environment, some(expect)));
    }

    // The cargo configuration.
    for (name, text) in [
        (
            "no cargo configuration is ever listed",
            swap(
                "    return sorted({path for path in found if os.path.lexists(path)})\n",
                "    return []\n",
            )?,
        ),
        (
            "only config.toml is looked for",
            swap(
                "    names = (\"config\", \"config.toml\")\n",
                "    names = (\"config.toml\",)\n",
            )?,
        ),
        (
            "only the working directory is looked in",
            swap("        here = parent\n", "        break\n")?,
        ),
        (
            "cargo's home is not looked in",
            swap(
                "    directories = [os.path.join(os.path.expanduser(\"~\"), \".cargo\")]\n",
                "    directories = []\n",
            )?,
        ),
    ] {
        fixtures.push(fx(name, text, Via::Configs, some("is not listed")));
    }
    Ok(fixtures)
}

/// What the build script must show for the real declaration, run as
/// `python3 -I -S SCRIPT --commands DECLARATION`: exactly the argv the
/// declaration implies.
fn check_script_commands(script: &Path, declaration: &Path, builds: &[CargoBuild]) -> Result<()> {
    let declaration = declaration
        .to_str()
        .context("the declaration's path is not UTF-8")?;
    let output = run_script(script, &["--commands", declaration], |_| {})?;
    let printed: Vec<Vec<String>> =
        serde_json::from_str(&output).context("reading the script's --commands output")?;
    let implied = expected_commands(builds);
    ensure!(
        printed == implied,
        "the script prints the cargo commands {printed:?} for {DECLARATION}, but the declaration implies exactly {implied:?}"
    );
    Ok(())
}

fn run_script(
    script: &Path,
    args: &[&str],
    configure: impl FnOnce(&mut Command),
) -> Result<String> {
    let mut command = Command::new("python3");
    command.args(["-I", "-S"]).arg(script).args(args);
    configure(&mut command);
    cmd::output(&mut command)
        .with_context(|| format!("running {} {}", script.display(), args.join(" ")))
}

/// Settings of a caller's environment that must not reach cargo. The values
/// are as harmless as the names are telling.
const PLANTED_ENVIRONMENT: [(&str, &str); 16] = [
    ("RUSTFLAGS", "--cfg ilex_drift"),
    ("RUSTDOCFLAGS", "--cfg ilex_drift"),
    ("CARGO_ENCODED_RUSTFLAGS", "--cfg=ilex_drift"),
    ("CARGO_BUILD_RUSTFLAGS", "--cfg ilex_drift"),
    (
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS",
        "--cfg ilex_drift",
    ),
    ("CARGO_BUILD_TARGET", "x86_64-unknown-linux-musl"),
    ("CARGO_HOME", "/nonexistent"),
    ("CARGO_TARGET_DIR", "/nonexistent"),
    ("cargo_net_offline", "true"),
    ("RUSTC", "/nonexistent"),
    ("RUSTC_WRAPPER", "/nonexistent"),
    ("RUSTC_WORKSPACE_WRAPPER", "/nonexistent"),
    ("RUSTUP_TOOLCHAIN", "nightly"),
    ("RUST_BACKTRACE", "1"),
    ("KACHE_CACHE_DIR", "/nonexistent"),
    ("KACHE_READONLY_STORE", "/nonexistent"),
];

/// A variable that has nothing to do with cargo and must survive.
const KEPT_VARIABLE: &str = "ILEX_SELF_CHECK_KEPT";

/// The script's `--environment` for an environment that sets every one of
/// [`PLANTED_ENVIRONMENT`]: nothing of `CARGO*`, `RUST*` or `KACHE*` but the
/// wrapper and the host store the script sets itself, and the rest intact.
fn check_script_environment(script: &Path) -> Result<()> {
    let output = run_script(script, &["--environment"], |command| {
        command.envs(PLANTED_ENVIRONMENT).env(KEPT_VARIABLE, "1");
    })?;
    let env: BTreeMap<String, String> =
        serde_json::from_str(&output).context("reading the script's --environment output")?;
    for (name, value) in &env {
        let upper = name.to_ascii_uppercase();
        if ["CARGO", "RUST", "KACHE"]
            .iter()
            .any(|prefix| upper.starts_with(prefix))
        {
            ensure!(
                (name == "RUSTC_WRAPPER" && value == "kache")
                    || (name == "KACHE_READONLY_STORE" && value == "/kache-host"),
                "the script starts cargo with {name}={value}; its environment may hold nothing of CARGO*, RUST* or KACHE* but RUSTC_WRAPPER=kache and the read-only host store"
            );
        }
    }
    ensure!(
        env.get("RUSTC_WRAPPER").map(String::as_str) == Some("kache"),
        "the script does not start cargo with RUSTC_WRAPPER=kache"
    );
    ensure!(
        env.get(KEPT_VARIABLE).map(String::as_str) == Some("1") && env.contains_key("PATH"),
        "the script's environment lost variables that have nothing to do with cargo ({KEPT_VARIABLE}, PATH)"
    );
    Ok(())
}

/// The script's `--configs` in a scratch directory: empty, then one cargo
/// configuration at a time where cargo would read it (the working directory,
/// a parent, cargo's home), which it must list. Paths outside `scratch` are
/// the host's and are ignored.
fn check_script_configs(script: &Path, scratch: &Path) -> Result<()> {
    let home = scratch.join("home");
    let parent = scratch.join("work");
    let work = parent.join("inner");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&work)?;
    let listed = || -> Result<Vec<String>> {
        let output = run_script(script, &["--configs"], |command| {
            command.current_dir(&work).env("HOME", &home);
        })?;
        let all: Vec<String> =
            serde_json::from_str(&output).context("reading the script's --configs output")?;
        Ok(all
            .into_iter()
            .filter(|path| Path::new(path).starts_with(scratch))
            .collect())
    };
    let none = listed()?;
    ensure!(
        none.is_empty(),
        "the script lists cargo configuration {none:?} where there is none"
    );
    for (place, file) in [
        ("the working directory", work.join(".cargo/config.toml")),
        ("a parent directory", parent.join(".cargo/config")),
        ("cargo's home", home.join(".cargo/config.toml")),
    ] {
        if let Some(directory) = file.parent() {
            std::fs::create_dir_all(directory)?;
        }
        std::fs::write(&file, "")?;
        let found = listed()?;
        std::fs::remove_file(&file)?;
        ensure!(
            found == [file.to_string_lossy().into_owned()],
            "a cargo configuration in {place} ({}) is not listed by the script's --configs, which printed {found:?}",
            file.display()
        );
    }
    Ok(())
}

fn run_script_fixture(
    fixture: &ScriptFixture,
    scratch: &Path,
    index: usize,
    root: &Path,
    builds: &[CargoBuild],
) -> Result<()> {
    let path = scratch.join(format!("script-{index}.py"));
    std::fs::write(&path, &fixture.text)?;
    let declaration = root.join(DECLARATION);
    match fixture.via {
        Via::Text => check_build_script(&fixture.text),
        Via::Blind => {
            check_build_script(&fixture.text).context(
                "fixture error: this one must pass the text check, only running the script can see it",
            )?;
            check_script_commands(&path, &declaration, builds)
        }
        Via::Commands => check_script_commands(&path, &declaration, builds),
        Via::Environment => check_script_environment(&path),
        Via::Configs => check_script_configs(&path, &scratch.join(format!("configs-{index}"))),
    }
}

/// `(name, files of build/, None to accept or Some(text) the refusal must
/// contain)`.
type BuildFilesFixture = (&'static str, Vec<(String, String)>, Option<&'static str>);

fn build_file_fixtures() -> Vec<BuildFilesFixture> {
    let file = |path: &str, text: &str| (path.to_owned(), text.to_owned());
    let (allowed_path, allowed_line) = BUILD_DIR_ALLOWED[0];
    vec![
        (
            "the declaration and the script say what they like",
            vec![
                file(
                    DECLARATION,
                    "# cargo build\n[[build]]\npackages = [\"cargo-x\"]\n",
                ),
                file(BUILD_SCRIPT, "subprocess.run(['cargo'])\n"),
            ],
            None,
        ),
        (
            "a helper that only mentions the toolchain in comments",
            vec![file(
                "build/x.sh",
                "#!/bin/sh\n# cargo build -p x, RUSTFLAGS\necho ok\n",
            )],
            None,
        ),
        (
            "the one message that names a host command",
            vec![file(allowed_path, &format!("    {allowed_line}\n"))],
            None,
        ),
        (
            "a shell helper that runs cargo",
            vec![file(
                "build/x.sh",
                "#!/bin/sh\ncargo build --release -p evil\n",
            )],
            Some("names cargo"),
        ),
        (
            "a python helper that runs cargo",
            vec![file(
                "build/x.py",
                "subprocess.run([\"cargo\", \"build\"])\n",
            )],
            Some("names cargo"),
        ),
        (
            "a helper that sets RUSTFLAGS",
            vec![file("build/x.sh", "export RUSTFLAGS=\"--cfg x\"\n")],
            Some("names cargo"),
        ),
        (
            "a helper that writes a cargo configuration",
            vec![file(
                "build/x.sh",
                "echo '[build]' > /root/.cargo/config.toml\n",
            )],
            Some("names cargo"),
        ),
        (
            "a helper that runs rustc",
            vec![file("build/y/x.sh", "rustc --version\n")],
            Some("names cargo"),
        ),
        (
            "the allowed message with something added",
            vec![file(
                allowed_path,
                &format!(
                    "{}\n",
                    allowed_line.replace("cargo ilexctl build", "cargo build -p evil")
                ),
            )],
            Some("names cargo"),
        ),
    ]
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

pub(crate) struct Scratch(pub(crate) PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The guard fixtures and the script run for real, under `scratch`, for the
/// checkout at `root`: the packages the root image builds and what was
/// checked.
pub(crate) fn self_check(
    root: &Path,
    scratch: &Path,
) -> Result<(Vec<CargoBuild>, serde_json::Value)> {
    let scripts = scratch.join("scripts");
    std::fs::create_dir_all(&scripts)?;
    let (containerfile_accepted, containerfile_refused) = run_guard_fixtures(
        "Containerfile",
        containerfile_fixtures()
            .iter()
            .map(|(name, text, expect)| (*name, check_containerfile(text), expect.as_deref())),
    )?;
    let script = std::fs::read_to_string(root.join(BUILD_SCRIPT))
        .with_context(|| format!("reading {BUILD_SCRIPT}"))?;
    let real = image_builds(root)?;
    let fixtures = script_fixtures(&script)?;
    let (script_accepted, script_refused) = run_guard_fixtures(
        "build script",
        fixtures.iter().enumerate().map(|(index, fixture)| {
            (
                fixture.name,
                run_script_fixture(fixture, &scripts, index, root, &real),
                fixture.expect.as_deref(),
            )
        }),
    )?;
    let (declaration_accepted, declaration_refused) = run_guard_fixtures(
        "declaration",
        declaration_fixtures()
            .iter()
            .map(|(name, text, expect)| (*name, parse_declaration(text).map(|_| ()), *expect)),
    )?;
    let (build_files_accepted, build_files_refused) = run_guard_fixtures(
        "build/",
        build_file_fixtures()
            .iter()
            .map(|(name, files, expect)| (*name, check_build_files(files), *expect)),
    )?;
    // The checkout's own files pass the guards (`image_builds` above), and
    // the real script prints the argv the declaration implies, starts cargo
    // with a scrubbed environment and sees every cargo configuration.
    let real_script = root.join(BUILD_SCRIPT);
    check_script_commands(&real_script, &root.join(DECLARATION), &real)?;
    check_script_environment(&real_script)?;
    check_script_configs(&real_script, &scratch.join("real-configs"))?;
    let report = serde_json::json!({
        "containerfile_fixtures_accepted": containerfile_accepted,
        "containerfile_fixtures_refused": containerfile_refused,
        "build_script_fixtures_accepted": script_accepted,
        "build_script_fixtures_refused": script_refused,
        "declaration_fixtures_accepted": declaration_accepted,
        "declaration_fixtures_refused": declaration_refused,
        "build_files_fixtures_accepted": build_files_accepted,
        "build_files_fixtures_refused": build_files_refused,
        "declared_builds": real.len(),
        "script_prints_the_argv_the_declaration_implies": true,
        "script_environment_is_scrubbed": true,
        "script_sees_cargo_configuration": true,
    });
    Ok((real, report))
}
