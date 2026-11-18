//! ACL-protected Windows AF_UNIX transport for the shared bounded QMP client.
use anyhow::{Context, Result, ensure};
use ilex_tooling_qmp::{Client, MAX_MESSAGE_BYTES, Transport};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

pub struct Socket {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    _access: crate::private_qmp::Access,
}
impl Transport for Socket {
    fn read_message(&mut self, deadline: Instant) -> Result<Value> {
        let mut message = Vec::new();
        loop {
            self.reader.get_ref().set_read_timeout(Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .context("QMP deadline expired")?,
            ))?;
            let bytes = self.reader.fill_buf()?;
            ensure!(!bytes.is_empty(), "QMP connection closed");
            let count = bytes
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |n| n + 1);
            ensure!(
                message.len() as u64 + count as u64 <= MAX_MESSAGE_BYTES,
                "QMP message exceeds limit"
            );
            message.extend_from_slice(&bytes[..count]);
            self.reader.consume(count);
            if message.last() == Some(&b'\n') {
                return Ok(serde_json::from_slice(&message)?);
            }
        }
    }
    fn write_message(&mut self, value: &Value, deadline: Instant) -> Result<()> {
        let mut message = serde_json::to_vec(value)?;
        message.push(b'\n');
        ensure!(
            message.len() as u64 <= MAX_MESSAGE_BYTES,
            "QMP message exceeds limit"
        );
        let mut remaining = message.as_slice();
        while !remaining.is_empty() {
            self.writer.set_write_timeout(Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .context("QMP deadline expired")?,
            ))?;
            let sent = self.writer.write(remaining)?;
            ensure!(sent != 0, "QMP connection closed during write");
            remaining = &remaining[sent..];
        }
        Ok(self.writer.flush()?)
    }
}
pub struct Qmp(Client<Socket>, Option<std::fs::File>);
impl Qmp {
    pub fn connect(endpoint: &str) -> Result<Self> {
        let (writer, access) = crate::private_qmp::connect(endpoint)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self(
            Client::connect(
                Socket {
                    reader,
                    writer,
                    _access: access,
                },
                Duration::from_secs(5),
            )?,
            None,
        ))
    }
    pub fn connect_serialized(endpoint: &str, path: &std::path::Path) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => break,
                Err(error)
                    if (error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(33))
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => return Err(error).context("QMP client busy (10s deadline)"),
            }
        }
        let mut client = Self::connect(endpoint)?;
        client.1 = Some(file);
        Ok(client)
    }
    pub fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
        self.0.execute(command, arguments)
    }
}

pub fn security(endpoint: &str) -> Result<Value> {
    Ok(crate::private_qmp::verify(endpoint)?.report.clone())
}

/// One newly created, ACL-protected directory per launch attempt. AF_UNIX
/// paths stay below the Windows sockaddr_un limit, independent of checkout.
pub struct Endpoints {
    pub control: String,
    pub input: String,
    path: std::path::PathBuf,
}
impl Endpoints {
    pub fn clean_stale(owner: &str, run: &str) -> Result<()> {
        let root = crate::host::root()?.join("qmp");
        for attempt in 0..5 {
            let path =
                root.join(&crate::host::hash(format!("{owner}:{run}:{attempt}").as_bytes())[..32]);
            if !path.exists() {
                continue;
            }
            let _directory = crate::received::Directory::open(&path)?;
            for name in ["control.sock", "input.sock"] {
                let socket = path.join(name);
                if socket.exists() {
                    let endpoint = format!("unix:{}", socket.to_string_lossy().replace('\\', "/"));
                    // Verify type and ACL before removing only this dead run's socket.
                    drop(crate::private_qmp::verify(&endpoint)?);
                    std::fs::remove_file(socket)?;
                }
            }
            drop(_directory);
            std::fs::remove_dir(path)?;
        }
        Ok(())
    }
    pub fn new(owner: &str, run: &str, attempt: usize) -> Result<Self> {
        let root = crate::host::root()?.join("qmp");
        crate::host::secure_dir(&root)?;
        let path =
            root.join(&crate::host::hash(format!("{owner}:{run}:{attempt}").as_bytes())[..32]);
        std::fs::create_dir(&path).context("allocating a new private QMP namespace")?;
        let result = Self {
            control: format!(
                "unix:{}",
                path.join("control.sock")
                    .to_string_lossy()
                    .replace('\\', "/")
            ),
            input: format!(
                "unix:{}",
                path.join("input.sock").to_string_lossy().replace('\\', "/")
            ),
            path,
        };
        crate::host::secure_dir(&result.path)?;
        ensure!(
            result.control[5..].len() < 108 && result.input[5..].len() < 108,
            "private QMP path exceeds Windows AF_UNIX limit"
        );
        Ok(result)
    }
}
impl Drop for Endpoints {
    fn drop(&mut self) {
        for name in ["control.sock", "input.sock"] {
            let _ = std::fs::remove_file(self.path.join(name));
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// Real pinned-QEMU transport probe, with no guest or guest RAM. The bounded
/// hold lets the e2e driver attempt connections using a low-integrity token.
pub fn transport_self_check(
    executable: &std::path::Path,
    artifacts: &std::path::Path,
) -> Result<()> {
    std::fs::create_dir_all(artifacts)?;
    let run = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    );
    let endpoints = Endpoints::new("transport-self-check", &run, 0)?;
    let name = format!("ilex-private-qmp-{run}");
    let mut command = std::process::Command::new(executable);
    use std::os::windows::process::CommandExt;
    command
        .creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW)
        .args([
            "-machine",
            "none",
            "-nodefaults",
            "-display",
            "none",
            "-m",
            "0",
            "-name",
            &name,
            "-qmp",
            &format!("{},server=on,wait=off", endpoints.control),
            "-qmp",
            &format!("{},server=on,wait=off", endpoints.input),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::fs::File::create(artifacts.join("qemu-stdout.log"))?)
        .stderr(std::fs::File::create(artifacts.join("qemu-stderr.log"))?);
    let (job, mut child) = crate::host::spawn_owned(&mut command)?;
    let result = (|| -> Result<()> {
        for endpoint in [&endpoints.control, &endpoints.input] {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                match Qmp::connect(endpoint) {
                    Ok(mut client) => {
                        ensure!(
                            client.execute("query-name", serde_json::json!({}))?["name"] == name,
                            "transport QEMU identity mismatch"
                        );
                        break;
                    }
                    Err(error) if Instant::now() >= deadline => return Err(error),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
                ensure!(child.try_wait()?.is_none(), "transport QEMU exited");
            }
            // Reconnection is part of the native CLI contract.
            Qmp::connect(endpoint)?.execute("query-status", serde_json::json!({}))?;
        }
        crate::host::atomic(
            &artifacts.join("transport-ready.json"),
            &serde_json::to_vec_pretty(
                &serde_json::json!({"schema":1,"run":run,"pid":child.id(),"control":security(&endpoints.control)?,"input":security(&endpoints.input)?,"reconnect_passed":true}),
            )?,
        )?;
        let until = Instant::now() + Duration::from_secs(120);
        while Instant::now() < until && !artifacts.join("transport.stop").exists() {
            ensure!(
                child.try_wait()?.is_none(),
                "transport QEMU exited during token probes"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    drop(job);
    result
}
