//! The display bridge behind `cargo xtask vm --gui` in WSL: QEMU draws the
//! guest into a private Xvfb, and this serves it to `ilex-viewer.exe`, a
//! native Windows window ([`crate::viewer`] is the wire protocol).
//!
//! Why not a window through WSLg: Windows keeps the Windows key and Alt+Tab
//! for itself in a WSLg window, QEMU's GTK clipboard is broken on a Wayland
//! host, QMP `screendump` can't capture virgl output, and WSLg itself fails
//! to draw anything on many starts of WSL (see CLAUDE.md). Here QEMU still
//! renders with virgl (Mesa's D3D12 driver works on Xvfb too), and:
//!
//! - frames: XDamage on the Xvfb root says what changed; the bridge reads
//!   those rectangles (`GetImage`), compares them in 64x64 tiles with what
//!   the viewer already has, and sends only tiles that differ;
//! - pointer image: XFixes cursor notifications (QEMU's GTK window shows the
//!   guest's hardware cursor), sent to the viewer to use as its own cursor;
//! - input: viewer events go straight to QEMU on a QMP socket of their own
//!   (`input.sock`), so `ilex-vmctl` keeps `qmp.sock`;
//! - size: the viewer's window size becomes the QEMU window's size on the
//!   Xvfb, which QEMU passes on to the guest (EDID), so pixels map 1:1;
//! - screenshots: `screen.sock` answers every connection with the current
//!   frame (`u32` width, `u32` height, BGRX pixels), for `ilex-vmctl`;
//! - clipboard: text both ways with `spice-vdagent` in the guest, through
//!   QEMU's `vdagent.sock` ([`crate::vdagent`]).

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt as _, EventMask,
    ImageFormat, MapState, Window,
};
use x11rb::rust_connection::RustConnection;

use crate::vdagent::Agent;
use crate::viewer::{Button, ToBridge, ToViewer};

/// Big enough for any window the viewer asks for (an 8K monitor).
const XVFB_SCREEN: &str = "7680x4320x24";

/// Tile edge for change detection.
const TILE: usize = 64;

// ---------------------------------------------------------------------------
// Xvfb
// ---------------------------------------------------------------------------

/// A private X server for QEMU's window. Killed on drop.
pub struct Xvfb {
    child: Child,
    /// `:N`, for `DISPLAY`.
    pub display: String,
    /// Where Xvfb keeps its framebuffer as a file (`-fbdir`, on tmpfs), so
    /// the bridge reads pixels from memory instead of `GetImage`.
    fbdir: PathBuf,
}

