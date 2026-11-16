//! Clipboard sharing with the guest: the client side of the SPICE agent
//! protocol, spoken by the display bridge to `spice-vdagent` in the guest.
//!
//! QEMU puts the guest's `com.redhat.spice.0` virtio-serial port on a plain
//! socket (`vdagent.sock`) instead of its own `qemu-vdagent` chardev: with
//! that one, `spice-vdagentd` reads EOF on the port, decides there is no
//! client and drops every clipboard message (and QEMU's GTK clipboard is
//! broken on a Wayland host anyway, qemu-project/qemu#1455). The guest half
//! works: the X11 agent sees GNOME's clipboard through XWayland.
//!
//! Wire format (spice-protocol `vd_agent.h`), little-endian: the port is a
//! stream of chunks, `VDIChunkHeader { port: u32, size: u32 }` and at most
//! 2048 bytes of data; chunk data on `VDP_CLIENT_PORT` concatenates into
//! messages, `VDAgentMessage { protocol: u32 = 1, type: u32, opaque: u64,
//! size: u32 }` and `size` bytes. The clipboard is "by demand": whoever has
//! something new sends a GRAB with the types it offers, the other side sends
//! a REQUEST when someone pastes, and the owner answers with CLIPBOARD. Only
//! UTF-8 text is offered; the guest's line ends are LF.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

const VDP_CLIENT_PORT: u32 = 1;
const VD_AGENT_PROTOCOL: u32 = 1;
const MAX_CHUNK: usize = 2048;

const VD_AGENT_CLIPBOARD: u32 = 4;
const VD_AGENT_ANNOUNCE_CAPABILITIES: u32 = 6;
const VD_AGENT_CLIPBOARD_GRAB: u32 = 7;
const VD_AGENT_CLIPBOARD_REQUEST: u32 = 8;
const VD_AGENT_CLIPBOARD_RELEASE: u32 = 9;

const VD_AGENT_CAP_CLIPBOARD_BY_DEMAND: u32 = 5;
const VD_AGENT_CLIPBOARD_UTF8_TEXT: u32 = 1;

/// The bridge's end of the guest agent channel.
pub struct Agent {
    writer: Mutex<UnixStream>,
    /// What the host offered the guest last (served on REQUEST).
    host_text: Mutex<String>,
    /// The guest's agent has announced itself (so it knows ours too).
    answered: AtomicBool,
}

impl Agent {
    /// Connect to QEMU's `vdagent.sock` and serve the guest. `on_guest_text`
    /// gets the guest's clipboard text whenever it changes.
    pub fn start(
        socket: &Path,
        on_guest_text: impl Fn(String) + Send + 'static,
    ) -> Result<Arc<Agent>> {
        let started = Instant::now();
        let stream = loop {
            match UnixStream::connect(socket) {
                Ok(s) => break s,
                Err(_) if started.elapsed() < Duration::from_secs(10) => {
                    thread::sleep(Duration::from_millis(100))
                }
                Err(e) => {
                    return Err(e).with_context(|| format!("connecting to {}", socket.display()));
                }
            }
        };
        let agent = Arc::new(Agent {
            writer: Mutex::new(stream.try_clone()?),
            host_text: Mutex::new(String::new()),
            answered: AtomicBool::new(false),
        });
        let reader_agent = Arc::clone(&agent);
        thread::Builder::new()
            .name("vdagent".into())
            .spawn(move || {
                if let Err(e) = reader_agent.serve(stream, on_guest_text) {
                    eprintln!("display bridge: guest agent channel: {e:#}");
                }
            })?;
        // What reaches the port before `spice-vdagentd` opens it in the
        // guest is lost, and without our capabilities it ignores the
        // clipboard: announce until the guest answers.
        let announcer = Arc::clone(&agent);
        thread::Builder::new()
            .name("vdagent-announce".into())
            .spawn(move || {
                while !announcer.answered.load(Ordering::Relaxed) {
                    if announcer.announce(true).is_err() {
                        return;
                    }
                    thread::sleep(Duration::from_secs(2));
                }
            })?;
        Ok(agent)
    }

