//! `cargo xtask disk`: write a qcow2 or raw disk with `bcvk to-disk`.
//!
//! The disk lands in the checkout's state directory
//! (`<state dir>/disks/<image>.qcow2`) unless `--out` says otherwise. bcvk
//! skips the install when the image hasn't changed since the disk was written.

use std::path::PathBuf;

use anyhow::{Result, bail};
use ilex_devkit::qemu::{self, DiskFormat, DiskSpec};
use ilex_devkit::Workspace;

use crate::build;

#[derive(clap::ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    #[default]
    Qcow2,
    Raw,
}

impl From<Format> for DiskFormat {
    fn from(f: Format) -> Self {
        match f {
            Format::Qcow2 => DiskFormat::Qcow2,
            Format::Raw => DiskFormat::Raw,
        }
    }
}

#[derive(clap::Args, Debug, Default)]
pub struct Args {
    /// Disk image format
    #[arg(long, value_enum, default_value_t = Format::Qcow2)]
    pub format: Format,
    /// Image tag (e.g. `candidate`) or reference to install (defaults to this checkout's image)
    #[arg(long)]
    pub image: Option<String>,
    /// Where to write the disk (defaults to the checkout's state directory)
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Virtual disk size
    #[arg(long, default_value = "30G")]
    pub size: String,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let path = ensure(ws, &args)?;
    println!("{}", path.display());
    Ok(())
}

/// Write the disk for `args` (or reuse an up-to-date one) and return its path.
/// The image must already be built (`cargo xtask build`).
pub fn ensure(ws: &Workspace, args: &Args) -> Result<PathBuf> {
    let image = ws.resolve_image(args.image.as_deref());
    if !build::image_exists(&image) {
        bail!("image {image} not found in local container storage; run `cargo xtask build` first");
    }
    let mut spec = DiskSpec::new(&image);
    spec.format = args.format.into();
    spec.size = args.size.clone();
    spec.out = args.out.clone();
    let path = qemu::ensure_disk(ws, &spec)?;
    eprintln!("disk for {image}: {}", path.display());
    Ok(path)
}
