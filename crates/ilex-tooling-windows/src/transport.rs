//! Pinned SSH for the builder's finite job protocol and guest terminal.
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
/// Artifact fetches return up to 64 MiB, independently of 4 MiB source chunks.
pub const MAX_REPLY_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;
pub const OEMDRV_BYTES: usize = 32 * 1024 * 1024;

pub struct OemdrvReadback {
    pub boot: String,
    pub fingerprint: String,
    pub envelope: Vec<u8>,
}
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
pub fn remove_cancel_handler() -> Result<()> {
    ensure!(
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(console_event), 0)
        } != 0,
        "removing job cancellation handler: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
pub fn available_space(path: &std::path::Path) -> Result<u64> {
    let ancestor = path
        .ancestors()
        .find(|p| p.is_dir())
        .context("output has no existing volume")?;
    Ok(fs2::available_space(ancestor)?)
}

pub fn allocated_bytes(path: &std::path::Path) -> Result<u64> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_STANDARD_INFO, FileStandardInfo, GetFileInformationByHandleEx,
    };
    let directory =
        crate::received::Directory::open(path.parent().context("disk parent missing")?)?;
    let file = directory.read(
        path.file_name()
            .and_then(|n| n.to_str())
            .context("disk filename")?,
    )?;
    let mut info: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe {
            GetFileInformationByHandleEx(
                file.file.as_raw_handle().cast(),
                FileStandardInfo,
                (&mut info as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } != 0,
        "reading downloaded disk allocation: {}",
        std::io::Error::last_os_error()
    );
    Ok(u64::try_from(info.AllocationSize)?)
}