    /// The host clipboard has new text: offer it to the guest.
    ///
    /// Offered twice, half a second apart: on GNOME (Wayland), mutter fetches
    /// an X11 clipboard owner's text the moment ownership changes, and its
    /// first fetch from `spice-vdagent` comes back empty, so the first paste
    /// would paste nothing. The second offer makes it fetch again.
    pub fn offer(self: &Arc<Self>, text: &str) -> Result<()> {
        let text = text.replace("\r\n", "\n");
        *self.host_text.lock().unwrap() = text.clone();
        self.grab()?;
        let agent = Arc::clone(self);
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            if *agent.host_text.lock().unwrap() == text {
                let _ = agent.grab();
            }
        });
        Ok(())
    }

    fn grab(&self) -> Result<()> {
        self.send(
            VD_AGENT_CLIPBOARD_GRAB,
            &VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes(),
        )
    }

    fn announce(&self, request: bool) -> Result<()> {
        let mut data = (request as u32).to_le_bytes().to_vec();
        data.extend_from_slice(&(1u32 << VD_AGENT_CAP_CLIPBOARD_BY_DEMAND).to_le_bytes());
        self.send(VD_AGENT_ANNOUNCE_CAPABILITIES, &data)
    }

    /// One message, split into chunks.
    fn send(&self, kind: u32, data: &[u8]) -> Result<()> {
        if std::env::var_os("ILEX_VIEWER_STATS").is_some() && kind != VD_AGENT_ANNOUNCE_CAPABILITIES
        {
            eprintln!(
                "display bridge: to the guest agent: message {kind} ({} bytes)",
                data.len()
            );
        }
        let mut msg = Vec::with_capacity(20 + data.len());
        msg.extend_from_slice(&VD_AGENT_PROTOCOL.to_le_bytes());
        msg.extend_from_slice(&kind.to_le_bytes());
        msg.extend_from_slice(&0u64.to_le_bytes());
        msg.extend_from_slice(&(data.len() as u32).to_le_bytes());
        msg.extend_from_slice(data);
        let mut w = self.writer.lock().unwrap();
        for chunk in msg.chunks(MAX_CHUNK) {
            w.write_all(&VDP_CLIENT_PORT.to_le_bytes())?;
            w.write_all(&(chunk.len() as u32).to_le_bytes())?;
            w.write_all(chunk)?;
        }
        Ok(())
    }

    fn serve(&self, mut stream: UnixStream, on_guest_text: impl Fn(String)) -> Result<()> {
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let mut head = [0u8; 8];
            stream
                .read_exact(&mut head)
                .context("reading a chunk header")?;
            let port = u32::from_le_bytes(head[..4].try_into().unwrap());
            let size = u32::from_le_bytes(head[4..].try_into().unwrap()) as usize;
            if size > 64 << 10 {
                bail!("chunk of {size} bytes");
            }
            let mut data = vec![0u8; size];
            stream.read_exact(&mut data)?;
            if port != VDP_CLIENT_PORT {
                continue;
            }
            pending.extend_from_slice(&data);
            // Whole messages out of what has arrived.
            while pending.len() >= 20 {
                let kind = u32::from_le_bytes(pending[4..8].try_into().unwrap());
                let len = u32::from_le_bytes(pending[16..20].try_into().unwrap()) as usize;
                if len > 64 << 20 {
                    bail!("agent message of {len} bytes");
                }
                if pending.len() < 20 + len {
                    break;
                }
                let body: Vec<u8> = pending[20..20 + len].to_vec();
                pending.drain(..20 + len);
                self.handle(kind, &body, &on_guest_text)?;
            }
        }
    }

    fn handle(&self, kind: u32, body: &[u8], on_guest_text: &impl Fn(String)) -> Result<()> {
        let u32_at = |i: usize| {
            body.get(i..i + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
        };
        if std::env::var_os("ILEX_VIEWER_STATS").is_some() {
            eprintln!(
                "display bridge: guest agent sent message {kind} ({} bytes, first word {:?})",
                body.len(),
                u32_at(0)
            );
        }
        match kind {
            // The guest's agent (re)appeared: tell it what we can do and
            // offer what the host clipboard holds.
            VD_AGENT_ANNOUNCE_CAPABILITIES => {
                self.answered.store(true, Ordering::Relaxed);
                if u32_at(0) == Some(1) {
                    self.announce(false)?;
                }
                if !self.host_text.lock().unwrap().is_empty() {
                    self.grab()?;
                }
            }
            // The guest copied something: ask for it as text right away.
            VD_AGENT_CLIPBOARD_GRAB => {
                let offers_text = (0..body.len() / 4)
                    .any(|i| u32_at(i * 4) == Some(VD_AGENT_CLIPBOARD_UTF8_TEXT));
                if offers_text {
                    self.send(
                        VD_AGENT_CLIPBOARD_REQUEST,
                        &VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes(),
                    )?;
                }
            }
            // Someone in the guest pastes what the host offered.
            VD_AGENT_CLIPBOARD_REQUEST => {
                let text = self.host_text.lock().unwrap().clone();
                let mut data = VD_AGENT_CLIPBOARD_UTF8_TEXT.to_le_bytes().to_vec();
                data.extend_from_slice(text.as_bytes());
                self.send(VD_AGENT_CLIPBOARD, &data)?;
            }
            VD_AGENT_CLIPBOARD => {
                if u32_at(0) == Some(VD_AGENT_CLIPBOARD_UTF8_TEXT) {
                    on_guest_text(String::from_utf8_lossy(&body[4..]).into_owned());
                }
            }
            VD_AGENT_CLIPBOARD_RELEASE => {}
            _ => {}
        }
        Ok(())
    }
}
