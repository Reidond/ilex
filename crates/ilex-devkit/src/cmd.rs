//! Running external tools (podman, bcvk, skopeo, ...) with the command line
//! echoed to stderr, so logs show exactly what was run.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Render a command for logs: `podman build --tag localhost/ilex:main .`
pub fn display(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        s.push(' ');
        s.push_str(&quote(arg));
    }
    s
}

fn quote(arg: &OsStr) -> String {
    let a = arg.to_string_lossy();
    if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c)) {
        a.into_owned()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

/// Run to completion with inherited stdio; fail on a non-zero exit.
pub fn run(cmd: &mut Command) -> Result<()> {
    let line = display(cmd);
    eprintln!("$ {line}");
    let status = cmd.status().with_context(|| format!("spawning `{line}`"))?;
    if !status.success() {
        bail!("`{line}` failed with {status}");
    }
    Ok(())
}

/// Run and capture stdout (trimmed); stderr passes through. Fails on a
/// non-zero exit.
pub fn output(cmd: &mut Command) -> Result<String> {
    let line = display(cmd);
    eprintln!("$ {line}");
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("spawning `{line}`"))?;
    if !out.status.success() {
        bail!("`{line}` failed with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Like [`output`] but quiet and returning `None` on failure — for probes
/// such as "does this image exist?".
pub fn probe(cmd: &mut Command) -> Option<String> {
    let out = cmd.stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Fail early with a useful hint when a required tool is missing.
pub fn require(tool: &str) -> Result<()> {
    if which(tool) {
        return Ok(());
    }
    let hint = if cfg!(windows) {
        " — on Windows run xtask inside the WSL2 Linux box (see CLAUDE.md)"
    } else {
        ""
    };
    bail!("`{tool}` not found on PATH{hint}")
}

fn which(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let p = dir.join(tool);
        p.is_file() || (cfg!(windows) && p.with_extension("exe").is_file())
    })
}
