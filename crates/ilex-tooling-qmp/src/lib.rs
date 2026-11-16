//! QMP framing, negotiation and request correlation over a backend transport.
//! No Unix/Windows imports. One absolute deadline per command, including events.
pub mod keys;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
pub const MAX_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

pub trait Transport {
    fn read_message(&mut self, deadline: Instant) -> Result<Value>;
    fn write_message(&mut self, message: &Value, deadline: Instant) -> Result<()>;
}
pub struct Client<T> {
    transport: T,
    next_id: u64,
    timeout: Duration,
}
impl<T: Transport> Client<T> {
    pub fn connect(mut transport: T, timeout: Duration) -> Result<Self> {
        ensure!(!timeout.is_zero(), "QMP timeout must be nonzero");
        let greeting = transport
            .read_message(Instant::now() + timeout)
            .context("waiting for QMP greeting")?;
        ensure!(
            greeting.get("QMP").is_some(),
            "endpoint did not send a QMP greeting: {greeting}"
        );
        let mut client = Self {
            transport,
            next_id: 1,
            timeout,
        };
        client.execute("qmp_capabilities", json!({}))?;
        Ok(client)
    }
    pub fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
        let deadline = Instant::now() + self.timeout;
        let id = self.next_id;
        self.next_id = id.checked_add(1).context("QMP request IDs exhausted")?;
        self.transport.write_message(
            &json!({"execute": command, "arguments": arguments, "id": id}),
            deadline,
        )?;
        loop {
            ensure!(Instant::now() < deadline, "QMP command {command} timed out");
            let message = self.transport.read_message(deadline)?;
            if message.get("event").is_some() {
                continue;
            }
            ensure!(
                message.get("id").and_then(Value::as_u64) == Some(id),
                "QMP response ID does not match request {id}"
            );
            if let Some(error) = message.get("error") {
                bail!(
                    "QMP {command} failed: {}",
                    error["desc"].as_str().unwrap_or("unknown error")
                );
            }
            return message
                .get("return")
                .cloned()
                .context("QMP response has neither return nor error");
        }
    }
}
pub fn key_event(qcode: &str, down: bool) -> Value {
    json!({"type": "key", "data": {"down": down, "key": {"type": "qcode", "data": qcode}}})
}
pub fn physical_key_event(code: u32, down: bool) -> Value {
    json!({"type": "key", "data": {"down": down, "key": {"type": "number", "data": code}}})
}
pub fn button_event(button: &str, down: bool) -> Value {
    json!({"type": "btn", "data": {"down": down, "button": button}})
}
pub fn absolute_events(x: u32, y: u32, width: u32, height: u32) -> Vec<Value> {
    let scale = |v: u32, size: u32| {
        let max = size.saturating_sub(1).max(1) as u64;
        ((v.min(size.saturating_sub(1)) as u64 * 0x7fff + max / 2) / max) as u32
    };
    vec![
        json!({"type": "abs", "data": {"axis": "x", "value": scale(x, width)}}),
        json!({"type": "abs", "data": {"axis": "y", "value": scale(y, height)}}),
    ]
}
