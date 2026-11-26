//! Shared fingerprint/cache key inputs. Preserve phase 0's publication rules.
use crate::{Workspace, cmd, config::Selection, git};
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Repo paths whose content ends up in the image, besides the crates
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

/// What one `cargo build` of the root `Containerfile` asks for.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Default)]
struct CargoBuild {
    packages: Vec<String>,
    no_default_features: bool,
    /// `--features`/`-F` or `--all-features`: widened to all features, which
    /// can only add crates to the closure, never drop one.
    all_features: bool,
}

/// Dockerfile instructions whose words are paths, names or run-time commands,
/// never a build-time shell command. Everything else (a `RUN`, or a line of its
/// heredoc) is shell.
const NON_SHELL_INSTRUCTIONS: &[&str] = &[
    "FROM",
    "COPY",
    "ADD",
    "ENV",
    "ARG",
    "LABEL",
    "WORKDIR",
    "USER",
    "EXPOSE",
    "VOLUME",
    "ENTRYPOINT",
    "CMD",
    "SHELL",
    "STOPSIGNAL",
    "HEALTHCHECK",
    "MAINTAINER",
];

/// Commands that take a package name where `cargo` or `rustc` can appear
/// without anything running.
const PACKAGE_MANAGERS: &[&str] = &[
    "dnf", "dnf5", "yum", "microdnf", "rpm", "apt", "apt-get", "zypper", "apk",
];

/// Words that can precede a command in a shell line without changing what runs.
const SHELL_PREFIXES: &[&str] = &[
    "if", "then", "else", "elif", "do", "while", "until", "!", "time", "exec", "command", "env",
];

/// `CARGO_*` variables that cannot change which packages are built.
const HARMLESS_CARGO_ENV: &[&str] = &[
    "CARGO_HOME",
    "CARGO_TARGET_DIR",
    "CARGO_INCREMENTAL",
    "CARGO_TERM_COLOR",
];

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

/// One simple command being collected by [`simple_commands`].
#[derive(Default)]
struct Frame {
    words: Vec<String>,
    word: Option<String>,
    /// Inside a double-quoted string of this command.
    double: bool,
    /// `(` for a `$(…)` substitution, `` ` `` for a backtick one.
    opener: Option<char>,
}

impl Frame {
    fn push(&mut self, c: char) {
        self.word.get_or_insert_with(String::new).push(c);
    }

    fn end_word(&mut self) {
        if let Some(word) = self.word.take() {
            self.words.push(word);
        }
    }

    fn finish(&mut self, commands: &mut Vec<Vec<String>>) {
        self.end_word();
        if !self.words.is_empty() {
            commands.push(std::mem::take(&mut self.words));
        }
    }
}