impl Xvfb {
    /// WSLg mounts `/tmp/.X11-unix` read-only, so Xvfb's socket file can't be
    /// created there; its abstract socket (`@/tmp/.X11-unix/XN`), which X
    /// clients try first, works. Readiness is therefore a real connection.
    pub fn start() -> Result<Xvfb> {
        crate::cmd::require("Xvfb")?;
        let n = (70..120)
            .find(|n| {
                !Path::new(&format!("/tmp/.X{n}-lock")).exists()
                    && !Path::new(&format!("/tmp/.X11-unix/X{n}")).exists()
            })
            .context("no free X display number between :70 and :119")?;
        let display = format!(":{n}");
        let fbdir = PathBuf::from(format!("/dev/shm/ilex-xvfb-{n}"));
        let _ = fs::remove_dir_all(&fbdir);
        fs::create_dir_all(&fbdir).with_context(|| format!("creating {}", fbdir.display()))?;
        let child = Command::new("Xvfb")
            .args([
                &display,
                "-screen",
                "0",
                XVFB_SCREEN,
                "-nolisten",
                "tcp",
                "-noreset",
                "-fbdir",
            ])
            .arg(&fbdir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning Xvfb")?;
        let mut xvfb = Xvfb {
            child,
            display,
            fbdir,
        };
        let started = Instant::now();
        while x11rb::connect(Some(&xvfb.display)).is_err() {
            if let Some(status) = xvfb.child.try_wait()? {
                bail!("Xvfb {} exited with {status}", xvfb.display);
            }
            if started.elapsed() > Duration::from_secs(10) {
                bail!("Xvfb {} did not come up within 10s", xvfb.display);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(xvfb)
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.fbdir);
    }
}

/// Xvfb's screen as a memory-mapped XWD file (`-fbdir`): the pixels Xvfb
/// draws into, readable without a round trip through the X server.
struct ScreenFile {
    map: memmap2::Mmap,
    offset: usize,
    stride: usize,
    width: usize,
    height: usize,
}

impl ScreenFile {
    fn open(fbdir: &Path) -> Result<ScreenFile> {
        let path = fbdir.join("Xvfb_screen0");
        let file = File::open(&path).with_context(|| format!("opening {}", path.display()))?;
        // Safety: Xvfb keeps writing to the file while it is mapped; reads
        // may see a frame half drawn, which the next damage event repairs.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        // XWDFileHeader: CARD32s, most significant byte first.
        let field = |i: usize| -> Result<usize> {
            let b = map.get(i * 4..i * 4 + 4).context("XWD header too short")?;
            Ok(u32::from_be_bytes(b.try_into().unwrap()) as usize)
        };
        let (header_size, width, height, bpp, stride, ncolors) = (
            field(0)?,
            field(4)?,
            field(5)?,
            field(11)?,
            field(12)?,
            field(19)?,
        );
        if bpp != 32 {
            bail!("{}: {bpp} bits per pixel, expected 32", path.display());
        }
        let offset = header_size + ncolors * 12;
        if map.len() < offset + stride * height {
            bail!("{}: shorter than its header says", path.display());
        }
        Ok(ScreenFile {
            map,
            offset,
            stride,
            width,
            height,
        })
    }

    /// A rectangle as BGRX rows, like `GetImage`.
    fn read(&self, x: usize, y: usize, width: usize, height: usize) -> Result<Vec<u8>> {
        if x + width > self.width || y + height > self.height {
            bail!(
                "{width}x{height}+{x}+{y} is outside the {}x{} screen",
                self.width,
                self.height
            );
        }
        let mut out = Vec::with_capacity(width * height * 4);
        for row in y..y + height {
            let start = self.offset + row * self.stride + x * 4;
            out.extend_from_slice(&self.map[start..start + width * 4]);
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// The bridge
// ---------------------------------------------------------------------------

/// Where the viewer connects and how it proves it was sent by `xtask vm`.
#[derive(Debug, Clone)]
pub struct Endpoint {
    /// WSL's own address on its virtual network: Windows reaches it directly
    /// (about 4x the throughput of the `localhost` relay).
    pub addr: SocketAddr,
    pub token: String,
}

/// State the bridge's threads share.
struct Shared {
    /// QEMU's top-level window on the Xvfb, once found.
    window: AtomicU32,
    /// Current window (= guest framebuffer) size, `width << 32 | height`.
    size: AtomicU64,
    /// The connected viewer, if any.
    viewer: Mutex<Option<TcpStream>>,
    /// A new viewer needs the whole screen.
    full: AtomicBool,
    stop: AtomicBool,
    display: String,
    fbdir: PathBuf,
}

impl Shared {
    fn size(&self) -> (u32, u32) {
        let v = self.size.load(Ordering::Relaxed);
        ((v >> 32) as u32, v as u32)
    }

    /// Send to the viewer; a viewer that fails to take it is dropped.
    fn send(&self, msgs: &[ToViewer]) {
        let mut guard = self.viewer.lock().unwrap();
        if let Some(stream) = guard.as_mut() {
            let ok = msgs.iter().all(|m| m.write(stream).is_ok());
            if !ok {
                *guard = None;
            }
        }
    }

    fn has_viewer(&self) -> bool {
        self.viewer.lock().unwrap().is_some()
    }
}

/// Running bridge threads. Dropping it stops the capture loop; the rest ends
/// with the process.
pub struct Bridge {
    shared: Arc<Shared>,
    pub endpoint: Endpoint,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Bridge {
    /// Serve the QEMU window on `display` to viewers. `vm_dir` is the VM's
    /// runtime directory: QEMU's `input.sock` is there, `screen.sock` goes
    /// there.
    pub fn start(xvfb: &Xvfb, vm_dir: &Path) -> Result<Bridge> {
        let display = xvfb.display.as_str();
        let shared = Arc::new(Shared {
            window: AtomicU32::new(0),
            size: AtomicU64::new(0),
            viewer: Mutex::new(None),
            full: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            display: display.to_string(),
            fbdir: xvfb.fbdir.clone(),
        });

        let window = find_qemu_window(display, Duration::from_secs(30))?;
        shared.window.store(window, Ordering::Relaxed);

        let capture = Arc::clone(&shared);
        thread::Builder::new()
            .name("capture".into())
            .spawn(move || {
                if let Err(e) = capture_loop(capture) {
                    eprintln!("display bridge: capture stopped: {e:#}");
                }
            })?;

        let screen_sock = vm_dir.join("screen.sock");
        let _ = fs::remove_file(&screen_sock);
        let screens = UnixListener::bind(&screen_sock)
            .with_context(|| format!("binding {}", screen_sock.display()))?;
        let screen_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("screen.sock".into())
            .spawn(move || {
                for stream in screens.incoming().flatten() {
                    if let Err(e) = serve_screenshot(&screen_shared, stream) {
                        eprintln!("display bridge: screenshot failed: {e:#}");
                    }
                }
            })?;

        // The guest's clipboard agent, when QEMU has its port on a socket.
        let agent_sock = vm_dir.join("vdagent.sock");
        let agent = if agent_sock.exists() {
            let to_viewer = Arc::clone(&shared);
            match Agent::start(&agent_sock, move |text| {
                to_viewer.send(&[ToViewer::Clipboard { text }])
            }) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("display bridge: no clipboard sharing: {e:#}");
                    None
                }
            }
        } else {
            None
        };

        let listener =
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).context("binding the viewer port")?;
        let port = listener.local_addr()?.port();
        let endpoint = Endpoint {
            addr: SocketAddr::new(own_address(), port),
            token: random_token()?,
        };
        let token = endpoint.token.clone();
        let input_sock = vm_dir.join("input.sock");
        let net_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("viewer".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let shared = Arc::clone(&net_shared);
                    let token = token.clone();
                    let input_sock = input_sock.clone();
                    let agent = agent.clone();
                    thread::spawn(move || {
                        if let Err(e) =
                            serve_viewer(&shared, stream, &token, &input_sock, agent.as_ref())
                        {
                            eprintln!("display bridge: viewer: {e:#}");
                        }
                    });
                }
            })?;

        Ok(Bridge { shared, endpoint })
    }
}

/// The address Windows reaches this WSL instance on (no packet is sent).
fn own_address() -> IpAddr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|s| {
            s.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
            s.local_addr()
        })
        .map(|a| a.ip())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// QEMU's GTK window on `display`: the biggest mapped top-level window whose
