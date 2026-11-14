//! The window: winit for the window and its events, softbuffer to show the
//! guest's pixels, a network thread per direction.

use std::io::{BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ilex_devkit::viewer::{Button, ToBridge, ToViewer};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::scancode::PhysicalKeyExtScancode;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{CustomCursor, Window, WindowId};

use crate::{Args, clipboard, keyboard};

/// What the network thread tells the window.
enum UserEvent {
    /// New pixels are in the framebuffer.
    Present,
    Cursor {
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        rgba: Vec<u8>,
    },
    /// The bridge went away (the VM stopped).
    Closed(String),
}

/// The guest's screen as the bridge sent it.
struct Framebuffer {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

/// Wait this long after the last resize before asking the guest to follow.
const RESIZE_SETTLE: Duration = Duration::from_millis(250);

/// Pixels of touchpad scrolling per wheel click.
const PIXELS_PER_CLICK: f64 = 60.0;

struct App {
    args: Args,
    to_bridge: Sender<ToBridge>,
    framebuffer: Arc<Mutex<Framebuffer>>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    /// A window size the guest hasn't been asked to take yet, and since when.
    pending_size: Option<(PhysicalSize<u32>, Instant)>,
    /// Touchpad scrolling not yet worth a wheel click.
    scroll: (f64, f64),
    error: Option<String>,
}

pub fn run(args: Args) -> Result<()> {
    let addr = args.connect.clone().context("--connect is required")?;
    let token = args.token.clone().context("--token is required")?;
    let stream = TcpStream::connect(&addr).with_context(|| format!("connecting to {addr}"))?;
    stream.set_nodelay(true)?;
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let (to_bridge, outbox) = mpsc::channel::<ToBridge>();
    to_bridge.send(ToBridge::Hello { token })?;
    let mut writer = BufWriter::new(stream.try_clone()?);
    thread::Builder::new()
        .name("to-bridge".into())
        .spawn(move || {
            for msg in outbox {
                if msg.write(&mut writer).and_then(|_| writer.flush()).is_err() {
                    return;
                }
            }
        })?;

    let framebuffer = Arc::new(Mutex::new(Framebuffer {
        width: 0,
        height: 0,
        pixels: Vec::new(),
    }));
    let fb = Arc::clone(&framebuffer);
    thread::Builder::new()
        .name("from-bridge".into())
        .spawn(move || {
            let reason = match receive(BufReader::with_capacity(1 << 20, stream), &fb, &proxy) {
                Ok(()) => "the VM stopped".to_string(),
                Err(e) => format!("{e:#}"),
            };
            let _ = proxy.send_event(UserEvent::Closed(reason));
        })?;

    clipboard::watch(to_bridge.clone());

    let mut app = App {
        args,
        to_bridge,
        framebuffer,
        window: None,
        surface: None,
        pending_size: None,
        scroll: (0.0, 0.0),
        error: None,
    };
    let result = event_loop.run_app(&mut app);
    keyboard::shutdown();
    result?;
    match app.error {
        Some(e) if e != "the VM stopped" => anyhow::bail!(e),
        _ => Ok(()),
    }
}

/// Frames drawn in the window, for `ILEX_VIEWER_STATS`.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Read the bridge's messages into the framebuffer until it closes. With
/// `ILEX_VIEWER_STATS` set, print frame rates every two seconds.
fn receive(
    mut reader: BufReader<TcpStream>,
    fb: &Mutex<Framebuffer>,
    proxy: &EventLoopProxy<UserEvent>,
) -> Result<()> {
    let stats = std::env::var_os("ILEX_VIEWER_STATS").is_some();
    let (mut since, mut presents, mut bytes, mut drawn) =
        (Instant::now(), 0u64, 0u64, DRAWN.load(Ordering::Relaxed));
    loop {
        if stats && since.elapsed() >= Duration::from_secs(2) {
            let secs = since.elapsed().as_secs_f64();
            let now_drawn = DRAWN.load(Ordering::Relaxed);
            eprintln!(
                "ilex-viewer: {:.1} frames/s received, {:.1} drawn, {:.0} MiB/s",
                presents as f64 / secs,
                (now_drawn - drawn) as f64 / secs,
                bytes as f64 / secs / (1 << 20) as f64
            );
            (since, presents, bytes, drawn) = (Instant::now(), 0, 0, now_drawn);
        }
        let msg = match ToViewer::read(&mut reader) {
            Ok(m) => m,
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::UnexpectedEof) =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        match msg {
            ToViewer::Size { width, height } => {
                let mut fb = fb.lock().unwrap();
                fb.width = width as usize;
                fb.height = height as usize;
                fb.pixels = vec![0; fb.width * fb.height];
            }
            ToViewer::Rect {
                x,
                y,
                width,
                height,
                pixels,
            } => {
                bytes += pixels.len() as u64;
                let mut fb = fb.lock().unwrap();
                let (x, y, w, h) = (x as usize, y as usize, width as usize, height as usize);
                if x + w > fb.width || y + h > fb.height {
                    continue;
                }
                let stride = fb.width;
                for row in 0..h {
                    let dst = &mut fb.pixels[(y + row) * stride + x..(y + row) * stride + x + w];
                    let src = &pixels[row * w * 4..(row + 1) * w * 4];
                    for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
                        *d = u32::from_le_bytes([s[0], s[1], s[2], 0]);
                    }
                }
            }
            ToViewer::Present => {
                presents += 1;
                let _ = proxy.send_event(UserEvent::Present);
            }
            ToViewer::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                pixels,
            } => {
                let rgba = pixels
                    .chunks_exact(4)
                    .flat_map(|p| [p[2], p[1], p[0], p[3]])
                    .collect();
                let _ = proxy.send_event(UserEvent::Cursor {
                    width: width as u16,
                    height: height as u16,
                    hot_x: hot_x as u16,
                    hot_y: hot_y as u16,
                    rgba,
                });
            }
            ToViewer::Clipboard { text } => clipboard::set_from_guest(&text),
        }
    }
}