/// Simple commands of a shell line with their words, quotes kept: split at the
/// command separators outside quotes, with `$(…)` and backtick substitutions
/// as commands of their own and `${…}` kept inside its word. A single-quoted
/// string is one opaque run; a double-quoted one only starts commands through
/// `$(` and backticks. A redirection's `&` and `|` (`2>&1`, `>&2`, `&>`, `>|`)
/// do not split. `{` and `}` are separators only as words of their own, so a
/// brace expansion stays one word; a `#` that starts a word ends the line.
/// The flag is false when a quote or substitution was still open at the end of
/// the line, which this model does not trust.
fn simple_commands(line: &str) -> (Vec<Vec<String>>, bool) {
    let chars: Vec<char> = line.chars().collect();
    let mut commands = Vec::new();
    let mut stack = vec![Frame::default()];
    let mut balanced = true;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let previous = i.checked_sub(1).map(|j| chars[j]);
        let frame = stack
            .last_mut()
            .expect("the outermost frame is never popped");
        match c {
            '\\' => {
                frame.push(c);
                if let Some(escaped) = next {
                    frame.push(escaped);
                }
                i += 2;
                continue;
            }
            '$' if next == Some('(') => {
                frame.push(c);
                stack.push(Frame {
                    opener: Some('('),
                    ..Frame::default()
                });
                i += 2;
                continue;
            }
            '$' if next == Some('{') => {
                // `${…}`: the whole expansion is part of the word.
                frame.push(c);
                i += 1;
                let mut depth = 0;
                while let Some(&inner) = chars.get(i) {
                    frame.push(inner);
                    i += 1;
                    match inner {
                        '{' => depth += 1,
                        '}' => depth -= 1,
                        _ => {}
                    }
                    if depth == 0 {
                        break;
                    }
                }
                balanced &= depth == 0;
                continue;
            }
            '`' => {
                if frame.opener == Some('`') {
                    frame.finish(&mut commands);
                    stack.pop();
                } else {
                    frame.push(c);
                    stack.push(Frame {
                        opener: Some('`'),
                        ..Frame::default()
                    });
                }
                i += 1;
                continue;
            }
            _ => {}
        }
        if frame.double {
            frame.push(c);
            frame.double = c != '"';
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                frame.push(c);
                i += 1;
                loop {
                    let Some(&inner) = chars.get(i) else {
                        balanced = false;
                        break;
                    };
                    frame.push(inner);
                    i += 1;
                    if inner == '\'' {
                        break;
                    }
                }
                continue;
            }
            '"' => {
                frame.push(c);
                frame.double = true;
            }
            c if c.is_whitespace() => frame.end_word(),
            // A comment runs to the end of the line.
            '#' if frame.word.is_none() => break,
            '&' if matches!(previous, Some('<' | '>')) || next == Some('>') => frame.push(c),
            '|' if previous == Some('>') => frame.push(c),
            ';' | '&' | '|' | '(' => frame.finish(&mut commands),
            ')' => {
                frame.finish(&mut commands);
                if frame.opener == Some('(') {
                    stack.pop();
                }
            }
            '{' | '}' if frame.word.is_none() && next.is_none_or(char::is_whitespace) => {
                frame.finish(&mut commands);
            }
            _ => frame.push(c),
        }
        i += 1;
    }
    balanced &= stack.len() == 1 && !stack[0].double;
    while let Some(mut frame) = stack.pop() {
        balanced &= !frame.double;
        frame.finish(&mut commands);
    }
    (commands, balanced)
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// A word that names the Rust toolchain.
fn names_toolchain(word: &str) -> bool {
    matches!(word, "cargo" | "rustc" | "rustup" | "rustdoc") || word.starts_with("cargo-")
}

/// Whether any word mentions the toolchain, on its own, in a path, in a quoted
/// string (`sh -c 'cargo …'`) or after an `=`.
fn mentions_toolchain(words: &[&str]) -> bool {
    words.iter().any(|word| {
        word.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+')))
            .any(names_toolchain)
    })
}

fn is_assignment(word: &str) -> Option<&str> {
    let (name, _) = word.split_once('=')?;
    (!name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(name)
}

/// A command word the shell expands or looks up in a way this model does not
/// follow (`$CARGO`, `"cargo"`, `c\argo`, `[c]argo`, `{cargo,}`): it could run
/// cargo or rustc under another spelling. `[` and `[[` are the test builtins.
fn plain_command_word(word: &str) -> bool {
    matches!(word, "[" | "[[")
        || !word.contains(['$', '`', '"', '\'', '\\', '[', ']', '{', '}', '*', '?'])
}

/// If `word` is a redirection (`>log`, `2>&1`, `&>log`, `<<'EOF'`, `>`), what
/// follows its operator: empty when its target is the next word.
fn redirection_rest(word: &str) -> Option<&str> {
    let word = word.trim_start_matches(|c: char| c.is_ascii_digit());
    let word = word.strip_prefix('&').unwrap_or(word);
    word.starts_with(['<', '>'])
        .then(|| word.trim_start_matches(['<', '>', '&', '|']))
}

/// The words of a simple command without its redirections and their targets:
/// the shell removes them before running the command, so the words around
/// them are still its arguments (`cargo build -p a 2>log -p b` builds `a` and
/// `b`).
fn without_redirections<'a>(words: &[&'a str]) -> Vec<&'a str> {
    let mut kept = Vec::new();
    let mut words = words.iter().copied();
    while let Some(word) = words.next() {
        match redirection_rest(word) {
            None => kept.push(word),
            Some("") => {
                words.next();
            }
            Some(_) => {}
        }
    }
    kept
}

