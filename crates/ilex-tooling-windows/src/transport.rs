//! Pinned, argument-free SSH transport for the builder's finite job protocol.
use crate::{
    builder::{self, BuilderEndpoint},
    host,
};
use anyhow::{Context, Result, ensure};
use ilex_tooling_core::{TransferFrame, TransferOperation};
use serde_json::Value;
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

static CANCEL: AtomicBool = AtomicBool::new(false);
unsafe extern "system" fn console_event(event: u32) -> i32 {
    if event == 0 || event == 1 {
        CANCEL.store(true, Ordering::SeqCst);
        1
    } else {
        0
    }
}
pub fn install_cancel_handler() -> Result<()> {
    CANCEL.store(false, Ordering::SeqCst);
    ensure!(
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_event), 1)
        } != 0,
        "installing job cancellation handler: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
pub fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}
pub fn available_space(path: &std::path::Path) -> Result<u64> {
    let ancestor = path
        .ancestors()
        .find(|p| p.is_dir())
        .context("output has no existing volume")?;
    Ok(fs2::available_space(ancestor)?)
}

pub struct Transport {
    pub endpoint: BuilderEndpoint,
}
impl Transport {
    pub fn connect() -> Result<Self> {
        let reply = builder::run(
            builder::Operation::Up,
            builder::Options {
                wait_for_memory_seconds: 10800,
                ..Default::default()
            },
        )?;
        let endpoint: BuilderEndpoint = serde_json::from_value(reply["endpoint"].clone())
            .context("builder did not return an authenticated endpoint")?;
        ilex_tooling_core::check_version(endpoint.schema)?;
        ensure!(
            endpoint.provisioning_revision == builder::revision(),
            "builder runner revision mismatch; run cargo xtask builder update"
        );
        Ok(Self { endpoint })
    }
    pub fn register<Job: serde::Serialize>(&self, request: &Job) -> Result<Value> {
        // Maintenance holds the same installation lock from idle probe to shutdown.
        let _guard = host::lock(&host::root()?.join("builder/host.lock"))?;
        self.rpc(TransferOperation::Submit { request }, &[])
            .map(|r| r.0)
    }
    pub fn rpc<Job: serde::Serialize>(
        &self,
        operation: TransferOperation<'_, Job>,
        payload: &[u8],
    ) -> Result<(Value, Vec<u8>)> {
        let request = TransferFrame {
            schema: 1,
            payload_bytes: payload.len(),
            operation,
        };
        ensure!(
            payload.len() <= ilex_tooling_core::CHUNK_BYTES,
            "oversized transfer chunk"
        );
        let endpoint = &self.endpoint;
        let mut ssh = Command::new("ssh.exe");
        use std::os::windows::process::CommandExt;
        ssh.creation_flags(0x08000000); // CREATE_NO_WINDOW: Ctrl-C belongs to xtask.
        ssh.args(["-T", "-F", "NUL", "-i"])
            .arg(&endpoint.identity_file)
            .args([
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "GlobalKnownHostsFile=NUL",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ConnectTimeout=10",
                "-o",
                "ServerAliveInterval=10",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "LogLevel=ERROR",
            ])
            .arg("-o")
            .arg(format!(
                "UserKnownHostsFile={}",
                endpoint.known_hosts_file.display()
            ))
            .arg("-o")
            .arg(format!("HostKeyAlias={}", endpoint.host_key_alias))
            .arg(format!("{}@{}", endpoint.user, endpoint.address))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // There is deliberately no SSH remote-command argument.
        let (_job, mut child) = host::spawn_owned(&mut ssh)?;
        let stdout = child.stdout.take().context("SSH stdout")?;
        let stderr = child.stderr.take().context("SSH stderr")?;
        let out = thread::spawn(move || {
            let mut b = Vec::new();
            stdout.take(80 * 1024 * 1024).read_to_end(&mut b).map(|_| b)
        });
        let err = thread::spawn(move || {
            let mut b = Vec::new();
            stderr.take(1024 * 1024).read_to_end(&mut b).map(|_| b)
        });
        {
            let mut input = child.stdin.take().context("SSH stdin")?;
            input.write_all(b"job\n")?;
            serde_json::to_writer(&mut input, &request)?;
            input.write_all(b"\n")?;
            input.write_all(payload)?;
        }
        let deadline = Instant::now() + Duration::from_secs(180);
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() > deadline {
                child.kill()?;
                let _ = child.wait();
                anyhow::bail!("builder RPC timed out; job retained for resume");
            }
            thread::sleep(Duration::from_millis(25));
        };
        let bytes = out
            .join()
            .map_err(|_| anyhow::anyhow!("SSH output reader panicked"))??;
        let diagnostic = err
            .join()
            .map_err(|_| anyhow::anyhow!("SSH diagnostic reader panicked"))??;
        let newline = bytes.iter().position(|b| *b == b'\n').with_context(|| {
            format!(
                "SSH disconnected ({status}): {}",
                String::from_utf8_lossy(&diagnostic)
            )
        })?;
        let reply: Value =
            serde_json::from_slice(&bytes[..newline]).context("invalid builder protocol frame")?;
        ensure!(reply["schema"] == 1, "builder protocol version mismatch");
        ensure!(
            reply["ok"] == true && status.success(),
            "builder RPC refused: {} {}",
            reply["error"],
            String::from_utf8_lossy(&diagnostic)
        );
        let data = bytes[newline + 1..].to_vec();
        ensure!(
            reply["payload_bytes"].as_u64() == Some(data.len() as u64),
            "truncated builder reply"
        );
        Ok((reply["value"].clone(), data))
    }
}