/// name starts with `QEMU`.
fn find_qemu_window(display: &str, timeout: Duration) -> Result<Window> {
    let (conn, screen) = x11rb::connect(Some(display))
        .with_context(|| format!("connecting to X display {display}"))?;
    let root = conn.setup().roots[screen].root;
    let started = Instant::now();
    loop {
        let mut best: Option<(Window, u32)> = None;
        for w in conn.query_tree(root)?.reply()?.children {
            let attrs = conn.get_window_attributes(w)?.reply()?;
            if attrs.map_state != MapState::VIEWABLE {
                continue;
            }
            let name = conn
                .get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 256)?
                .reply()?
                .value;
            if !name.starts_with(b"QEMU") {
                continue;
            }
            let geo = conn.get_geometry(w)?.reply()?;
            let area = geo.width as u32 * geo.height as u32;
            if best.is_none_or(|(_, a)| area > a) {
                best = Some((w, area));
            }
        }
        if let Some((w, _)) = best {
            // No window manager: put it where the capture expects it.
            conn.configure_window(w, &ConfigureWindowAux::new().x(0).y(0))?;
            conn.flush()?;
            return Ok(w);
        }
        if started.elapsed() > timeout {
            bail!("QEMU's window did not appear on {display} within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

// ----- frames ---------------------------------------------------------------

/// The pixels the viewer has, for tile comparison.
struct Mirror {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

impl Mirror {
    fn new(width: usize, height: usize) -> Self {
        Mirror {
            width,
            height,
            pixels: vec![0; width * height * 4],
        }
    }
}

fn capture_loop(shared: Arc<Shared>) -> Result<()> {
    // Sending runs beside the capture: while one frame goes out, the next is
    // read and compared. Frames stay in order and none is dropped (the
    // mirror assumes the viewer has every one), so the queue blocks.
    let (frames, outbox) = mpsc::sync_channel::<Vec<ToViewer>>(2);
    let sender = Arc::clone(&shared);
    thread::Builder::new()
        .name("frames".into())
        .spawn(move || {
            for msgs in outbox {
                sender.send(&msgs);
            }
        })?;
    let (conn, screen) = x11rb::connect(Some(&shared.display))?;
    let root = conn.setup().roots[screen].root;
    let window = shared.window.load(Ordering::Relaxed);

    conn.damage_query_version(1, 1)?.reply()?;
    conn.xfixes_query_version(5, 0)?.reply()?;
    let damage = conn.generate_id()?;
    conn.damage_create(damage, root, damage::ReportLevel::NON_EMPTY)?;
    let region = conn.generate_id()?;
    conn.xfixes_create_region(region, &[])?;
    conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;
    conn.change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
    )?;
    conn.flush()?;

    let geo = conn.get_geometry(window)?.reply()?;
    let mut mirror = Mirror::new(geo.width as usize, geo.height as usize);
    shared.size.store(
        (geo.width as u64) << 32 | geo.height as u64,
        Ordering::Relaxed,
    );
    let mut cursor: Option<ToViewer> = None;
    let mut stats = Stats::new();
    let screen = ScreenFile::open(&shared.fbdir)
        .inspect_err(|e| eprintln!("display bridge: {e:#}; reading pixels with GetImage"))
        .ok();
    let read = |x: usize, y: usize, w: usize, h: usize| match &screen {
        Some(s) => s.read(x, y, w, h),
        None => grab(&conn, root, x, y, w, h),
    };

    while !shared.stop.load(Ordering::Relaxed) {
        let mut damaged = false;
        let mut resized = None;
        let mut cursor_changed = false;
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::DamageNotify(_) => damaged = true,
                Event::ConfigureNotify(e) if e.window == window => {
                    resized = Some((e.width, e.height))
                }
                Event::XfixesCursorNotify(_) => cursor_changed = true,
                _ => {}
            }
        }

        if let Some((w, h)) = resized
            && (w as usize, h as usize) != (mirror.width, mirror.height)
        {
            mirror = Mirror::new(w as usize, h as usize);
            shared
                .size
                .store((w as u64) << 32 | h as u64, Ordering::Relaxed);
            shared.full.store(true, Ordering::Relaxed);
        }

        if cursor_changed {
            let img = conn.xfixes_get_cursor_image()?.reply()?;
            cursor = Some(cursor_message(&img));
            if let Some(c) = &cursor {
                shared.send(std::slice::from_ref(c));
            }
        }

        if !shared.has_viewer() {
            // Keep the damage flowing so nothing piles up in the server.
            if damaged {
                conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE)?;
                conn.flush()?;
            }
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        if shared.full.swap(false, Ordering::Relaxed) {
            conn.damage_subtract(damage, x11rb::NONE, x11rb::NONE)?;
            let pixels = read(0, 0, mirror.width, mirror.height)?;
            mirror.pixels.copy_from_slice(&pixels);
            let mut msgs = vec![ToViewer::Size {
                width: mirror.width as u32,
                height: mirror.height as u32,
            }];
            if let Some(c) = &cursor {
                msgs.push(c.clone());
            }
            msgs.push(ToViewer::Rect {
                x: 0,
                y: 0,
                width: mirror.width as u32,
                height: mirror.height as u32,
                pixels,
            });
            msgs.push(ToViewer::Present);
            frames.send(msgs)?;
            continue;
        }

        if damaged {
            let t0 = Instant::now();
            conn.damage_subtract(damage, x11rb::NONE, region)?;
            let rects = conn.xfixes_fetch_region(region)?.reply()?.rectangles;
            let mut msgs = Vec::new();
            let (mut grab_t, mut diff_t) = (Duration::ZERO, Duration::ZERO);
            for r in rects {
                let x0 = (r.x.max(0) as usize).min(mirror.width);
                let y0 = (r.y.max(0) as usize).min(mirror.height);
                let x1 = ((r.x as i64 + r.width as i64).max(0) as usize).min(mirror.width);
                let y1 = ((r.y as i64 + r.height as i64).max(0) as usize).min(mirror.height);
                if x1 > x0 && y1 > y0 {
                    let t = Instant::now();
                    let pixels = read(x0, y0, x1 - x0, y1 - y0)?;
                    grab_t += t.elapsed();
                    let t = Instant::now();
                    diff_tiles(&mut mirror, x0, y0, x1 - x0, y1 - y0, &pixels, &mut msgs);
                    diff_t += t.elapsed();
                }
            }
            let t = Instant::now();
            let bytes: usize = msgs
                .iter()
                .map(|m| {
                    if let ToViewer::Rect { pixels, .. } = m {
                        pixels.len()
                    } else {
                        0
                    }
                })
                .sum();
            if !msgs.is_empty() {
                msgs.push(ToViewer::Present);
                frames.send(msgs)?;
            }
            stats.frame(t0.elapsed(), grab_t, diff_t, t.elapsed(), bytes);
            continue;
        }

        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

/// With `ILEX_VIEWER_STATS` set: frames the bridge handled per second and
/// where their time went, every two seconds.
struct Stats {
    on: bool,
    since: Instant,
    frames: u32,
    total: Duration,
    grab: Duration,
    diff: Duration,
    send: Duration,
    bytes: usize,
}

impl Stats {
    fn new() -> Stats {
        Stats {
            on: std::env::var_os("ILEX_VIEWER_STATS").is_some(),
            since: Instant::now(),
            frames: 0,
            total: Duration::ZERO,
            grab: Duration::ZERO,
            diff: Duration::ZERO,
            send: Duration::ZERO,
            bytes: 0,
        }
    }

    fn frame(
        &mut self,
        total: Duration,
        grab: Duration,
        diff: Duration,
        send: Duration,
        bytes: usize,
    ) {
        if !self.on {
            return;
        }
        self.frames += 1;
        self.total += total;
        self.grab += grab;
        self.diff += diff;
        self.send += send;
        self.bytes += bytes;
        let secs = self.since.elapsed().as_secs_f64();
        if secs >= 2.0 {
            let ms = |d: Duration| d.as_secs_f64() * 1000.0 / self.frames as f64;
            eprintln!(
                "display bridge: {:.1} frames/s, per frame {:.1} ms (grab {:.1}, diff {:.1}, send {:.1}), {:.0} MiB/s",
                self.frames as f64 / secs,
                ms(self.total),
                ms(self.grab),
                ms(self.diff),
                ms(self.send),
                self.bytes as f64 / secs / (1 << 20) as f64
            );
            *self = Stats::new();
        }
    }
}

/// `GetImage` of a screen rectangle: BGRX, `width * 4` bytes per row.
fn grab(
    conn: &RustConnection,
    root: Window,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
) -> Result<Vec<u8>> {
    let reply = conn
        .get_image(
            ImageFormat::Z_PIXMAP,
            root,
            x as i16,
            y as i16,
            width as u16,
            height as u16,
            !0,
        )?
        .reply()?;
    if reply.data.len() != width * height * 4 {
        bail!(
            "GetImage {width}x{height} returned {} bytes (expected a 32-bit visual)",
            reply.data.len()
        );
    }
    Ok(reply.data)
}

/// Compare a freshly grabbed rectangle with the mirror tile by tile, update
/// the mirror, and queue runs of changed tiles (per tile row) as rects.
fn diff_tiles(
    m: &mut Mirror,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    px: &[u8],
    out: &mut Vec<ToViewer>,
) {
    let stride = w * 4;
    let mut ty = y0 - y0 % TILE;
    while ty < y0 + h {
        let (ry0, ry1) = (ty.max(y0), (ty + TILE).min(y0 + h));
        let mut run: Option<usize> = None;
        let mut tx = x0 - x0 % TILE;
        loop {
            let done = tx >= x0 + w;
            let changed = !done && {
                let (rx0, rx1) = (tx.max(x0), (tx + TILE).min(x0 + w));
                (ry0..ry1).any(|y| {
                    let src =
                        &px[(y - y0) * stride + (rx0 - x0) * 4..(y - y0) * stride + (rx1 - x0) * 4];
                    let dst = &m.pixels[(y * m.width + rx0) * 4..(y * m.width + rx1) * 4];
                    src != dst
                })
            };
            match (changed, run) {
                (true, None) => run = Some(tx),
                (false, Some(start)) => {
                    let (rx0, rx1) = (start.max(x0), tx.min(x0 + w));
                    let mut pixels = Vec::with_capacity((rx1 - rx0) * (ry1 - ry0) * 4);
                    for y in ry0..ry1 {
                        let src = &px[(y - y0) * stride + (rx0 - x0) * 4
                            ..(y - y0) * stride + (rx1 - x0) * 4];
                        m.pixels[(y * m.width + rx0) * 4..(y * m.width + rx1) * 4]
                            .copy_from_slice(src);
                        pixels.extend_from_slice(src);
                    }
                    out.push(ToViewer::Rect {
                        x: rx0 as u32,
                        y: ry0 as u32,
                        width: (rx1 - rx0) as u32,
                        height: (ry1 - ry0) as u32,
                        pixels,
                    });
                    run = None;
                }
                _ => {}
            }
            if done {
                break;
            }
            tx += TILE;
        }
        ty += TILE;
    }
}

/// XFixes cursor (premultiplied ARGB `u32`s) as a viewer cursor (straight
/// BGRA bytes).
fn cursor_message(img: &xfixes::GetCursorImageReply) -> ToViewer {
    let mut pixels = Vec::with_capacity(img.cursor_image.len() * 4);
    for &argb in &img.cursor_image {
        let a = (argb >> 24) & 0xff;
        let un = |c: u32| {
            (c * 255 + a / 2)
                .checked_div(a)
                .map_or(0, |v| v.min(255) as u8)
        };
        pixels.extend_from_slice(&[
            un(argb & 0xff),
            un((argb >> 8) & 0xff),
            un((argb >> 16) & 0xff),
            a as u8,
        ]);
    }
    ToViewer::Cursor {
        width: img.width as u32,
        height: img.height as u32,
        hot_x: img.xhot as u32,
        hot_y: img.yhot as u32,
        pixels,
    }
}

fn serve_screenshot(shared: &Shared, mut stream: UnixStream) -> Result<()> {
    let (conn, screen) = x11rb::connect(Some(&shared.display))?;
    let root = conn.setup().roots[screen].root;
    let (w, h) = shared.size();
    if w == 0 || h == 0 {
        bail!("no frame yet");
    }
    let pixels = match ScreenFile::open(&shared.fbdir) {
        Ok(screen) => screen.read(0, 0, w as usize, h as usize)?,
        Err(_) => grab(&conn, root, 0, 0, w as usize, h as usize)?,
    };
    stream.write_all(&w.to_le_bytes())?;
    stream.write_all(&h.to_le_bytes())?;
    stream.write_all(&pixels)?;
    Ok(())
}

// ----- input -----------------------------------------------------------------

fn serve_viewer(
    shared: &Shared,
    stream: TcpStream,
    token: &str,
    input_sock: &Path,
    agent: Option<&Arc<Agent>>,
) -> Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::with_capacity(1 << 16, stream.try_clone()?);
    match ToBridge::read(&mut reader)? {
        ToBridge::Hello { token: t } if t == token => {}
        _ => bail!("{} sent no valid token", stream.peer_addr()?),
    }
    let mut qmp = Qmp::connect(input_sock)?;
    let (ctl, screen) = x11rb::connect(Some(&shared.display))?;
    let _ = screen;
    *shared.viewer.lock().unwrap() = Some(stream.try_clone()?);
    shared.full.store(true, Ordering::Relaxed);
    eprintln!(
        "display bridge: viewer connected from {}",
        stream.peer_addr()?
    );

    let mut held: HashMap<u16, ()> = HashMap::new();
    let result = (|| -> Result<()> {
        loop {
            match ToBridge::read(&mut reader)? {
                ToBridge::Hello { .. } => {}
                ToBridge::Key { qnum, down } => {
                    if down {
                        held.insert(qnum, ());
                    } else {
                        held.remove(&qnum);
                    }
                    qmp.input(vec![key_event(qnum, down)])?;
                }
                ToBridge::Pointer { x, y } => {
                    let (w, h) = shared.size();
                    qmp.input(abs_events(x, y, w, h))?;
                }
                ToBridge::Button { button, down } => qmp.input(vec![button_event(button, down)])?,
                ToBridge::WindowSize { width, height } => {
                    let window = shared.window.load(Ordering::Relaxed);
                    let (w, h) = (width.clamp(320, 7680), height.clamp(200, 4320));
                    ctl.configure_window(
                        window,
                        &ConfigureWindowAux::new().x(0).y(0).width(w).height(h),
                    )?;
                    ctl.flush()?;
                }
                ToBridge::Clipboard { text } => {
                    if let Some(agent) = agent
                        && let Err(e) = agent.offer(&text)
                    {
                        eprintln!("display bridge: clipboard to the guest: {e:#}");
                    }
                }
                ToBridge::Quit => {
                    let _ = qmp.execute("quit", json!({}));
                    return Ok(());
                }
            }
        }
    })();
    // Whatever was held down when the viewer went away comes back up.
    let ups: Vec<Value> = held.keys().map(|&q| key_event(q, false)).collect();
    if !ups.is_empty() {
        let _ = qmp.input(ups);
    }
    *shared.viewer.lock().unwrap() = None;
    eprintln!("display bridge: viewer disconnected");
    result
}

fn key_event(qnum: u16, down: bool) -> Value {
    json!({ "type": "key", "data": { "down": down, "key": { "type": "number", "data": qnum } } })
}

fn button_event(button: Button, down: bool) -> Value {
    json!({ "type": "btn", "data": { "down": down, "button": button.qemu_name() } })
}

/// QEMU's absolute axes run 0..=0x7fff across the screen.
fn abs_events(x: u32, y: u32, width: u32, height: u32) -> Vec<Value> {
    let scale = |v: u32, size: u32| -> u64 {
        if size <= 1 {
            0
        } else {
            (v.min(size - 1) as u64 * 0x7fff) / (size as u64 - 1)
        }
    };
    vec![
        json!({ "type": "abs", "data": { "axis": "x", "value": scale(x, width) } }),
        json!({ "type": "abs", "data": { "axis": "y", "value": scale(y, height) } }),
    ]
}

/// A QMP connection held for the viewer's lifetime (its own socket, so it
/// never blocks `ilex-vmctl`).
struct Qmp {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Qmp {
    fn connect(path: &Path) -> Result<Qmp> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("connecting to {}", path.display()))?;
        let mut qmp = Qmp {
            reader: BufReader::new(stream.try_clone()?),
            writer: stream,
        };
        qmp.read_message()?;
        qmp.execute("qmp_capabilities", json!({}))?;
        Ok(qmp)
    }

    fn read_message(&mut self) -> Result<Value> {
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            bail!("QMP closed");
        }
        Ok(serde_json::from_str(&line)?)
    }

    fn execute(&mut self, command: &str, arguments: Value) -> Result<Value> {
        let msg = json!({ "execute": command, "arguments": arguments });
        self.writer.write_all(format!("{msg}\n").as_bytes())?;
        loop {
            let reply = self.read_message()?;
            if let Some(ret) = reply.get("return") {
                return Ok(ret.clone());
            }
            if let Some(err) = reply.get("error") {
                bail!("QMP {command}: {err}");
            }
            // An asynchronous event: skip.
        }
    }

    fn input(&mut self, events: Vec<Value>) -> Result<()> {
        self.execute("input-send-event", json!({ "events": events }))
            .map(|_| ())
    }
}
