//! Framebuffer frames: decode QEMU's PNG, tell blank from not blank, shrink
//! for agents, re-encode.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

/// An 8-bit RGB image.
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// `width * height * 3` bytes, row-major.
    pub rgb: Vec<u8>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Frame({}x{})", self.width, self.height)
    }
}

impl Frame {
    /// Decode a PNG (as written by QEMU's `screendump`).
    pub fn decode(png_bytes: &[u8]) -> Result<Frame> {
        let mut decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().context("reading PNG header")?;
        let mut buf = vec![0; reader.output_buffer_size().context("PNG too large")?];
        let info = reader.next_frame(&mut buf).context("decoding PNG")?;
        let (width, height) = (info.width, info.height);
        let data = &buf[..info.buffer_size()];
        let rgb = match (info.color_type, info.bit_depth) {
            (png::ColorType::Rgb, png::BitDepth::Eight) => data.to_vec(),
            (png::ColorType::Rgba, png::BitDepth::Eight) => {
                data.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect()
            }
            (png::ColorType::Grayscale, png::BitDepth::Eight) => {
                data.iter().flat_map(|&g| [g, g, g]).collect()
            }
            (png::ColorType::GrayscaleAlpha, png::BitDepth::Eight) => {
                data.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0]]).collect()
            }
            (ct, bd) => bail!("unsupported PNG format {ct:?} {bd:?}"),
        };
        Ok(Frame { width, height, rgb })
    }

    pub fn encode_png(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&self.rgb)?;
        writer.finish()?;
        Ok(out)
    }

    /// Shrink so the longer edge is at most `max_edge`, averaging pixels
    /// (box filter). Returns a clone when it already fits.
    pub fn downscale(&self, max_edge: u32) -> Frame {
        let long = self.width.max(self.height);
        if max_edge == 0 || long <= max_edge {
            return self.clone();
        }
        let scale = long as f64 / max_edge as f64;
        let width = ((self.width as f64 / scale).round() as u32).max(1);
        let height = ((self.height as f64 / scale).round() as u32).max(1);
        let mut rgb = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            let y0 = (y as f64 * scale) as u32;
            let y1 = (((y + 1) as f64 * scale).ceil() as u32).clamp(y0 + 1, self.height);
            for x in 0..width {
                let x0 = (x as f64 * scale) as u32;
                let x1 = (((x + 1) as f64 * scale).ceil() as u32).clamp(x0 + 1, self.width);
                let mut sum = [0u32; 3];
                for sy in y0..y1 {
                    let row = (sy * self.width) as usize * 3;
                    for sx in x0..x1 {
                        let i = row + sx as usize * 3;
                        sum[0] += self.rgb[i] as u32;
                        sum[1] += self.rgb[i + 1] as u32;
                        sum[2] += self.rgb[i + 2] as u32;
                    }
                }
                let n = (y1 - y0) * (x1 - x0);
                rgb.extend(sum.iter().map(|s| (s / n) as u8));
            }
        }
        Frame { width, height, rgb }
    }

    /// Roughly 100k evenly spread pixels.
    fn samples(&self) -> impl Iterator<Item = [u8; 3]> + '_ {
        let total = (self.width as usize) * (self.height as usize);
        let stride = (total / 100_000).max(1);
        // A stride that shares a factor with the width would sample a few
        // columns only; keep it odd.
        let stride = if stride.is_multiple_of(2) { stride + 1 } else { stride };
        (0..total).step_by(stride).map(move |p| {
            let i = p * 3;
            [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
        })
    }

    /// Share of sampled pixels that have the most common colour (1.0 for a
    /// solid screen).
    pub fn dominant_fraction(&self) -> f64 {
        let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
        let mut n = 0u32;
        for px in self.samples() {
            *counts.entry(px).or_default() += 1;
            n += 1;
        }
        counts.values().copied().max().unwrap_or(0) as f64 / n.max(1) as f64
    }

    /// A screen with (almost) nothing on it: one colour covers 99.9% or more
    /// of the sampled pixels. A black screen before the display initialises
    /// is blank; a boot splash, a login screen or a desktop is not.
    pub fn is_blank(&self) -> bool {
        self.dominant_fraction() >= 0.999
    }

    /// Cheap fingerprint of the picture, for "did it change / has it
    /// settled" checks. Identical frames hash identically.
    pub fn fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for px in self.samples() {
            for b in px {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        h
    }
}