/// An environment variable that can change what cargo builds or how, however
/// it is set: inline before cargo, `export`, `ENV` or `ARG`.
fn check_environment(line: &str, name: &str, value: &str) -> Result<()> {
    ensure!(
        !name.starts_with("CARGO_") || HARMLESS_CARGO_ENV.contains(&name),
        "`{line}` sets {name}; it can change what cargo builds, teach `cargo_builds` first"
    );
    ensure!(
        !(matches!(name, "RUSTFLAGS" | "RUSTDOCFLAGS") && value.contains("--cfg")),
        "`{line}` sets {name} with --cfg; it can change what is compiled, teach `cargo_builds` first"
    );
    Ok(())
}

/// `ENV` and `ARG` instructions: `NAME=value …`, `ENV NAME value` and
/// `ARG NAME[=default]`. Every word's name is checked, so a value that
/// contains spaces cannot hide one.
fn check_environment_instruction(line: &str, rest: &str) -> Result<()> {
    for word in rest.split_whitespace() {
        let (name, value) = word.split_once('=').unwrap_or((word, ""));
        check_environment(line, name.trim_matches(['"', '\'']), value)?;
    }
    ensure!(
        !(rest.contains("RUSTFLAGS") || rest.contains("RUSTDOCFLAGS")) || !rest.contains("--cfg"),
        "`{line}` sets RUSTFLAGS with --cfg; it can change what is compiled, teach `cargo_builds` first"
    );
    Ok(())
}

/// A cargo package spec this model accepts: a plain package name.
fn package_name(line: &str, spec: &str) -> Result<String> {
    ensure!(
        !spec.is_empty()
            && spec
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "`{line}`: `-p {spec}` is not a plain package name (a version, glob, quote or variable); teach `cargo_builds` first"
    );
    Ok(spec.to_owned())
}

