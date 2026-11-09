//! A small QMP client: connect, negotiate capabilities, run commands.
//!
//! QEMU serves one QMP client at a time on a `server=on` socket, so callers
//! should keep connections short (connect, run what they need, drop) to
//! leave the socket free for the next tool, e.g. an agent while a test runs.

#[cfg(unix)]
pub use unix::Qmp;

#[cfg(not(unix))]
pub use fallback::Qmp;

#[cfg(not(unix))]
mod fallback {
    use std::path::Path;

    use anyhow::{Result, bail};
    use serde_json::Value;

    pub struct Qmp;

    impl Qmp {
        pub fn connect(_socket: &Path) -> Result<Qmp> {
            bail!("QMP sockets are unix sockets: run ilex-vmctl inside the WSL2 Linux box (see CLAUDE.md)")
        }

        pub fn execute(&mut self, _command: &str, _arguments: Value) -> Result<Value> {
            bail!("not supported on this platform")
        }
    }
}

#[cfg(unix)]
mod unix {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{Context, Result, bail};
    use serde_json::{Value, json};

    /// How long to wait for QEMU to answer. Generous because the socket's
    /// greeting only comes once any previous client has disconnected.
    const TIMEOUT: Duration = Duration::from_secs(60);

    pub struct Qmp {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Qmp {
        /// Connect and leave capabilities negotiation mode.
        pub fn connect(socket: &Path) -> Result<Qmp> {
            let stream = UnixStream::connect(socket)
                .with_context(|| format!("connecting to {} (is the VM running?)", socket.display()))?;
            stream.set_read_timeout(Some(TIMEOUT))?;
            stream.set_write_timeout(Some(TIMEOUT))?;
            let mut qmp = Qmp { reader: BufReader::new(stream.try_clone()?), writer: stream };
            let greeting = qmp.read_message().context("waiting for the QMP greeting")?;
            if greeting.get("QMP").is_none() {
                bail!("{} did not send a QMP greeting: {greeting}", socket.display());
            }
            qmp.execute("qmp_capabilities", json!({}))?;
            Ok(qmp)
        }

        fn read_message(&mut self) -> Result<Value> {
            let mut line = String::new();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("QMP socket closed");
            }
            Ok(serde_json::from_str(&line)?)
        }

        /// Run one command and return its `return` value. Events that arrive
        /// in between are skipped.
        pub fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
            let request = json!({ "execute": command, "arguments": arguments });
            writeln!(self.writer, "{request}")?;
            self.writer.flush()?;
            loop {
                let msg = self.read_message()?;
                if let Some(ret) = msg.get("return") {
                    return Ok(ret.clone());
                }
                if let Some(err) = msg.get("error") {
                    bail!("QMP `{command}` failed: {}", err["desc"].as_str().unwrap_or("unknown error"));
                }
                // {"event": ...}: ignore.
            }
        }
    }
}
