//! The wire protocol between the display bridge (`xtask vm --gui` in WSL,
//! [`crate::display`]) and `ilex-viewer.exe`, the native Windows window.
//!
//! One TCP connection, viewer to bridge. Every message is a type byte, a
//! little-endian `u32` payload length and the payload. The viewer speaks
//! first ([`ToBridge::Hello`] with the bridge's token); everything after
//! flows both ways at will.
//!
//! Frames travel as rectangles of raw BGRX pixels (`u32` per pixel,
//! `0x00RRGGBB` on little-endian: what X11, softbuffer and Windows DIBs all
//! use), only for the tiles that changed, closed by [`ToViewer::Present`].
//! WSL's virtual network moves about 2.4 GiB/s to Windows, so a whole 4K
//! frame (33 MB) fits about 70 times a second without compression.

use std::io::{self, Read, Write};

use anyhow::{Context, Result, bail};

/// Bridge to viewer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToViewer {
    /// The guest's framebuffer is now this size; all pixels follow as rects.
    Size { width: u32, height: u32 },
    /// A rectangle of pixels, `width * height` BGRX `u32`s, row by row.
    Rect {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    /// Show what arrived since the last `Present`.
    Present,
    /// The guest's pointer image, BGRA (straight alpha), and its hot spot.
    Cursor {
        width: u32,
        height: u32,
        hot_x: u32,
        hot_y: u32,
        pixels: Vec<u8>,
    },
    /// The guest's clipboard has this text now.
    Clipboard { text: String },
}

/// Viewer to bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToBridge {
    /// First message: the token the bridge printed.
    Hello {
        token: String,
    },
    /// A key, as a QEMU "qnum": the PC set-1 scancode, `| 0x80` for the
    /// `E0`-prefixed (extended) keys. That is Windows' own `scanCode` plus
    /// `LLKHF_EXTENDED`, so no keymap is needed anywhere.
    Key {
        qnum: u16,
        down: bool,
    },
    /// Pointer position in guest framebuffer pixels.
    Pointer {
        x: u32,
        y: u32,
    },
    Button {
        button: Button,
        down: bool,
    },
    /// The window's client area in physical pixels: make the guest this big.
    WindowSize {
        width: u32,
        height: u32,
    },
    /// The Windows clipboard has this text now.
    Clipboard {
        text: String,
    },
    /// The window was closed: stop the VM.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Side,
    Extra,
}

impl Button {
    /// QEMU's `InputButton` name for `input-send-event`.
    pub fn qemu_name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Middle => "middle",
            Button::Right => "right",
            Button::WheelUp => "wheel-up",
            Button::WheelDown => "wheel-down",
            Button::WheelLeft => "wheel-left",
            Button::WheelRight => "wheel-right",
            Button::Side => "side",
            Button::Extra => "extra",
        }
    }

    fn code(self) -> u8 {
        self as u8
    }

    fn from_code(c: u8) -> Result<Button> {
        const ALL: [Button; 9] = [
            Button::Left,
            Button::Middle,
            Button::Right,
            Button::WheelUp,
            Button::WheelDown,
            Button::WheelLeft,
            Button::WheelRight,
            Button::Side,
            Button::Extra,
        ];
        ALL.get(c as usize)
            .copied()
            .with_context(|| format!("unknown button {c}"))
    }
}

/// Largest payload either side accepts: an 8K frame and some.
const MAX_PAYLOAD: usize = 160 << 20;

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn frame(kind: u8, payload: &[u8], w: &mut impl Write) -> io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)
}

fn read_frame(r: &mut impl Read) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        bail!("message of {len} bytes is too big");
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    Ok((head[0], payload))
}

/// Reads little-endian fields off a payload.
struct Fields<'a>(&'a [u8]);

impl Fields<'_> {
    fn u32(&mut self) -> Result<u32> {
        if self.0.len() < 4 {
            bail!("message too short");
        }
        let (v, rest) = self.0.split_at(4);
        self.0 = rest;
        Ok(u32::from_le_bytes(v.try_into().unwrap()))
    }

    fn u16(&mut self) -> Result<u16> {
        if self.0.len() < 2 {
            bail!("message too short");
        }
        let (v, rest) = self.0.split_at(2);
        self.0 = rest;
        Ok(u16::from_le_bytes(v.try_into().unwrap()))
    }

    fn u8(&mut self) -> Result<u8> {
        let (&v, rest) = self.0.split_first().context("message too short")?;
        self.0 = rest;
        Ok(v)
    }

    fn rest(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn text(self) -> Result<String> {
        String::from_utf8(self.0.to_vec()).context("text is not UTF-8")
    }
}