/// `--flag=value` and `-jN` forms of flags that cannot change which packages
/// are built.
fn ignored_flag_with_value(word: &str) -> bool {
    ["--timings=", "--profile=", "--target-dir=", "--jobs="]
        .iter()
        .any(|prefix| word.starts_with(prefix))
        || word
            .strip_prefix("-j")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// Parse the words after `cargo build`. Every word must be understood: an
/// unknown flag, a positional argument or any shell expansion is an error,
/// since either could change which packages the build compiles.
fn parse_cargo_build(line: &str, words: &[&str]) -> Result<CargoBuild> {
    let mut build = CargoBuild::default();
    if let Some(word) = words
        .iter()
        .find(|word| word.contains(['$', '`', '"', '\'', '\\', '*', '?', '[', '~']))
    {
        bail!(
            "`{line}`: `{word}` uses shell expansion or quoting the fingerprint's crate closure does not model"
        );
    }
    let mut words = words.iter().copied();
    while let Some(word) = words.next() {
        let mut value = |flag: &str| {
            words
                .next()
                .map(str::to_owned)
                .with_context(|| format!("`{line}` ends after {flag}"))
        };
        match word {
            "-p" | "--package" => build.packages.push(package_name(line, &value(word)?)?),
            "--no-default-features" => build.no_default_features = true,
            "--all-features" => build.all_features = true,
            "-F" | "--features" => {
                value(word)?;
                build.all_features = true;
            }
            "--release"
            | "-r"
            | "--locked"
            | "--frozen"
            | "--offline"
            | "-q"
            | "--quiet"
            | "-v"
            | "-vv"
            | "--verbose"
            | "--keep-going"
            | "--timings"
            | "--future-incompat-report" => {}
            "--profile" | "--target-dir" | "-j" | "--jobs" => {
                value(word)?;
            }
            "--workspace" | "--all" | "--exclude" | "--bins" | "--bin" | "--lib" => {
                bail!(
                    "`{line}` selects targets the fingerprint's crate closure does not model; teach `cargo_builds` first"
                )
            }
            other => {
                if let Some(spec) = other.strip_prefix("--package=") {
                    build.packages.push(package_name(line, spec)?);
                } else if other.starts_with("--features=") {
                    build.all_features = true;
                } else if let Some(spec) = other.strip_prefix("-p")
                    && !other.starts_with("--")
                {
                    // `-pfoo` and `-p=foo`.
                    build
                        .packages
                        .push(package_name(line, spec.strip_prefix('=').unwrap_or(spec))?);
                } else if other.starts_with("-F") && !other.starts_with("--") {
                    // `-Ffeat`, `-F=feat`.
                    build.all_features = true;
                } else if !ignored_flag_with_value(other) {
                    bail!(
                        "`{line}`: `{other}` is a flag or argument of `cargo build` the fingerprint's crate closure does not model; teach `cargo_builds` first"
                    );
                }
            }
        }
    }
    ensure!(
        !build.packages.is_empty(),
        "`{line}` names no package with -p; the root image's crates cannot be derived"
    );
    Ok(build)
}

/// One simple shell command: `Ok(Some(build))` for a `cargo build` that is
/// fully understood, `Ok(None)` for one that cannot compile anything the image
/// ships (a package manager naming the `cargo` package, a non-building cargo
/// subcommand, or no cargo at all), an error for any other way of running
/// cargo or rustc.
fn parse_command(line: &str, words: &[&str]) -> Result<Option<CargoBuild>> {
    let words = without_redirections(words);
    let mut rest = &words[..];
    let mut env = Vec::new();
    // Leading environment assignments and words that only wrap a command.
    while let Some((first, tail)) = rest.split_first() {
        if let Some(name) = is_assignment(first) {
            env.push((name, &first[name.len() + 1..]));
        } else if !SHELL_PREFIXES.contains(first) {
            break;
        }
        rest = tail;
    }
    let Some((command, args)) = rest.split_first() else {
        // Only assignments: they stay set for the following commands.
        for (name, value) in env {
            check_environment(line, name, value)?;
        }
        return Ok(None);
    };
    ensure!(
        plain_command_word(command),
        "`{line}` runs `{command}`, a command word with an expansion, quote, bracket or backslash that could spell cargo or rustc; write the command as a plain word, or teach `cargo_builds`"
    );
    // `export NAME=value` and its relatives set variables for what follows.
    if matches!(
        *command,
        "export" | "declare" | "typeset" | "readonly" | "local"
    ) {
        for arg in args {
            if let Some(name) = is_assignment(arg) {
                check_environment(line, name, &arg[name.len() + 1..])?;
            }
        }
    }
    if PACKAGE_MANAGERS.contains(&basename(command)) {
        return Ok(None);
    }
    if !mentions_toolchain(&words) {
        return Ok(None);
    }
    ensure!(
        basename(command) == "cargo",
        "`{line}` runs `{command}` with cargo or rustc among its words; only a plain `cargo build -p …` is modeled, teach `cargo_builds` first"
    );
    for (name, value) in env {
        check_environment(line, name, value)?;
    }
    let mut args = args;
    // `cargo +toolchain build`.
    if let Some((toolchain, tail)) = args.split_first()
        && toolchain.starts_with('+')
    {
        args = tail;
    }
    let Some((subcommand, args)) = args.split_first() else {
        bail!("`{line}` runs cargo without a subcommand");
    };
    match *subcommand {
        "build" | "b" => parse_cargo_build(line, args).map(Some),
        // They compile nothing into the image.
        "fetch" | "vendor" | "clean" | "metadata" | "tree" | "version" | "--version" | "-V" => {
            Ok(None)
        }
        other => bail!(
            "`{line}` runs `cargo {other}`, which can build what the image ships in a way the fingerprint's crate closure does not model; teach `cargo_builds` first"
        ),
    }
}

/// The `cargo build -p …` commands of a Containerfile. Fails when there is
/// none, rather than hash nothing, and on any other way of running cargo or
/// rustc: a build line this does not fully parse could ship crates the
/// fingerprint would then not hash. (A `RUN` that hands the build to a script
/// is invisible here; scripts under `build/` are hashed whole, not parsed.)
fn cargo_builds(containerfile: &str) -> Result<Vec<CargoBuild>> {
    let mut builds = Vec::new();
    for (number, line) in logical_lines(containerfile) {
        let (instruction, rest) = line.split_once(char::is_whitespace).unwrap_or((&line, ""));
        // Case-sensitive on purpose: a lowercase `env cargo build …` in a
        // heredoc is shell, not an `ENV` instruction.
        if matches!(instruction, "ENV" | "ARG") {
            check_environment_instruction(&line, rest)
                .with_context(|| format!("Containerfile line {number}"))?;
        }
        if NON_SHELL_INSTRUCTIONS.contains(&instruction) {
            continue;
        }
        let mut shell = if instruction == "RUN" { rest } else { &line };
        if instruction == "RUN" {
            // `RUN --mount=… --network=… command`.
            while let Some(stripped) = shell.trim_start().strip_prefix("--") {
                shell = stripped
                    .split_once(char::is_whitespace)
                    .map_or("", |(_, r)| r);
            }
            // `RUN ["cargo", "build", …]` runs no shell; its JSON is not
            // parsed, so it must not mention the toolchain at all.
            if shell.trim_start().starts_with('[') {
                ensure!(
                    !shell.contains("cargo")
                        && !shell.contains("rustc")
                        && !shell.contains("rustup")
                        && !shell.contains("rustdoc"),
                    "Containerfile line {number}: `{line}` is an exec-form RUN that mentions cargo or rustc; use a shell-form `RUN cargo build -p …`"
                );
            }
        }
        let (commands, balanced) = simple_commands(shell);
        if !balanced {
            ensure!(
                !(line.contains("cargo")
                    || line.contains("rustc")
                    || line.contains("rustup")
                    || line.contains("rustdoc")),
                "Containerfile line {number}: `{line}` ends inside a quote or substitution and mentions cargo or rustc; the fingerprint's parser does not follow it, teach `cargo_builds` first"
            );
        }
        for words in commands {
            let words: Vec<&str> = words.iter().map(String::as_str).collect();
            if let Some(build) = parse_command(&line, &words)
                .with_context(|| format!("Containerfile line {number}"))?
            {
                builds.push(build);
            }
        }
    }
    ensure!(
        !builds.is_empty(),
        "the Containerfile has no `cargo build -p …` line; the root image's crates cannot be derived"
    );
    builds.sort();
    builds.dedup();
    Ok(builds)
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
    if build.all_features {
        metadata.arg("--all-features");
    } else if build.no_default_features {
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
/// dependency closure, derived from `cargo metadata`, of every package the root
/// `Containerfile` builds (not a list kept by hand, which would rot). An edit
/// to any other crate (the installer, the e2e harness, the development
/// tooling) cannot reach the image and no longer changes its fingerprint.
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
    let containerfile = std::fs::read_to_string(ws.root.join("Containerfile"))
        .context("reading the root Containerfile")?;
    let builds = cargo_builds(&containerfile)?;
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

fn build(packages: &[&str], no_default_features: bool, all_features: bool) -> CargoBuild {
    CargoBuild {
        packages: packages.iter().map(|p| (*p).to_owned()).collect(),
        no_default_features,
        all_features,
    }
}

type ParserFixture = (
    &'static str,
    &'static str,
    Result<Vec<CargoBuild>, &'static str>,
);

/// Containerfile fixtures: `Ok(builds)` must parse to exactly those builds,
/// `Err(text)` must be refused with an error containing `text`.
fn parser_fixtures() -> Vec<ParserFixture> {
    vec![
        (
            "heredoc with mount flags and a wrapper export",
            r#"
# cargo is mentioned in a comment
FROM fedora AS builder
RUN dnf -y install --setopt=keepcache=1 cargo gcc
COPY --from=builder /usr/bin/cargo /usr/local/bin/cargo
RUN --mount=type=cache,id=x-cargo,target=/root/.cargo/registry \
    --mount=type=cache,id=x-kache,target=/root/.cache/kache <<'EOF'
set -e
export RUSTC_WRAPPER=kache
cargo build --release --locked -p ilexctl --no-default-features
cargo build --release --locked -p drupe -p ilex-enroll   # trailing comment
kache stats --last-build
EOF
"#,
            Ok(vec![
                build(&["drupe", "ilex-enroll"], false, false),
                build(&["ilexctl"], true, false),
            ]),
        ),
        (
            "continuations, a comment inside one, RUN prefix, chain and toolchain",
            r#"
RUN set -e && CARGO_INCREMENTAL=0 /root/.cargo/bin/cargo +stable build \
      # a comment inside a continuation
      --release -j 4 \
      -p a -p=b -pc --package d --package=e && echo done
"#,
            Ok(vec![build(&["a", "b", "c", "d", "e"], false, false)]),
        ),
        (
            "attached and spaced feature flags widen",
            "RUN cargo build -p a -Ffoo\nRUN cargo build -p b -F foo\nRUN cargo build -p c --features=foo\nRUN cargo build -p d --all-features\n",
            Ok(vec![
                build(&["a"], false, true),
                build(&["b"], false, true),
                build(&["c"], false, true),
                build(&["d"], false, true),
            ]),
        ),
        (
            "env prefix, shell keyword and a non-building subcommand",
            "RUN if RUSTC_WRAPPER=kache cargo build -p a; then cargo fetch --locked; fi\n",
            Ok(vec![build(&["a"], false, false)]),
        ),
        (
            "cargo install",
            "RUN cargo install --path crates/x\nRUN cargo build -p a\n",
            Err("cargo install"),
        ),
        (
            "cargo rustc",
            "RUN cargo rustc -p a -- -C lto\n",
            Err("cargo rustc"),
        ),
        (
            "cargo test",
            "RUN cargo build -p a && cargo test -p a\n",
            Err("cargo test"),
        ),
        (
            "bare rustc",
            "RUN cargo build -p a && rustc x.rs\n",
            Err("rustc"),
        ),
        (
            "absolute rustc",
            "RUN /usr/bin/rustc x.rs\nRUN cargo build -p a\n",
            Err("rustc"),
        ),
        (
            "no package",
            "RUN cargo build --release\n",
            Err("names no package"),
        ),
        (
            "workspace",
            "RUN cargo build -p a --workspace\n",
            Err("--workspace"),
        ),
        (
            "exclude",
            "RUN cargo build -p a --exclude b\n",
            Err("--exclude"),
        ),
        ("bin", "RUN cargo build -p a --bin x\n", Err("--bin")),
        (
            "unknown flag",
            "RUN cargo build -p a --frobnicate\n",
            Err("--frobnicate"),
        ),
        (
            "manifest path",
            "RUN cargo build -p a --manifest-path other/Cargo.toml\n",
            Err("--manifest-path"),
        ),
        (
            "target",
            "RUN cargo build -p a --target aarch64-unknown-linux-gnu\n",
            Err("--target"),
        ),
        ("positional", "RUN cargo build -p a extra\n", Err("extra")),
        ("passthrough", "RUN cargo build -p a -- -x\n", Err("`--`")),
        (
            "-p at the end",
            "RUN cargo build -p\n",
            Err("ends after -p"),
        ),
        (
            "-F at the end",
            "RUN cargo build -p a -F\n",
            Err("ends after -F"),
        ),
        (
            "variable package",
            "RUN cargo build -p $PKG\n",
            Err("shell expansion"),
        ),
        (
            "quoted package",
            "RUN cargo build -p 'a'\n",
            Err("shell expansion"),
        ),
        (
            "glob package",
            "RUN cargo build -p 'a*'\n",
            Err("shell expansion"),
        ),
        (
            "versioned package",
            "RUN cargo build -p a@1.0\n",
            Err("plain package name"),
        ),
        (
            "command substitution",
            "RUN $(echo cargo) build -p a\nRUN cargo build -p b\n",
            Err("echo"),
        ),
        ("sh -c", "RUN sh -c 'cargo build -p a'\n", Err("sh")),
        ("xargs", "RUN echo a | xargs cargo build -p\n", Err("xargs")),
        ("wrapper", "RUN nice cargo build -p a\n", Err("nice")),
        (
            "env wrapper flag",
            "RUN env -i cargo build -p a\n",
            Err("-i"),
        ),
        (
            "toolchain flag",
            "RUN cargo +nightly -Z build-std build -p a\n",
            Err("-Z"),
        ),
        (
            "cargo env",
            "RUN CARGO_BUILD_TARGET=x cargo build -p a\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "joined by a continuation",
            "RUN cargo \\\n    build -p a \\\n    --workspace\n",
            Err("--workspace"),
        ),
        (
            "lowercase env in a heredoc",
            "RUN <<'EOF'\nenv cargo install foo\nEOF\nRUN cargo build -p a\n",
            Err("cargo install"),
        ),
        (
            "subcommand missing",
            "RUN cargo\n",
            Err("without a subcommand"),
        ),
        (
            "no cargo build at all",
            "RUN dnf -y install cargo\n",
            Err("no `cargo build"),
        ),
        (
            "redirections keep their neighbours' arguments",
            "RUN cargo build -p a 2>build.log -p b >&2 -p c &>>all.log -p d >| out -p e 2>&1 --release\n",
            Ok(vec![build(&["a", "b", "c", "d", "e"], false, false)]),
        ),
        (
            "leading and spaced redirections, a redirection's ampersand",
            "RUN >log 2>&1 cargo build -p a > out.txt 2> err.txt --locked && echo ok >&2\n",
            Ok(vec![build(&["a"], false, false)]),
        ),
        (
            "separators and expansions inside quotes",
            r##"RUN echo "${SUM}  x; y" | sha256sum -c - && mkdir -p "/out$(dirname "$c")" && systemctl set-default "$(python3 -c 'print("a;b")')" && if [ -f /x ]; then echo '(' ; fi && cargo build -p a
"##,
            Ok(vec![build(&["a"], false, false)]),
        ),
        (
            "harmless environment in instructions and exports",
            "ENV CARGO_HOME=/x CARGO_INCREMENTAL=0\nARG CARGO_TARGET_DIR=/t\nRUN export CARGO_TERM_COLOR=always RUSTFLAGS=-Dwarnings && cargo build -p a\n",
            Ok(vec![build(&["a"], false, false)]),
        ),
        (
            "a flag after a redirection",
            "RUN cargo build -p a 2>log --workspace\n",
            Err("--workspace"),
        ),
        (
            "an unknown flag after a combined redirection",
            "RUN cargo build -p a &>log --frobnicate\n",
            Err("--frobnicate"),
        ),
        (
            "a positional argument after a descriptor copy",
            "RUN cargo build -p a 2>&1 extra\n",
            Err("extra"),
        ),
        (
            "cargo behind a redirection",
            "RUN echo >&2 cargo install x\nRUN cargo build -p a\n",
            Err("echo"),
        ),
        (
            "exec-form cargo",
            "RUN [\"cargo\", \"install\", \"x\"]\nRUN cargo build -p a\n",
            Err("exec-form"),
        ),
        (
            "exec-form absolute cargo",
            "RUN [\"/usr/bin/cargo\", \"build\", \"-p\", \"a\"]\n",
            Err("exec-form"),
        ),
        (
            "exec-form rustc",
            "RUN [\"rustc\", \"x.rs\"]\nRUN cargo build -p a\n",
            Err("exec-form"),
        ),
        (
            "variable as the command",
            "RUN $CARGO install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "quoted variable as the command",
            "RUN \"$CARGO\" install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "braced variable as the command",
            "RUN ${CARGO} install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "backticks",
            "RUN `which cargo` install b\nRUN cargo build -p a\n",
            Err("which"),
        ),
        (
            "quotes inside the command word",
            "RUN c\"arg\"o install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "backslash inside the command word",
            "RUN car\\go install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "glob as the command",
            "RUN /usr/bin/car?o install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "brace expansion as the command",
            "RUN {cargo,} install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "bracket glob as the command",
            "RUN [c]argo install b\nRUN cargo build -p a\n",
            Err("command word"),
        ),
        (
            "ENV sets a cargo variable",
            "ENV CARGO_BUILD_TARGET=aarch64-unknown-linux-gnu\nRUN cargo build -p a\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "legacy ENV form",
            "ENV CARGO_PROFILE_RELEASE_LTO fat\nRUN cargo build -p a\n",
            Err("CARGO_PROFILE_RELEASE_LTO"),
        ),
        (
            "ENV after a quoted value",
            "ENV A=\"x y\" CARGO_BUILD_TARGET=z\nRUN cargo build -p a\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "ARG names a cargo variable",
            "ARG CARGO_BUILD_TARGET\nRUN cargo build -p a\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "ENV RUSTFLAGS with --cfg",
            "ENV RUSTFLAGS=\"-C opt-level=3 --cfg foo\"\nRUN cargo build -p a\n",
            Err("RUSTFLAGS"),
        ),
        (
            "export of a cargo variable",
            "RUN export CARGO_BUILD_TARGET=x && cargo build -p a\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "declare of a cargo variable",
            "RUN declare -x CARGO_BUILD_TARGET=x; cargo build -p a\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "export of RUSTFLAGS with --cfg",
            "RUN export RUSTFLAGS='--cfg foo'; cargo build -p a\n",
            Err("RUSTFLAGS"),
        ),
        (
            "an assignment on a line of its own",
            "RUN <<'EOF'\nCARGO_BUILD_TARGET=x\ncargo build -p a\nEOF\n",
            Err("CARGO_BUILD_TARGET"),
        ),
        (
            "a quote left open before cargo",
            "RUN echo 'x; cargo install y\nRUN cargo build -p a\n",
            Err("ends inside a quote"),
        ),
    ]
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

/// Negative and positive fixtures for the Containerfile parser, the closure
/// walk (local path packages, registry/git packages, dev edges) and the record
/// the signing process reads. Run by `ilexctl doctor --self-check`; no unit
/// tests, as everywhere in this repository.
pub fn self_check() -> Result<serde_json::Value> {
    let mut accepted = 0;
    let mut refused = 0;
    for (name, text, expect) in parser_fixtures() {
        match (cargo_builds(text), expect) {
            (Ok(mut got), Ok(mut want)) => {
                got.sort();
                want.sort();
                ensure!(
                    got == want,
                    "parser fixture `{name}`: got {got:?}, want {want:?}"
                );
                accepted += 1;
            }
            (Err(error), Err(want)) => {
                let error = format!("{error:#}");
                ensure!(
                    error.contains(want),
                    "parser fixture `{name}` failed with `{error}`, which does not contain `{want}`"
                );
                refused += 1;
            }
            (Ok(got), Err(want)) => {
                bail!("parser fixture `{name}` parsed to {got:?}; it must fail closed (`{want}`)")
            }
            (Err(error), Ok(_)) => bail!("parser fixture `{name}` was refused: {error:#}"),
        }
    }

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
    let wanted = build(&["app"], false, false);
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
            &build(&["nothere"], false, false)
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
        "containerfile_fixtures_parsed": accepted,
        "containerfile_fixtures_refused": refused,
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
