//! `disk::` tier: A qcow2 written by `bcvk to-disk`.
//!
//! Boots the installed disk (UEFI, throwaway overlay) once and asserts over
//! SSH what only an install can show: the kernel arguments from `kargs.d`
//! are on the command line, the root filesystem is the one the install
//! config asks for, and the first boot left no failed units.

use anyhow::{Context, Result, bail};
use ilex_devkit::qemu::{Vm, VmConfig};
use libtest_mimic::{Failed, Trial};

use crate::common;

#[path = "vmtest.rs"]
pub(crate) mod vmtest;

use vmtest::Report;

pub fn trials() -> Vec<Trial> {
    vec![Trial::test("disk::install", || run().map_err(|e| Failed::from(format!("{e:#}"))))]
}

fn run() -> Result<()> {
    let ws = common::workspace()?;
    let disk = vmtest::disk(&ws)?;
    let vm = vmtest::boot(&ws, VmConfig::new("disk", disk))?;

    // Everything below is asserted against this one boot.
    let mut report = Report::default();
    report.check("system reaches `running`", system_running(&vm));
    report.check("no failed units", no_failed_units(&vm));
    report.check("kargs.d arguments are on the kernel command line", kargs(&vm));
    report.check("root filesystem matches the install config", root_filesystem(&vm));
    report.check("first-boot units succeeded", first_boot_units(&vm));
    report.check("bootc booted from the installed image", bootc_status(&vm));
    report.finish()
}

fn system_running(vm: &Vm) -> Result<()> {
    // `--wait` blocks until the boot transaction settles; it exits 0 only
    // for `running`, so read the state from stdout either way.
    let out = vm.ssh("timeout 300 systemctl is-system-running --wait")?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state == "running" { Ok(()) } else { bail!("system state is `{state}`") }
}

fn no_failed_units(vm: &Vm) -> Result<()> {
    let failed = vm.ssh_ok("systemctl --failed --no-legend --plain")?;
    if failed.is_empty() { Ok(()) } else { bail!("failed units:\n{failed}") }
}

/// Every `kargs = [...]` entry of `/usr/lib/bootc/kargs.d/*.toml` must be on
/// `/proc/cmdline`.
fn kargs(vm: &Vm) -> Result<()> {
    let mut wanted = Vec::new();
    for table in vmtest::guest_toml(vm, "/usr/lib/bootc/kargs.d/*.toml")? {
        if let Some(kargs) = table.get("kargs").and_then(|v| v.as_array()) {
            wanted.extend(kargs.iter().filter_map(|v| v.as_str().map(str::to_string)));
        }
    }
    if wanted.is_empty() {
        bail!("no kargs found in /usr/lib/bootc/kargs.d (is the image missing its kargs?)");
    }
    let cmdline = vm.ssh_ok("cat /proc/cmdline")?;
    let have: Vec<&str> = cmdline.split_whitespace().collect();
    let missing: Vec<_> = wanted.iter().filter(|k| !have.contains(&k.as_str())).collect();
    if missing.is_empty() {
        println!("     kargs {wanted:?} are on the command line");
        Ok(())
    } else {
        bail!("missing from /proc/cmdline: {missing:?}\ncmdline: {cmdline}")
    }
}

/// `[install.filesystem.root] type` of the install config is the filesystem
/// the physical root (`/sysroot`) was formatted with.
fn root_filesystem(vm: &Vm) -> Result<()> {
    let mut wanted = None;
    for table in vmtest::guest_toml(vm, "/usr/lib/bootc/install/*.toml")? {
        let ty = table
            .get("install")
            .and_then(|t| t.get("filesystem"))
            .and_then(|t| t.get("root"))
            .and_then(|t| t.get("type"))
            .and_then(|t| t.as_str());
        if let Some(ty) = ty {
            wanted = Some(ty.to_string());
        }
    }
    let wanted = wanted.context("no [install.filesystem.root] type in /usr/lib/bootc/install")?;
    let sysroot = vm.ssh_ok("findmnt -no FSTYPE /sysroot")?;
    let var = vm.ssh_ok("findmnt -no FSTYPE /var")?;
    println!("     install config: {wanted}; /sysroot is {sysroot}, /var is {var}");
    if sysroot != wanted {
        bail!("install config asks for {wanted} but /sysroot is {sysroot}");
    }
    if var != wanted {
        bail!("/var should live on the {wanted} root but is {var}");
    }
    Ok(())
}

/// What a freshly installed machine must have brought up on its own: the
/// services we reach it through and that make it a desktop, the ostree
/// mount setup, and bootc's own status trigger. (Whether first-boot-only
/// units *failed* is covered by `no_failed_units`.)
fn first_boot_units(vm: &Vm) -> Result<()> {
    let mut problems = Vec::new();
    for unit in [
        "sshd.service",
        "NetworkManager.service",
        "gdm.service",
        "ostree-remount.service",
        "bootc-status-updated-onboot.target",
    ] {
        let out = vm.ssh(&format!("systemctl is-active {unit}"))?;
        let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if state != "active" {
            problems.push(format!("{unit} is `{state}`"));
        }
    }
    if problems.is_empty() { Ok(()) } else { bail!("{}", problems.join("\n")) }
}

fn bootc_status(vm: &Vm) -> Result<()> {
    let json = vm.ssh_ok("bootc status --json")?;
    let status: serde_json::Value = serde_json::from_str(&json).context("parsing bootc status")?;
    let booted = &status["status"]["booted"];
    if booted.is_null() {
        bail!("bootc status shows no booted deployment: {json}");
    }
    let image = booted["image"]["image"]["image"].as_str().unwrap_or("?");
    println!("     booted image: {image}");
    Ok(())
}