impl ToViewer {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        let mut p = Vec::new();
        match self {
            ToViewer::Size { width, height } => {
                put_u32(&mut p, *width);
                put_u32(&mut p, *height);
                frame(1, &p, w)
            }
            ToViewer::Rect {
                x,
                y,
                width,
                height,
                pixels,
            } => {
                // Header first, then the pixels without copying them again.
                let mut head = Vec::with_capacity(21);
                head.push(2);
                head.extend_from_slice(&((16 + pixels.len()) as u32).to_le_bytes());
                for v in [x, y, width, height] {
                    head.extend_from_slice(&v.to_le_bytes());
                }
                w.write_all(&head)?;
                w.write_all(pixels)
            }
            ToViewer::Present => frame(3, &[], w),
            ToViewer::Cursor {
                width,
                height,
                hot_x,
                hot_y,
                pixels,
            } => {
                for v in [width, height, hot_x, hot_y] {
                    put_u32(&mut p, *v);
                }
                p.extend_from_slice(pixels);
                frame(4, &p, w)
            }
            ToViewer::Clipboard { text } => frame(5, text.as_bytes(), w),
        }
    }

    pub fn read(r: &mut impl Read) -> Result<ToViewer> {
        let (kind, payload) = read_frame(r)?;
        let mut f = Fields(&payload);
        Ok(match kind {
            1 => ToViewer::Size {
                width: f.u32()?,
                height: f.u32()?,
            },
            2 => {
                let (x, y, width, height) = (f.u32()?, f.u32()?, f.u32()?, f.u32()?);
                let pixels = f.rest();
                if pixels.len() != width as usize * height as usize * 4 {
                    bail!("rect {width}x{height} carries {} bytes", pixels.len());
                }
                ToViewer::Rect {
                    x,
                    y,
                    width,
                    height,
                    pixels,
                }
            }
            3 => ToViewer::Present,
            4 => {
                let (width, height, hot_x, hot_y) = (f.u32()?, f.u32()?, f.u32()?, f.u32()?);
                ToViewer::Cursor {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    pixels: f.rest(),
                }
            }
            5 => ToViewer::Clipboard { text: f.text()? },
            other => bail!("unknown message type {other} from the bridge"),
        })
    }
}

impl ToBridge {
    pub fn write(&self, w: &mut impl Write) -> io::Result<()> {
        let mut p = Vec::new();
        match self {
            ToBridge::Hello { token } => frame(101, token.as_bytes(), w),
            ToBridge::Key { qnum, down } => {
                p.extend_from_slice(&qnum.to_le_bytes());
                p.push(*down as u8);
                frame(102, &p, w)
            }
            ToBridge::Pointer { x, y } => {
                put_u32(&mut p, *x);
                put_u32(&mut p, *y);
                frame(103, &p, w)
            }
            ToBridge::Button { button, down } => frame(104, &[button.code(), *down as u8], w),
            ToBridge::WindowSize { width, height } => {
                put_u32(&mut p, *width);
                put_u32(&mut p, *height);
                frame(105, &p, w)
            }
            ToBridge::Clipboard { text } => frame(106, text.as_bytes(), w),
            ToBridge::Quit => frame(107, &[], w),
        }
    }

    pub fn read(r: &mut impl Read) -> Result<ToBridge> {
        let (kind, payload) = read_frame(r)?;
        let mut f = Fields(&payload);
        Ok(match kind {
            101 => ToBridge::Hello { token: f.text()? },
            102 => ToBridge::Key {
                qnum: f.u16()?,
                down: f.u8()? != 0,
            },
            103 => ToBridge::Pointer {
                x: f.u32()?,
                y: f.u32()?,
            },
            104 => ToBridge::Button {
                button: Button::from_code(f.u8()?)?,
                down: f.u8()? != 0,
            },
            105 => ToBridge::WindowSize {
                width: f.u32()?,
                height: f.u32()?,
            },
            106 => ToBridge::Clipboard { text: f.text()? },
            107 => ToBridge::Quit,
            other => bail!("unknown message type {other} from the viewer"),
        })
    }
}
