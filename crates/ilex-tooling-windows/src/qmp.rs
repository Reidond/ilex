//! Loopback-only transport for the shared bounded, ID-correlated QMP client.
use anyhow::{Context, Result, ensure};
use ilex_tooling_qmp::{Client, MAX_MESSAGE_BYTES, Transport};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

pub struct Socket {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
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
        self.writer.set_write_timeout(Some(
            deadline
                .checked_duration_since(Instant::now())
                .context("QMP deadline expired")?,
        ))?;
        writeln!(self.writer, "{value}")?;
        Ok(self.writer.flush()?)
    }
}
pub struct Qmp(Client<Socket>, Option<std::fs::File>);
impl Qmp {
    pub fn connect(endpoint: &str) -> Result<Self> {
        let address: SocketAddr = endpoint.parse().context("invalid QMP endpoint")?;
        ensure!(
            address.ip() == std::net::Ipv4Addr::LOCALHOST,
            "QMP endpoint must be IPv4 loopback"
        );
        let writer = TcpStream::connect_timeout(&address, Duration::from_secs(3))?;
        writer.set_nodelay(true)?;
        let reader = BufReader::new(writer.try_clone()?);
        Ok(Self(
            Client::connect(Socket { reader, writer }, Duration::from_secs(5))?,
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
