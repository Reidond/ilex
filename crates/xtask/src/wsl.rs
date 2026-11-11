//! On a Windows host, `cargo xtask vm` runs here first and hands itself to
//! the WSL2 distro that holds the VM stack (see CLAUDE.md).
//!
//! With `--gui` it first builds `ilex-viewer.exe` (the native window) next
//! to this xtask binary. The WSL side boots the VM and its display bridge
//! and prints the viewer's arguments on a line of its own
//! ([`crate::vm::VIEWER_LINE`]); this reads its stderr, starts the viewer
//! with them, and passes everything else through.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result, bail};

use crate::vm::VIEWER_LINE;

/// The distro with the VM stack: `ILEX_WSL_DISTRO`, or `FedoraLinux-45`.
fn distro() -> String {
    std::env::var("ILEX_WSL_DISTRO")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "FedoraLinux-45".into())
}

/// Run this `cargo xtask` command line in the distro, from `root` (the
/// checkout, a Windows path).
pub fn forward(root: &Path, gui: bool) -> Result<()> {
    let distro = distro();
    let mut wsl = Command::new("wsl.exe");

    let viewer = if gui {
        ilex_devkit::cmd::run(
            Command::new("cargo")
                .args(["build", "-q", "-p", "ilex-viewer"])
                .current_dir(root),
        )
        .context("building ilex-viewer")?;
        let exe = std::env::current_exe()?.with_file_name("ilex-viewer.exe");
        if !exe.is_file() {
            bail!(
                "{} not found after `cargo build -p ilex-viewer`",
                exe.display()
            );
        }
        // Tell the WSL side that the window is started here (and pass the
        // frame-rate readout switch on to the bridge).
        let mut wslenv = std::env::var("WSLENV").unwrap_or_default();
        for var in ["ILEX_VIEWER_ON_WINDOWS", "ILEX_VIEWER_STATS"] {
            if !wslenv.is_empty() {
                wslenv.push(':');
            }
            wslenv.push_str(var);
        }
        wsl.env("ILEX_VIEWER_ON_WINDOWS", "1")
            .env("WSLENV", wslenv)
            .stderr(Stdio::piped());
        Some(exe)
    } else {
        None
    };

    let args: Vec<String> = std::env::args().skip(1).collect();
    let line = format!(
        "cargo xtask {}",
        args.iter()
            .map(|a| sh_quote(a))
            .collect::<Vec<_>>()
            .join(" ")
    );
    // `--cd` takes the Windows path; `bash -l` reads /etc/profile.d (the
    // target dir on the Linux filesystem).
    wsl.args(["-d", &distro, "-u", "root", "--cd"])
        .arg(root)
        .args(["-e", "bash", "-lc", &line]);
    eprintln!(
        "$ wsl.exe -d {distro} -u root --cd {} -e bash -lc {}",
        root.display(),
        sh_quote(&line)
    );
    let mut child = wsl.spawn().context("spawning wsl.exe")?;

    let window: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let relay = match (viewer, child.stderr.take()) {
        (Some(exe), Some(stderr)) => Some(relay_stderr(stderr, exe, Arc::clone(&window))),
        _ => None,
    };
    let status = child.wait().context("waiting for wsl.exe")?;
    if let Some(relay) = relay {
        let _ = relay.join();
    }
    if let Some(mut w) = window.lock().unwrap().take() {
        let _ = w.kill();
    }
    if !status.success() {
        bail!("`{line}` in WSL failed with {status}");
    }
    Ok(())
}

/// Pass the WSL side's stderr through and start the viewer when it says so.
fn relay_stderr(
    stderr: std::process::ChildStderr,
    exe: PathBuf,
    window: Arc<Mutex<Option<Child>>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            eprintln!("{line}");
            if let Some(args) = line.strip_prefix(VIEWER_LINE) {
                match Command::new(&exe).args(args.split_whitespace()).spawn() {
                    Ok(child) => *window.lock().unwrap() = Some(child),
                    Err(e) => eprintln!("xtask: starting {}: {e}", exe.display()),
                }
            }
        }
    })
}

/// Quote for bash.
fn sh_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', r"'\''"))
    }
}
