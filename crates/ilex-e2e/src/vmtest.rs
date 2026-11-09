//! Helpers for the tiers that boot a disk (`disk::`, `graphical::`): get a
//! disk image for the image under test, and report many checks from one boot.
//!
//! Shared through `disk.rs` (`crate::disk::vmtest`) because `main.rs` is not
//! this tier's file.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ilex_devkit::qemu::{self, DiskFormat, DiskSpec, Vm, VmConfig};
use ilex_devkit::{Workspace, cmd};

use crate::common;

/// How long a freshly installed disk gets to bring up sshd.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(420);

/// The qcow2 for the image under test: `ILEX_DISK` if set (a ready-made
/// disk, skips the install), otherwise `bcvk to-disk` of `ILEX_IMAGE` / this
/// checkout's image into the state directory. bcvk reuses an up-to-date disk.
pub fn disk(ws: &Workspace) -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("ILEX_DISK").filter(|p| !p.is_empty()) {
        return Ok(path.into());
    }
    let image = common::image()?;
    if cmd::probe(std::process::Command::new("podman").args(["image", "exists", &image])).is_none() {
        bail!("image {image} not found in local container storage (run `cargo xtask build`, or set ILEX_IMAGE)");
    }
    let mut spec = DiskSpec::new(&image);
    spec.format = DiskFormat::Qcow2;
    qemu::ensure_disk(ws, &spec)
}

/// Boot `config` and wait for SSH.
pub fn boot(ws: &Workspace, config: VmConfig) -> Result<Vm> {
    let mut vm = Vm::boot(ws, &config)?;
    vm.wait_ssh(BOOT_TIMEOUT).with_context(|| format!("booting {}", vm.info.name))?;
    println!("{} is up: {}", vm.info.name, vm.info.ssh_command_line());
    Ok(vm)
}

/// Collects named checks from one boot, so one failure doesn't hide the rest.
#[derive(Default)]
pub struct Report {
    failures: Vec<String>,
}

impl Report {
    pub fn check(&mut self, name: &str, result: Result<()>) {
        match result {
            Ok(()) => println!("ok   {name}"),
            Err(e) => {
                println!("FAIL {name}: {e:#}");
                self.failures.push(format!("{name}: {e:#}"));
            }
        }
    }

    /// Has any check failed so far?
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    pub fn finish(self) -> Result<()> {
        if self.failures.is_empty() {
            Ok(())
        } else {
            bail!("{} check(s) failed:\n  {}", self.failures.len(), self.failures.join("\n  "))
        }
    }
}

/// Entries of `[install.filesystem.root] type` / `kargs = [...]` live in toml
/// drop-ins in the image; read them from the booted guest so the test checks
/// what the build actually shipped.
pub fn guest_toml(vm: &Vm, glob: &str) -> Result<Vec<toml::Table>> {
    let files = vm.ssh_ok(&format!("ls {glob}"))?;
    files
        .lines()
        .map(|f| {
            let text = vm.ssh_ok(&format!("cat {f}"))?;
            text.parse::<toml::Table>().with_context(|| format!("parsing {f}"))
        })
        .collect()
}