pub struct Transport {
    pub endpoint: BuilderEndpoint,
}
impl Transport {
    /// Reuse an already authenticated job endpoint without starting the builder
    /// or taking its maintenance lock. T2's read-only, fail-closed UUID lookup
    /// validates that this is still the same running allocation; SSH stays pinned.
    pub fn existing(mut endpoint: BuilderEndpoint) -> Result<Self> {
        ilex_tooling_core::check_version(endpoint.schema)?;
        let status = builder::run(builder::Operation::Status, Default::default())?;
        ensure!(
            status["state"] == "Running"
                && status["vm_id"] == endpoint.vm_id
                && status["owner"] == endpoint.owner
                && status["name"] == endpoint.host_key_alias
                && status["journal"]["revision"] == endpoint.provisioning_revision,
            "saved job endpoint no longer belongs to the running builder"
        );
        let root =
            std::path::PathBuf::from(status["root"].as_str().context("builder root missing")?);
        ensure!(
            endpoint.user == "ilex"
                && endpoint.identity_file == root.join("identity")
                && endpoint.known_hosts_file == root.join("known_hosts"),
            "saved job endpoint authority mismatch"
        );
        endpoint.address = status["addresses"]
            .as_array()
            .context("builder addresses missing")?
            .iter()
            .filter_map(|v| v.as_str())
            .find(|v| v.parse::<std::net::Ipv4Addr>().is_ok())
            .context("running builder has no IPv4 address")?
            .to_owned();
        Ok(Self { endpoint })
    }
    pub fn connect() -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(10800);
        let mut announced = false;
        let reply = loop {
            let result = builder::run(
                builder::Operation::Up,
                builder::Options {
                    wait_for_memory_seconds: deadline
                        .saturating_duration_since(Instant::now())
                        .as_secs() as u32,
                    ..Default::default()
                },
            );
            match result {
                Ok(reply) => break reply,
                Err(error) if installation_busy(&error) && Instant::now() < deadline => {
                    if !announced {
                        eprintln!("ilex: waiting for the shared builder installation lock");
                        announced = true;
                    }
                    thread::sleep(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        };
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
        let deadline = Instant::now() + Duration::from_secs(10800);
        let _guard = loop {
            ensure!(!cancelled(), "cancelled before remote job registration");
            match host::lock(&host::root()?.join("builder/host.lock")) {
                Ok(guard) => break guard,
                Err(error) if installation_busy(&error) && Instant::now() < deadline => {
                    thread::sleep(Duration::from_secs(2))
                }
                Err(error) => return Err(error),
            }
        };
        self.rpc(TransferOperation::Submit { request }, &[])
            .map(|r| r.0)
    }
    pub fn terminal(&self, owner: &str, job: &str) -> Result<()> {
        use std::io::IsTerminal;

        ensure!(
            std::io::stdin().is_terminal(),
            "ephemeral VM shell needs a terminal"
        );
        // OpenSSH handles Windows console modes and window size negotiation.
        let (_lease, mut child) = host::spawn_owned(self.ssh("-t").args(["terminal", owner, job]))
            .context("opening the builder guest terminal")?;
        let status = child.wait()?;
        ensure!(
            status.success(),
            "builder guest terminal exited with {status}"
        );
        Ok(())
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
        self.exchange(&request, payload)
    }

    pub fn read_oemdrv(&self, raw: &std::path::Path) -> Result<OemdrvReadback> {
        use crate::received::Directory;
        use ilex_tooling_core::digest;
        let parent = Directory::open(raw.parent().context("missing OEMDRV directory")?)?;
        let raw = parent.read(
            raw.file_name()
                .and_then(|name| name.to_str())
                .context("missing OEMDRV filename")?,
        )?;
        ensure!(
            raw.file.metadata()?.len() == OEMDRV_BYTES as u64,
            "invalid OEMDRV size"
        );
        let mut payload = Vec::with_capacity(OEMDRV_BYTES);
        raw.file
            .take(OEMDRV_BYTES as u64 + 1)
            .read_to_end(&mut payload)?;
        ensure!(payload.len() == OEMDRV_BYTES, "OEMDRV size changed");
        let request = serde_json::json!({
            "schema":1, "payload_bytes":payload.len(),
            "operation":"read_oemdrv", "sha256":digest(&payload)
        });
        let (reply, envelope) = self.exchange(&request, &payload)?;
        ensure!(
            envelope.len() == 256 && reply["sha256"] == digest(&envelope),
            "invalid OEMDRV envelope receipt"
        );
        Ok(OemdrvReadback {
            boot: reply["boot"]
                .as_str()
                .context("missing OEMDRV boot ID")?
                .into(),
            fingerprint: reply["fingerprint"]
                .as_str()
                .context("missing OEMDRV fingerprint")?
                .into(),
            envelope,
        })
    }

    fn exchange(
        &self,
        request: &impl serde::Serialize,
        payload: &[u8],
    ) -> Result<(Value, Vec<u8>)> {
        use std::os::windows::process::CommandExt;
        let mut ssh = self.ssh("-T");
        ssh.creation_flags(0x08000000); // CREATE_NO_WINDOW: Ctrl-C belongs to xtask.
        ssh.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // There is deliberately no SSH remote-command argument.
        let (_job, mut child) = host::spawn_owned(&mut ssh)?;
        let stdout = child.stdout.take().context("SSH stdout")?;
        let stderr = child.stderr.take().context("SSH stderr")?;
        let out = thread::spawn(move || {
            let mut b = Vec::new();
            stdout
                .take((32 * 1024 * 1024 + MAX_REPLY_PAYLOAD_BYTES + 1) as u64)
                .read_to_end(&mut b)
                .map(|_| b)
        });
        let err = thread::spawn(move || {
            let mut b = Vec::new();
            stderr.take(1024 * 1024).read_to_end(&mut b).map(|_| b)
        });
        {
            let mut input = child.stdin.take().context("SSH stdin")?;
            input.write_all(b"job\n")?;
            serde_json::to_writer(&mut input, request)?;
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
        ensure!(
            newline <= 32 * 1024 * 1024,
            "builder reply metadata size limit"
        );
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
            data.len() <= MAX_REPLY_PAYLOAD_BYTES,
            "builder reply payload size limit"
        );
        ensure!(
            reply["payload_bytes"].as_u64() == Some(data.len() as u64),
            "truncated builder reply"
        );
        Ok((reply["value"].clone(), data))
    }

    fn ssh(&self, terminal: &str) -> Command {
        let endpoint = &self.endpoint;
        let mut ssh = Command::new("ssh.exe");
        ssh.args([terminal, "-F", "NUL", "-i"])
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
            .args([
                "-o",
                &format!("UserKnownHostsFile={}", endpoint.known_hosts_file.display()),
            ])
            .args(["-o", &format!("HostKeyAlias={}", endpoint.host_key_alias)])
            .arg(format!("{}@{}", endpoint.user, endpoint.address));
        ssh
    }
}

fn installation_busy(error: &anyhow::Error) -> bool {
    error.to_string().contains("installation lock")
        && error.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.raw_os_error() == Some(33))
        })
}
