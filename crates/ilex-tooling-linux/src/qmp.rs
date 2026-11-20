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
        io::{BufRead, BufReader, Write},
        os::unix::net::UnixStream,
        path::Path,
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
            let reader = BufReader::new(writer.try_clone()?);
            Ok(Self(Client::connect(Socket { reader, writer }, timeout)?))
        }
        pub fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
            self.0.execute(command, arguments)
        }
    }
}
