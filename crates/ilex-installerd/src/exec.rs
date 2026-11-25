//! Running the packaged tools: a clean environment, a deadline, captured output
//! with a ceiling. Nothing here takes a command from the bus.

use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

/// What a tool may print before it is stopped: output comes from tools that
/// read disks and the registry, so its size is not the tool's to decide.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub stdout: usize,
    pub stderr: usize,
}

impl Limits {
    /// Plenty for lsblk's JSON of a machine with many disks.
    pub const DEFAULT: Self = Self {
        stdout: 8 << 20,
        stderr: 64 << 10,
    };
    /// A registry manifest is at most 4 MiB (the OCI image specification).
    pub const MANIFEST: Self = Self {
        stdout: 4 << 20,
        stderr: 64 << 10,
    };
}

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// [`run_limited`] with the default ceilings.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    run_limited(program, args, timeout, Limits::DEFAULT)
}

/// Run `program` to the end or until `timeout`. A nonzero exit is not an
/// error here: the caller knows what each tool's failure means. A tool that
/// prints more than `limits` on either stream is killed and the call fails:
/// a truncated document would be read as a complete one.
pub fn run_limited(
    program: &str,
    args: &[&str],
    timeout: Duration,
    limits: Limits,
) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin")
        .env("LC_ALL", "C.UTF-8")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .env("SYSTEMD_LOG_LEVEL", "err")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let overflow = Arc::new(AtomicBool::new(false));
    let reader = |pipe: Box<dyn Read + Send>, limit: usize| {
        let overflow = Arc::clone(&overflow);
        thread::spawn(move || {
            let mut bytes = Vec::new();
            // One byte past the ceiling is how an overflow is told from a
            // tool that printed exactly the ceiling.
            let _ = pipe.take(limit as u64 + 1).read_to_end(&mut bytes);
            if bytes.len() > limit {
                overflow.store(true, Ordering::SeqCst);
            }
            bytes
        })
    };
    let stdout = reader(
        Box::new(child.stdout.take().context("stdout")?),
        limits.stdout,
    );
    let stderr = reader(
        Box::new(child.stderr.take().context("stderr")?),
        limits.stderr,
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if overflow.load(Ordering::SeqCst) {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{program} printed more than its allowed output ({} bytes on stdout, {} on stderr); refusing a truncated answer",
                limits.stdout,
                limits.stderr
            );
        }
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting for {program}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} did not finish within {}s", timeout.as_secs());
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if overflow.load(Ordering::SeqCst) {
        bail!(
            "{program} printed more than its allowed output ({} bytes on stdout, {} on stderr); refusing a truncated answer",
            limits.stdout,
            limits.stderr
        );
    }
    Ok(Output {
        code: status.code(),
        stdout,
        stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
    })
}

/// [`run`], failing with the tool's own message unless it exited 0.
pub fn run_ok(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
    let output = run(program, args, timeout)?;
    if !output.success() {
        bail!(
            "{program} failed (exit {}): {}",
            output
                .code
                .map_or_else(|| "signal".into(), |c| c.to_string()),
            output.stderr.lines().last().unwrap_or("no message")
        );
    }
    Ok(output)
}