impl App {
    fn send(&self, msg: ToBridge) {
        let _ = self.to_bridge.send(msg);
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buffer) = surface.buffer_mut() else {
            return;
        };
        let (bw, bh) = (size.width as usize, size.height as usize);
        let fb = self.framebuffer.lock().unwrap();
        if fb.width == 0 || fb.height == 0 {
            buffer.fill(0);
        } else if (fb.width, fb.height) == (bw, bh) {
            buffer.copy_from_slice(&fb.pixels);
        } else {
            // The window was resized and the guest hasn't followed yet:
            // stretch its last frame (nearest neighbour) instead of showing
            // it in a corner.
            let xs: Vec<usize> = (0..bw).map(|x| x * fb.width / bw).collect();
            for y in 0..bh {
                let src = &fb.pixels[(y * fb.height / bh) * fb.width..][..fb.width];
                for (d, &sx) in buffer[y * bw..(y + 1) * bw].iter_mut().zip(&xs) {
                    *d = src[sx];
                }
            }
        }
        drop(fb);
        if buffer.present().is_ok() {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Window pixels to guest pixels (1:1, clamped to the guest's screen).
    fn guest_point(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let fb = self.framebuffer.lock().unwrap();
        let window = self.window.as_ref()?.inner_size();
        if fb.width == 0 || window.width == 0 || window.height == 0 || x < 0.0 || y < 0.0 {
            return None;
        }
        // 1:1 once the guest has the window's size; scaled while it catches
        // up (the frame is stretched then, see `redraw`).
        let gx = x * fb.width as f64 / window.width as f64;
        let gy = y * fb.height as f64 / window.height as f64;
        Some((
            (gx as usize).min(fb.width - 1) as u32,
            (gy as usize).min(fb.height - 1) as u32,
        ))
    }

    fn wheel(&self, button: Button, clicks: i32) {
        for _ in 0..clicks {
            self.send(ToBridge::Button { button, down: true });
            self.send(ToBridge::Button {
                button,
                down: false,
            });
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.args.title)
            .with_inner_size(LogicalSize::new(1600.0, 1000.0));
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                self.error = Some(format!("creating the window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let surface = softbuffer::Context::new(Rc::clone(&window))
            .and_then(|ctx| softbuffer::Surface::new(&ctx, Rc::clone(&window)));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                self.error = Some(format!("creating the drawing surface: {e}"));
                event_loop.exit();
                return;
            }
        }
        if let Ok(handle) = window.window_handle()
            && let RawWindowHandle::Win32(h) = handle.as_raw()
            && let Err(e) = keyboard::install(h.hwnd.get() as usize, self.to_bridge.clone())
        {
            eprintln!("ilex-viewer: {e:#} (the Windows key stays with Windows)");
        }
        self.pending_size = Some((window.inner_size(), Instant::now() - RESIZE_SETTLE));
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.send(ToBridge::Quit);
                // Give the writer a moment to send it.
                thread::sleep(Duration::from_millis(100));
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(size) => {
                self.pending_size = Some((size, Instant::now()));
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::Focused(false) => keyboard::release_all(),
            // Only keys the hook didn't take get here (see keyboard.rs).
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(scancode) = event.physical_key.to_scancode() {
                    keyboard::from_window(scancode, event.state == ElementState::Pressed);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if let Some((x, y)) = self.guest_point(position.x, position.y) {
                    self.send(ToBridge::Pointer { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let button = match button {
                    MouseButton::Left => Button::Left,
                    MouseButton::Middle => Button::Middle,
                    MouseButton::Right => Button::Right,
                    MouseButton::Back => Button::Side,
                    MouseButton::Forward => Button::Extra,
                    MouseButton::Other(_) => return,
                };
                self.send(ToBridge::Button {
                    button,
                    down: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => {
                        self.scroll.0 += p.x / PIXELS_PER_CLICK;
                        self.scroll.1 += p.y / PIXELS_PER_CLICK;
                        let whole = (self.scroll.0.trunc(), self.scroll.1.trunc());
                        self.scroll.0 -= whole.0;
                        self.scroll.1 -= whole.1;
                        whole
                    }
                };
                let (dx, dy) = (dx.round() as i32, dy.round() as i32);
                if dy > 0 {
                    self.wheel(Button::WheelUp, dy);
                } else if dy < 0 {
                    self.wheel(Button::WheelDown, -dy);
                }
                if dx > 0 {
                    self.wheel(Button::WheelRight, dx);
                } else if dx < 0 {
                    self.wheel(Button::WheelLeft, -dx);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Present => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                rgba,
            } => {
                if let (Some(window), Ok(source)) = (
                    &self.window,
                    CustomCursor::from_rgba(rgba, width, height, hot_x, hot_y),
                ) {
                    window.set_cursor(event_loop.create_custom_cursor(source));
                }
            }
            UserEvent::Closed(reason) => {
                self.error = Some(reason);
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.pending_size {
            Some((size, since)) if since.elapsed() >= RESIZE_SETTLE => {
                self.send(ToBridge::WindowSize {
                    width: size.width,
                    height: size.height,
                });
                self.pending_size = None;
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some((_, since)) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(since + RESIZE_SETTLE))
            }
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}
