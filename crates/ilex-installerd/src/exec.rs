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

// --- tools that run for a long time -------------------------------------------

/// A tool of the installation itself: it can run for minutes (a pull), prints
/// progress as it goes, may be fed one secret on its standard input and is
/// stopped, process group and all, when it overruns its deadline.
pub struct Job<'a> {
    pub program: &'a str,
    pub args: &'a [&'a str],
    /// Added to the clean environment every tool gets.
    pub env: &'a [(&'a str, &'a str)],
    /// Written to the tool's standard input, which is then closed. Never an
    /// argument: arguments are visible in `/proc` and in logs.
    pub stdin: Option<&'a [u8]>,
    /// Descriptors the tool inherits under their own numbers (a key file,
    /// read through `/proc/self/fd/N`): close-on-exec in this process, and
    /// made inheritable in the child alone.
    pub inherit: &'a [std::os::fd::RawFd],
    pub timeout: Duration,
}

/// What a line sink says about the job it is reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    Continue,
    /// Stop the job now: what it was run to find out is known.
    Stop,
}

/// How a job ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The exit status, or `None` when a signal ended it.
    pub code: Option<i32>,
    /// The sink stopped it ([`Flow::Stop`]).
    pub stopped: bool,
}

impl Exit {
    pub fn success(self) -> bool {
        self.code == Some(0)
    }

    pub fn describe(self) -> String {
        self.code.map_or_else(
            || "was killed by a signal".into(),
            |c| format!("exited {c}"),
        )
    }
}

/// A line of a job's output made safe to keep and show: no control characters,
/// bounded. (Progress bars end their lines with `\r`: each is a line.)
pub fn clean_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|c| !c.is_control())
        .take(300)
        .collect()
}

/// Run a job to the end. Its standard output and error arrive, merged and in
/// order, as lines for `line`; `tick` is called about once a second while it
/// runs (for progress that is measured, not printed). The child dies with the
/// calling thread's process (`PR_SET_PDEATHSIG`): a helper that is killed
/// takes the tools that write to the disk with it.
pub fn run_job(
    job: &Job<'_>,
    line: &mut dyn FnMut(&str) -> Flow,
    tick: &mut dyn FnMut(),
) -> Result<Exit> {
    use std::{io::Write, os::unix::process::CommandExt, sync::mpsc};
    let (mut reader, writer) = std::io::pipe().context("making a pipe")?;
    let mut command = Command::new(job.program);
    command
        .args(job.args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin")
        .env("LC_ALL", "C.UTF-8")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_PAGER", "")
        .envs(job.env.iter().copied())
        .stdin(if job.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(writer.try_clone().context("duplicating the pipe")?)
        .stderr(writer)
        .process_group(0);
    let inherit = job.inherit.to_vec();
    // SAFETY: only async-signal-safe `prctl` and `fcntl` run between fork and
    // exec (the list of descriptors was made before the fork).
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for &fd in &inherit {
                crate::luks::inheritable(fd)?;
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("starting {}", job.program))?;
    // The parent's copies of the write end are inside `command`: drop them so
    // the reader sees the end of the output when the child is gone.
    drop(command);
    if let (Some(bytes), Some(mut stdin)) = (job.stdin, child.stdin.take()) {
        // Small (a password line) and the tool reads it first thing: the pipe
        // buffer holds it, so this does not wait for the tool.
        let _ = stdin.write_all(bytes);
    }
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();
    let pump = thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    for &byte in &chunk[..n] {
                        if byte == b'\n' || byte == b'\r' {
                            if !pending.is_empty() {
                                let _ = sender.send(std::mem::take(&mut pending));
                            }
                        } else if pending.len() < 4096 {
                            pending.push(byte);
                        }
                    }
                }
            }
        }
        if !pending.is_empty() {
            let _ = sender.send(pending);
        }
    });
    let pgid = child.id() as libc::pid_t;
    let deadline = Instant::now() + job.timeout;
    let mut next_tick = Instant::now() + Duration::from_secs(1);
    let mut stopped = false;
    let status = loop {
        while let Ok(bytes) = receiver.recv_timeout(Duration::from_millis(50)) {
            if line(&clean_line(&bytes)) == Flow::Stop {
                stopped = true;
                break;
            }
        }
        if stopped {
            // SAFETY: a signal to the job's own process group.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            break child.wait().context("waiting for the tool")?;
        }
        if let Some(status) = child.try_wait().context("waiting for the tool")? {
            break status;
        }
        if Instant::now() >= next_tick {
            tick();
            next_tick = Instant::now() + Duration::from_secs(1);
        }
        if Instant::now() >= deadline {
            // SAFETY: a signal to the job's own process group.
            unsafe {
                libc::killpg(pgid, libc::SIGKILL);
            }
            let _ = child.wait();
            bail!(
                "{} did not finish within {}s and was stopped",
                job.program,
                job.timeout.as_secs()
            );
        }
    };
    // What the tool printed last, with a bound: a grandchild that keeps the
    // pipe open must not keep the helper waiting.
    let end = Instant::now() + Duration::from_secs(2);
    while !stopped && Instant::now() < end {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(bytes) => {
                if line(&clean_line(&bytes)) == Flow::Stop {
                    stopped = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pump.is_finished() {
                    break;
                }
            }
        }
    }
    // Anything the tool left running in its group goes with it.
    // SAFETY: a signal to the job's own process group.
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    Ok(Exit {
        code: status.code(),
        stopped,
    })
}
