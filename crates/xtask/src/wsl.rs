//! On a Windows host, `cargo xtask vm` runs here first and hands itself to
//! the WSL2 distro that holds the VM stack (see CLAUDE.md).
//!
//! With `--gui` it first makes sure WSLg can draw the window
//! ([`ilex_devkit::wslg`]). WSLg decides at each start of WSL whether it has
//! shared memory with Windows, and only a new start can change that. So:
//! hold WSL up with a keep-alive process (WSL stops an instance with nothing
//! running in it, and the next start rolls the dice again), read Weston's
//! log, and while WSLg is in copy mode restart WSL (`wsl --shutdown`), but
//! only when nothing else runs in WSL, unless `--restart-wsl`. Then run
//! `cargo xtask vm …` in the checkout inside the distro while the keep-alive
//! still holds.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_devkit::wslg::{self, State};

/// How often to restart WSL before giving up on WSLg.
const ATTEMPTS: u32 = 6;

/// Programs in WSL that a restart would kill mid-work.
const BUSY: &[&str] = &[
    "qemu-system-x86",
    "podman",
    "buildah",
    "conmon",
    "bcvk",
    "cargo",
    "rustc",
    "skopeo",
    "image-builder",
    "osbuild",
];

/// The distro with the VM stack: `ILEX_WSL_DISTRO`, or `FedoraLinux-45`.
fn distro() -> String {
    std::env::var("ILEX_WSL_DISTRO")
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| "FedoraLinux-45".into())
}

/// Run this `cargo xtask` command line in the distro, from `root` (the
/// checkout, a Windows path), after making sure WSLg works when `gui`.
pub fn forward(root: &Path, gui: bool, restart_wsl: bool) -> Result<()> {
    let distro = distro();
    let keep = if gui {
        Some(healthy_wslg(&distro, restart_wsl)?)
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
    let mut wsl = Command::new("wsl.exe");
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
    let status = wsl.status().context("spawning wsl.exe")?;
    drop(keep);
    if !status.success() {
        bail!("`{line}` in WSL failed with {status}");
    }
    Ok(())
}

/// Get WSL to a start in which WSLg is healthy and return the keep-alive
/// that holds it there.
fn healthy_wslg(distro: &str, restart_wsl: bool) -> Result<KeepAlive> {
    let mut keep = KeepAlive::start(distro)?;
    let mut state = wait_for_weston(distro)?;
    if state == State::Broken {
        let busy = busy(distro);
        if !busy.is_empty() && !restart_wsl {
            bail!(
                "WSLg is in copy mode in this start of WSL, so the VM's window would stay invisible \
                 (microsoft/WSL#40618), and only restarting WSL fixes it. Not restarting: that would also \
                 stop {}. Run again once that's done, or with --restart-wsl.",
                busy.join(", ")
            );
        }
        for attempt in 1..=ATTEMPTS {
            eprintln!(
                "WSLg is in copy mode (windows would stay invisible): restarting WSL ({attempt}/{ATTEMPTS})"
            );
            drop(keep);
            let status = Command::new("wsl.exe")
                .arg("--shutdown")
                .status()
                .context("spawning wsl.exe --shutdown")?;
            if !status.success() {
                bail!("`wsl.exe --shutdown` failed with {status}");
            }
            keep = KeepAlive::start(distro)?;
            state = wait_for_weston(distro)?;
            if state != State::Broken {
                break;
            }
        }
    }
    match state {
        State::Healthy => {
            eprintln!("WSLg is healthy (shared memory with Windows)");
            Ok(keep)
        }
        State::Broken => bail!(
            "WSLg was still in copy mode after {ATTEMPTS} restarts of WSL; try `wsl --shutdown` later"
        ),
        State::Starting => bail!(
            "WSLg didn't start in {distro} ({} never logged its settings); is `guiApplications=false` in .wslconfig?",
            wslg::LOG
        ),
    }
}

/// Read Weston's log until it says whether WSLg got its shared memory.
fn wait_for_weston(distro: &str) -> Result<State> {
    let started = Instant::now();
    loop {
        let out = Command::new("wsl.exe")
            .args(["-d", distro, "-u", "root", "-e", "cat", wslg::LOG])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .context("spawning wsl.exe")?;
        let state = if out.status.success() {
            wslg::state_from_log(&String::from_utf8_lossy(&out.stdout))
        } else {
            State::Starting
        };
        if state != State::Starting || started.elapsed() > Duration::from_secs(60) {
            return Ok(state);
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// What `wsl --shutdown` would stop besides WSLg: other running distros and
/// work in this one (builds, VMs).
fn busy(distro: &str) -> Vec<String> {
    let mut busy = Vec::new();
    if let Ok(out) = Command::new("wsl.exe")
        .args(["--list", "--running", "--quiet"])
        .env("WSL_UTF8", "1")
        .stdin(Stdio::null())
        .output()
    {
        let list = String::from_utf8_lossy(&out.stdout).replace('\0', "");
        busy.extend(
            list.lines()
                .map(str::trim)
                .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(distro))
                .map(|d| format!("the {d} distro")),
        );
    }
    if let Ok(out) = Command::new("wsl.exe")
        .args(["-d", distro, "-u", "root", "-e", "ps", "-eo", "comm="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        let mut progs: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::trim)
            .filter(|c| BUSY.iter().any(|b| c.starts_with(b)))
            .map(String::from)
            .collect();
        progs.sort();
        progs.dedup();
        busy.extend(progs);
    }
    busy
}

/// A `sleep` in the distro: WSL keeps an instance running while a process
/// does, so the WSLg start that was checked is the one the VM gets.
struct KeepAlive(Child);

impl KeepAlive {
    fn start(distro: &str) -> Result<Self> {
        let child = Command::new("wsl.exe")
            .args(["-d", distro, "-u", "root", "-e", "sleep", "infinity"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning wsl.exe")?;
        Ok(KeepAlive(child))
    }
}

impl Drop for KeepAlive {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
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
