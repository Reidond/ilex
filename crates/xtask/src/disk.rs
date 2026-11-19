//! `cargo xtask disk`: write a qcow2 or raw disk with `bcvk to-disk`.
//!
//! The disk lands in the checkout's state directory
//! (`<state dir>/disks/<image>.qcow2`) unless `--out` says otherwise. bcvk
//! skips the install when the image hasn't changed since the disk was written.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::Workspace;
use ilex_devkit::qemu::{self, DiskFormat, DiskSpec};

use crate::build;
mod check;

#[derive(
    clap::ValueEnum,
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
)]
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

/// The virtual disk size when `--size` isn't given.
const DEFAULT_SIZE: &str = "30G";

#[derive(clap::Args, Debug, serde::Serialize, serde::Deserialize)]
pub struct Args {
    /// Inject alias inspection/store failures in disposable CLI fixtures (Linux)
    #[arg(long)]
    pub self_check: bool,
    #[command(flatten)]
    pub selection: crate::select::Args,
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
    #[arg(long, default_value = DEFAULT_SIZE)]
    pub size: String,
    /// Private development composition (pinned SSH and session integration); never published
    #[arg(long)]
    #[serde(default)]
    pub development: bool,
    /// Internal request from the Windows security harness to the builder.
    #[arg(skip)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gen2: Option<Gen2>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Gen2 {
    pub build: bool,
    pub no_pull: bool,
}

/// The command line's defaults, for `xtask vm --disk`, which writes its disk
/// without parsing one. (A derived `Default` left `size` empty, and bcvk
/// rejects `--disk-size ''`.)
impl Default for Args {
    fn default() -> Self {
        Self {
            self_check: false,
            selection: crate::select::Args::default(),
            format: Format::default(),
            image: None,
            out: None,
            size: DEFAULT_SIZE.into(),
            development: false,
            gen2: None,
        }
    }
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    if args.self_check {
        return check::run(ws);
    }
    let (path, image) = ensure(ws, &args)?;
    write_metadata(ws, &args, &path, &image)?;
    println!("{}", path.display());
    Ok(())
}

/// Write the disk for `args` (or reuse one) and return its path and image.
/// The image must already be built (`cargo xtask build`).
pub fn ensure(ws: &Workspace, args: &Args) -> Result<(PathBuf, String)> {
    let image = if let Some(gen2) = &args.gen2 {
        let base = crate::test::image_under_test_with_reference(
            ws,
            &crate::test::Args {
                selection: args.selection.clone(),
                build: gen2.build,
                no_pull: gen2.no_pull,
                filters: vec![],
                test_args: vec![],
            },
            args.image.clone(),
        )?;
        crate::dev_image::ensure(ws, &base, true)?
    } else {
        let resolved = args.selection.resolve(ws, args.image.as_deref(), None)?;
        resolved.require_match()?;
        if args.development {
            crate::dev_image::ensure(ws, &resolved.image, false)?
        } else {
            resolved.image
        }
    };
    Ok((ensure_image(ws, args, &image)?, image))
}

/// `vm` checked its resolved base already; do not read selection again for
/// that reference or for the viewer layer derived from it.
pub fn ensure_image(ws: &Workspace, args: &Args, image: &str) -> Result<PathBuf> {
    if !build::image_exists(image) {
        bail!("image {image} not found in local container storage; run `cargo xtask build` first");
    }
    let mut spec = DiskSpec::new(image);
    spec.format = args.format.into();
    spec.size = args.size.clone();
    spec.out = args.out.clone();
    let path = qemu::ensure_disk(ws, &spec)?;
    eprintln!("disk for {image}: {}", path.display());
    Ok(path)
}

/// Authenticated installed-disk manifest, shared by local and remote delivery.
pub fn write_metadata(ws: &Workspace, a: &Args, disk: &std::path::Path, image: &str) -> Result<()> {
    let info = ilex_devkit::cmd::output(
        std::process::Command::new("qemu-img")
            .args(["info", "--output=json"])
            .arg(disk),
    )?;
    let info: serde_json::Value = serde_json::from_str(&info)?;
    let format = match a.format {
        crate::disk::Format::Qcow2 => "qcow2",
        crate::disk::Format::Raw => "raw",
    };
    ensure!(
        info["format"] == format && info["virtual-size"].as_u64().is_some_and(|s| s > 0),
        "installed disk format/size mismatch"
    );
    ensure!(
        info["backing-filename"].is_null(),
        "installed base disk must be standalone"
    );
    use ilex_devkit::core::{self, BuildExecutor};
    let executor = ilex_devkit::executor::LocalBuildExecutor::new(ws.clone())?
        .probe()?
        .executor_id;
    let checksum = hash_file(disk)?;
    let metadata = core::DiskArtifact {
    schema: 1, architecture: core::Architecture::X86_64,
    format: if a.format == crate::disk::Format::Qcow2 { core::DiskFormat::Qcow2 } else { core::DiskFormat::Raw },
    virtual_bytes: info["virtual-size"].as_u64().unwrap(),
    allocated_bytes: info["actual-size"].as_u64().context("disk allocation size missing")?,
    sector_bytes: 512,
    image: core::ImageIdentity { executor_id: executor.clone(), local_reference: image.into(),
        immutable_id: ilex_devkit::bcvk::image_id(image)?,
        registry_digest: Some(ilex_devkit::image::image_digest(image)?).filter(|s| !s.is_empty()),
        architecture: core::Architecture::X86_64 },
    install_recipe_hash: core::digest(serde_json::to_vec(&serde_json::json!({"bcvk":"0.22.0","format":format,"size":a.size,"firmware":"uefi"}))?.as_slice()),
    firmware: core::Firmware::Uefi,
    // The base remains unenrolled. The existing QEMU fixture injects
    // its public key through systemd credentials at each test boot.
    provisioning_hash: if a.gen2.is_some() { core::digest(b"ilex-gen2-v1;seed-host-key;seed-access-key") } else if a.development { core::digest(b"ilex-dev-session-v2;fwcfg-host-key;fwcfg-access-key") } else { core::digest(b"unenrolled-base;qemu-systemd-ssh-credentials-v1") },
    location: core::ArtifactLocation { executor_id: executor, path: disk.to_path_buf(), sha256: checksum },
};
    std::fs::write(
        disk.with_file_name(format!(
            "{}.disk.json",
            disk.file_name().unwrap().to_string_lossy()
        )),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    Ok(())
}

pub(crate) fn hash_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hash = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}
