//! Linux Unix-socket transport for the portable protocol.
pub use ilex_tooling_qmp as protocol;
#[cfg(target_os = "linux")]
pub use linux::Qmp;

#[cfg(not(target_os = "linux"))]
pub struct Qmp;
#[cfg(not(target_os = "linux"))]
impl Qmp {
    pub fn connect_with_timeout(
        _: &std::path::Path,
        _: std::time::Duration,
    ) -> anyhow::Result<Self> {
        anyhow::bail!("Linux QMP transport requires Linux")
    }
    pub fn connect(_: &std::path::Path) -> anyhow::Result<Self> {
        anyhow::bail!("Linux QMP transport requires Linux")
    }
    pub fn connect_ready(
        _: &std::path::Path,
        _: std::time::Duration,
        _: impl FnMut() -> anyhow::Result<bool>,
    ) -> anyhow::Result<Self> {
        anyhow::bail!("Linux QMP transport requires Linux")
    }
    pub fn execute(&mut self, _: &str, _: serde_json::Value) -> anyhow::Result<serde_json::Value> {
        anyhow::bail!("Linux QMP transport is unavailable on this host")
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use anyhow::{Context, Result, ensure};
    use ilex_tooling_qmp::{Client, MAX_MESSAGE_BYTES, Transport};
    use serde_json::Value;
    use std::{
        io::{BufRead, BufReader, ErrorKind, Write},
        os::unix::net::UnixStream,
        path::Path,
        thread,
        time::{Duration, Instant},
    };

    pub struct Socket {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }
    impl Transport for Socket {
        fn read_message(&mut self, deadline: Instant) -> Result<Value> {
            let mut message = Vec::new();
            // Reapply the remaining deadline even for partial/slow messages.
            loop {
                self.reader.get_ref().set_read_timeout(Some(
                    deadline
                        .checked_duration_since(Instant::now())
                        .context("QMP read deadline expired")?,
                ))?;
                let chunk = self.reader.fill_buf()?;
                ensure!(!chunk.is_empty(), "QMP socket closed");
                let count = chunk
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(chunk.len(), |n| n + 1);
                ensure!(
                    message.len() as u64 + count as u64 <= MAX_MESSAGE_BYTES,
                    "QMP message exceeds size limit"
                );
                message.extend_from_slice(&chunk[..count]);
                self.reader.consume(count);
                if message.last() == Some(&b'\n') {
                    return Ok(serde_json::from_slice(&message)?);
                }
            }
        }
        fn write_message(&mut self, message: &Value, deadline: Instant) -> Result<()> {
            self.writer.set_write_timeout(Some(
                deadline
                    .checked_duration_since(Instant::now())
                    .context("QMP write deadline expired")?,
            ))?;
            writeln!(self.writer, "{message}")?;
            self.writer.flush()?;
            Ok(())
        }
    }
    pub struct Qmp(Client<Socket>);
    impl Qmp {
        pub fn connect(path: &Path) -> Result<Self> {
            Self::connect_with_timeout(path, Duration::from_secs(60))
        }
        pub fn connect_with_timeout(path: &Path, timeout: Duration) -> Result<Self> {
            let writer = UnixStream::connect(path).with_context(|| {
                format!("connecting to {} (is the VM running?)", path.display())
            })?;
            Self::handshake(writer, timeout)
        }
        /// Connect to a QMP server that is still starting: one deadline covers the
        /// socket's appearance, the connection and the handshake. QEMU binds
        /// (creating the path) before it listens, so a missing path
        /// (`NotFound`) and a refused connection (`ConnectionRefused`) are
        /// retried; any other error ends the wait. `peer_gone` runs before every
        /// attempt and returns `Ok(true)` once the server's process has exited
        /// (its socket path then stays behind and refuses forever). The greeting
        /// and `qmp_capabilities` each get the time that remains.
        pub fn connect_ready(
            path: &Path,
            timeout: Duration,
            mut peer_gone: impl FnMut() -> Result<bool>,
        ) -> Result<Self> {
            let deadline = Instant::now() + timeout;
            let mut last = String::from("no connection was attempted");
            loop {
                ensure!(!peer_gone()?, "the QMP server exited");
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "{} did not accept a connection within {timeout:?}: {last}",
                    path.display()
                );
                match UnixStream::connect(path) {
                    Ok(stream) => return Self::handshake(stream, remaining),
                    Err(e)
                        if matches!(
                            e.kind(),
                            ErrorKind::NotFound | ErrorKind::ConnectionRefused
                        ) =>
                    {
                        last = e.to_string();
                        thread::sleep(Duration::from_millis(50).min(remaining));
                    }
                    Err(e) => {
                        return Err(e).with_context(|| format!("connecting to {}", path.display()));
                    }
                }
            }
        }
        fn handshake(writer: UnixStream, timeout: Duration) -> Result<Self> {
            let reader = BufReader::new(writer.try_clone()?);
            Ok(Self(Client::connect(Socket { reader, writer }, timeout)?))
        }
        pub fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
            self.0.execute(command, arguments)
        }
    }
}
