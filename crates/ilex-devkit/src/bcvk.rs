//! Driving VMs through the packaged `bcvk` CLI (never the `bcvk-qemu`
//! library) and podman: the plumbing the e2e tiers and xtask share.
//!
//! * [`EphemeralVm`]: `bcvk ephemeral run -d -K` + `bcvk ephemeral ssh`, no
//!   disk. Holds a machine-wide VM slot for as long as it lives and removes
//!   its container on drop, so a panicking test cannot leak a VM.
//!
//! Commands that run over SSH are not echoed (there are dozens per run) and
//! show their output only when they fail.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::cmd;
use crate::slots::{self, VmSlot};

/// Exit code, stdout and stderr of a finished command.
#[derive(Debug, Clone)]
pub struct Output {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Trimmed stdout.
    pub fn out(&self) -> &str {
        self.stdout.trim()
    }

    /// Fail with the full output unless the command exited 0.
    pub fn ok(self, what: &str) -> Result<Self> {
        if self.success() {
            return Ok(self);
        }
        bail!(
            "{what} failed (exit {})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.code
                .map_or_else(|| "signal".to_string(), |c| c.to_string()),
            self.stdout.trim_end(),
            self.stderr.trim_end(),
        )
    }
}

/// Run `command` to completion, capturing its output; kill it and fail after
/// `timeout`. Does not echo the command line.
pub fn capture(command: &mut Command, timeout: Duration) -> Result<Output> {
    let line = cmd::display(command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning `{line}`"))?;
    let out = drain(child.stdout.take().expect("piped stdout"));
    let err = drain(child.stderr.take().expect("piped stderr"));

    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting for `{line}`"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "`{line}` timed out after {}s\n--- stdout ---\n{}\n--- stderr ---\n{}",
                timeout.as_secs(),
                snapshot(&out).trim_end(),
                snapshot(&err).trim_end(),
            );
        }
        thread::sleep(Duration::from_millis(100));
    };
    // The child is gone, so the pipes reach EOF and the readers finish; give
    // them a moment to flush the tail.
    thread::sleep(Duration::from_millis(50));
    Ok(Output {
        code: status.code(),
        stdout: snapshot(&out),
        stderr: snapshot(&err),
    })
}

type Buf = Arc<Mutex<Vec<u8>>>;

fn drain(mut pipe: impl Read + Send + 'static) -> Buf {
    let buf: Buf = Arc::default();
    let sink = Arc::clone(&buf);
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = pipe.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    buf
}

fn snapshot(buf: &Buf) -> String {
    String::from_utf8_lossy(&buf.lock().unwrap()).into_owned()
}

