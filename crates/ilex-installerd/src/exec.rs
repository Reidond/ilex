//! Running the packaged tools: a clean environment, a deadline, captured output.
//! Nothing here takes a command from the bus.

use anyhow::{Context, Result, bail};
use std::{
    io::Read,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

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

/// Run `program` to the end or until `timeout`. A nonzero exit is not an
/// error here: the caller knows what each tool's failure means.
pub fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
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
    let reader = |mut pipe: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes);
            bytes
        })
    };
    let stdout = reader(Box::new(child.stdout.take().context("stdout")?));
    let stderr = reader(Box::new(child.stderr.take().context("stderr")?));
    let deadline = Instant::now() + timeout;
    let status = loop {
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
    Ok(Output {
        code: status.code(),
        stdout: stdout.join().unwrap_or_default(),
        stderr: String::from_utf8_lossy(&stderr.join().unwrap_or_default())
            .trim()
            .to_owned(),
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