/// Force-remove a container (running or not); quiet, never fails.
pub fn podman_rm(name: &str) {
    let _ = Command::new("podman")
        .args(["rm", "--force", "--time", "0", "--ignore", name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// True if `image` exists in local container storage.
pub fn image_exists(image: &str) -> bool {
    cmd::probe(Command::new("podman").args(["image", "exists", image])).is_some()
}

/// Image ID (the config digest, without `sha256:`) that `reference` currently
/// points at in local storage.
pub fn image_id(reference: &str) -> Result<String> {
    cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Id}}", reference]))
}

/// `sha256` of `path` inside `image`, computed in a throwaway container.
pub fn image_file_sha256(image: &str, path: &str) -> Result<String> {
    let out = cmd::output(Command::new("podman").args([
        "run",
        "--rm",
        "--entrypoint",
        "sha256sum",
        image,
        path,
    ]))?;
    out.split_whitespace()
        .next()
        .map(str::to_string)
        .with_context(|| format!("unexpected sha256sum output: {out}"))
}

/// How an ephemeral VM is sized and started.
#[derive(Debug, Clone)]
pub struct EphemeralOpts {
    pub memory: String,
    pub vcpus: u32,
    /// Extra `bcvk ephemeral run` arguments (e.g. `--karg`, `--bind`).
    pub extra_args: Vec<String>,
    /// How long to wait for the guest to accept SSH.
    pub boot_timeout: Duration,
}

impl Default for EphemeralOpts {
    fn default() -> Self {
        Self {
            memory: "4G".into(),
            vcpus: 4,
            extra_args: Vec::new(),
            boot_timeout: Duration::from_secs(600),
        }
    }
}

/// A running ephemeral VM (the image booted from virtiofs, no disk).
///
/// Dropping it removes the container, which stops QEMU, and frees the VM slot.
#[derive(Debug)]
pub struct EphemeralVm {
    name: String,
    image: String,
    slot: VmSlot,
}

impl EphemeralVm {
    /// Take a VM slot (waiting if the machine is full), boot `image` as
    /// container `name`, and wait until SSH answers. A leftover container of
    /// the same name from a killed run is replaced.
    pub fn boot(name: &str, image: &str, opts: &EphemeralOpts) -> Result<Self> {
        cmd::require("podman")?;
        cmd::require("bcvk")?;
        podman_rm(name);
        let slot = slots::acquire(name)?;
        // Build the guard first: from here on any early return or panic
        // removes whatever `bcvk ephemeral run` managed to create.
        let vm = Self {
            name: name.to_string(),
            image: image.to_string(),
            slot,
        };

        let mut run = Command::new("bcvk");
        run.args(["ephemeral", "run", "--detach", "--ssh-keygen", "--console"])
            .args(["--name", name])
            .args(["--memory", &opts.memory])
            .args(["--vcpus", &opts.vcpus.to_string()])
            .args(&opts.extra_args)
            .arg(image);
        cmd::run(&mut run).with_context(|| format!("starting ephemeral VM {name}"))?;

        vm.wait_ssh(opts.boot_timeout)?;
        Ok(vm)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn image(&self) -> &str {
        &self.image
    }

    /// Index of the VM slot this VM holds.
    pub fn slot_index(&self) -> usize {
        self.slot.index
    }

    /// Run a shell command line in the guest (as root, over SSH). The remote
    /// shell interprets `script`, so pipes and `&&` work.
    pub fn ssh(&self, script: &str) -> Result<Output> {
        self.ssh_timeout(script, Duration::from_secs(180))
    }

    pub fn ssh_timeout(&self, script: &str, timeout: Duration) -> Result<Output> {
        let mut c = Command::new("bcvk");
        c.args(["ephemeral", "ssh", &self.name, "--", script]);
        capture(&mut c, timeout)
    }

    /// ID of the image the VM's container was created from. bcvk boots the
    /// image's own filesystem, so this is the identity of what is running.
    pub fn booted_image_id(&self) -> Result<String> {
        cmd::output(Command::new("podman").args(["inspect", "--format", "{{.Image}}", &self.name]))
    }

    /// Recent console output of the VM (boot messages, kernel, systemd).
    pub fn console_tail(&self, lines: usize) -> String {
        let mut c = Command::new("podman");
        c.args(["logs", "--tail", &lines.to_string(), &self.name]);
        match capture(&mut c, Duration::from_secs(30)) {
            Ok(o) => format!("{}{}", o.stdout, o.stderr),
            Err(e) => format!("(could not read console: {e:#})"),
        }
    }

    fn wait_ssh(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut last = String::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!(
                    "{} did not accept SSH within {}s; last attempt: {last}\n--- console tail ---\n{}",
                    self.name,
                    timeout.as_secs(),
                    self.console_tail(60),
                );
            }
            match self.ssh_timeout("true", left.min(Duration::from_secs(120))) {
                Ok(o) if o.success() => return Ok(()),
                Ok(o) => last = format!("exit {:?}: {}", o.code, o.stderr.trim()),
                Err(e) => last = format!("{e:#}"),
            }
            thread::sleep(Duration::from_secs(2));
        }
    }
}

impl Drop for EphemeralVm {
    fn drop(&mut self) {
        podman_rm(&self.name);
    }
}
