//! `iso::next_unattended`: an installation from the real ISO with no interface.
//!
//! The ISO boots through UEFI with Secure Boot beside a blank disk and a small
//! ext4 volume labelled `ILEX-ANSWERS` that holds `answers.toml`. Nothing is
//! typed and no page is drawn: the udev rule that sees the label starts
//! `ilex-installer-answers.service`, which stops the interface's session and
//! drives the helper (`Probe`, `Plan`, `SetSecret`, `Apply`, `State`) as root,
//! prints `ILEX-INSTALLER-ANSWERS` lines, and powers the machine off when the
//! installation has succeeded. The harness follows those lines on the serial
//! console, then boots the disk it installed (without the ISO) and asserts what
//! the interactive trials assert of an installed system: the digest the plan
//! pinned, the origin that enforces the container signature policy, the
//! administrator logging in with the password and `sudo`, the answers applied,
//! the system running with nothing failed and GDM up.
//!
//! Two refusals come first, each on a VM of its own that must power nothing on
//! and write nothing: an answers file that is not valid (the line and column,
//! never its text), and one whose `confirm-erase` is not the disk.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::core::{Capability, Requirement, SecurityLaunch, VmProvider};
use ilex_devkit::provider::LinuxProvider;
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_devkit::installer_model::{answers::Answers, probe::Probe};
use ilex_devkit::cmd;

use ilex_vmctl::Vmctl;

use super::{
    Guest, PAGE_TIMEOUT, blank_disk_of, count_markers, diagnostics, fields, iso_path, wait_for_marker,
};
use crate::common;
use crate::installer::apply::{ADMIN, Expected, PASSWORD, check_installed, gdm_screenshot};

/// The target's virtual size (`41G` for qemu-img): how the harness finds it in
/// a probe, since the id a virtio disk gets without a serial is its PCI slot.
const TARGET_BYTES: u64 = 41 << 30;
/// The unattended installation, from the pull to the power-off.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(40 * 60);
/// The ISO's VM: the pull streams into the target and the helper, bootc and
/// the page cache share the rest.
const MEMORY: &str = "6G";
const MARKER: &str = "ILEX-INSTALLER-ANSWERS";

/// `answers.toml` for the target disk: answers that are not the defaults, so
/// that applying them is visible.
fn answers(disk: &str, confirm: &str) -> String {
    format!(
        r#"schema = 1

[install]
disk = "{disk}"
mode = "whole-disk"
confirm-erase = "{confirm}"

[encryption]
enabled = false

[system]
keyboard = "de-latin1"
language = "de_DE.UTF-8"
time-zone = "Europe/Berlin"
host-name = "ilextest"

[administrator]
name = "{ADMIN}"
password = "{}"
"#,
        // A TOML basic string: the quotes of the password are escaped.
        PASSWORD.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// A small ext4 image labelled `ILEX-ANSWERS` with `text` as `answers.toml`:
/// `mke2fs -d` fills it from a directory, no mount and no root needed.
fn answers_volume(root: &Path, text: &str) -> Result<std::path::PathBuf> {
    let content = root.join("answers");
    fs::create_dir_all(&content)?;
    fs::write(content.join("answers.toml"), text)?;
    let image = root.join("answers.img");
    let _ = fs::remove_file(&image);
    cmd::require("mke2fs")?;
    cmd::run(
        Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-L", "ILEX-ANSWERS", "-d"])
            .arg(&content)
            .arg(&image)
            .arg("16M"),
    )?;
    Ok(image)
}

/// The ISO, a blank target and an answers volume, booted: UEFI, Secure Boot.
fn boot(root: &Path, text: &str) -> Result<Guest> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let target = root.join("target.qcow2");
    blank_disk_of(&target, "41G")?;
    let volume = answers_volume(root, text)?;
    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    let mut config = VmConfig::new("iso-next-unatt", &target);
    config.snapshot = false;
    config.memory = memory;
    config.cdrom = Some(iso);
    config.extra_disks = vec![volume];
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    let provider = LinuxProvider::new(ws.clone())?;
    let caps = provider.probe(&spec)?;
    caps.require_provisioning(Capability::FirmwareEnforcement)?;
    let vm = provider.create(spec)?;
    // What the guard removes with the VM is a directory of its own: the disk
    // it installed to is read after it is gone.
    let scratch = root.join("vm");
    fs::create_dir_all(&scratch)?;
    let mut guest = Guest {
        provider,
        vm,
        root: scratch,
    };
    guest.provider.start(&mut guest.vm)?;
    ensure!(
        guest.vm.running_mut()?.info.secure_boot,
        "the VM was not started with Secure Boot"
    );
    Ok(guest)
}

pub(super) fn run() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let root = ws.state_dir().join("iso-next-fixtures").join("unattended");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;

    refuse_invalid(&root, &artifacts)?;
    // Which also says what the disk is called: the same VM, with the same
    // devices in the same order, names it the same way.
    let target = refuse_confirmation(&root, &artifacts)?;
    println!("iso::next unattended: the target is {target}");

    // The installation.
    let mut guest = boot(&root, &answers(&target, &target))?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let started = Instant::now();
    let outcome = install(vm, &serial, &artifacts);
    if let Err(error) = &outcome {
        diagnostics(vm, &artifacts, "unattended");
        return Err(anyhow::anyhow!("{error:#}"));
    }
    let console = outcome?;
    let _ = fs::write(artifacts.join("next-unattended-serial.txt"), &console);
    println!(
        "iso::next unattended: installed and powered off in {:?}",
        started.elapsed()
    );
    let plan = fields(&console, MARKER, "plan")?;
    let digest = plan.get("image").context("the plan line has no image=")?.clone();
    ensure!(digest.starts_with("sha256:"), "the plan's image: {digest}");
    // No interface was ever drawn: the session did not get to start (or was
    // stopped) before an answers file was found, and no page was reported.
    ensure!(
        count_markers(&console, "ILEX-INSTALLER", Some("page")) == 0,
        "an interface drew a page during an unattended installation: {console}"
    );
    // The machine is off; the disk it installed boots on its own.
    drop(guest);
    let mut config = VmConfig::new("iso-next-unatt-t", root.join("target.qcow2"));
    config.snapshot = true;
    config.memory = "4G".into();
    let mut installed = Vm::boot(&ws, &config)?;
    installed
        .wait_ssh(PAGE_TIMEOUT)
        .context("the installed disk did not come up")?;
    check_installed(
        &|name, text| {
            let _ = qemu::write_artifact(&artifacts, &format!("next-unattended-{name}"), text);
        },
        &installed,
        &Expected {
            digest: &digest,
            target: "ghcr.io/reidond/ilex:generic.gnome",
            host: "ilextest",
            zone: "Europe/Berlin",
            locale: "de_DE.UTF-8",
            keymap: "de-latin1",
            layout: "de",
            admin: ADMIN,
            password: PASSWORD,
        },
    )?;
    gdm_screenshot(&mut installed, &artifacts.join("next-unattended-gdm.png"))?;
    // The ISO's own answers file was never written to, and the volume is what
    // it was (the helper read it once, read-only).
    let volume = fs::read_to_string(root.join("answers/answers.toml"))?;
    ensure!(Answers::parse(&volume).is_ok(), "the answers fixture is not valid");
    Ok(())
}

/// Boot to the end of the installation: the markers say what happened. On the
/// way, once the pull is under way, the screen is saved: what a person at the
/// machine sees of an unattended installation.
fn install(vm: &mut Vm, serial: &Path, artifacts: &Path) -> Result<String> {
    let started = Instant::now();
    let mut shown = false;
    let console = loop {
        let console = fs::read(serial)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        if !shown
            && ["0.4", "0.5", "0.6"]
                .iter()
                .any(|p| console.contains(&format!("stage=image progress={p}")))
        {
            shown = true;
            let _ = Vmctl::from_info(vm.info.clone())
                .save_screenshot(&artifacts.join("next-unattended-progress.png"));
        }
        if count_markers(&console, MARKER, Some("result")) >= 1 {
            break console;
        }
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} before the installation ended; serial: {console}");
        }
        ensure!(
            started.elapsed() < INSTALL_TIMEOUT,
            "the unattended installation did not end in {INSTALL_TIMEOUT:?}; serial: {console}"
        );
        std::thread::sleep(Duration::from_secs(2));
    };
    let result = fields(&console, MARKER, "result")?;
    ensure!(
        result.get("result").map(String::as_str) == Some("success"),
        "the unattended installation did not succeed: {}",
        console
            .lines()
            .filter(|l| l.contains(MARKER))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // It powers the machine off by itself.
    let started = Instant::now();
    loop {
        if vm.exited()?.is_some() {
            return Ok(console);
        }
        if started.elapsed() > Duration::from_secs(300) {
            bail!("the machine did not power off after a successful installation");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// An answers file that is not valid: refused with where and not what, and
/// nothing is written.
fn refuse_invalid(root: &Path, artifacts: &Path) -> Result<()> {
    let dir = root.join("invalid");
    fs::create_dir_all(&dir)?;
    // A password in a line that is wrong: the error must not quote it.
    let secret = "S3cret-never-to-be-printed";
    let text = format!("schema = 1\n[administrator]\npassword = \"{secret}\" extra\n");
    let mut guest = boot(&dir, &text)?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, "ILEX-INSTALLER-ANSWERS", "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, "unattended-invalid"))?;
    let _ = fs::write(artifacts.join("next-unattended-invalid-serial.txt"), &console);
    ensure!(
        console.contains("result=failed") && console.contains("line 3"),
        "an invalid answers file was not refused with its position: {console}"
    );
    ensure!(!console.contains(secret), "the password was printed");
    assert_untouched(vm, &dir)?;
    println!("iso::next unattended: an invalid answers file is refused by position, without its text");
    Ok(())
}

/// Whole-disk mode erases: `confirm-erase` has to repeat the disk's id. Returns
/// the id of the target disk, as this VM's helper names it.
fn refuse_confirmation(root: &Path, artifacts: &Path) -> Result<String> {
    let dir = root.join("confirmation");
    fs::create_dir_all(&dir)?;
    let mut guest = boot(&dir, &answers("a-disk", "another-disk"))?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, "ILEX-INSTALLER-ANSWERS", "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, "unattended-confirmation"))?;
    let _ = fs::write(artifacts.join("next-unattended-confirmation-serial.txt"), &console);
    ensure!(
        console.contains("result=failed") && console.contains("confirm-erase"),
        "a wrong confirm-erase was not refused: {console}"
    );
    assert_untouched(vm, &dir)?;
    println!("iso::next unattended: a wrong confirm-erase is refused and the disk is untouched");
    // The helper's own probe: the disk of the target's size.
    let reply = vm.ssh_ok("busctl --system --json=short call org.ilex.Installer1 /org/ilex/Installer1 org.ilex.Installer1 Probe")?;
    let reply: serde_json::Value = serde_json::from_str(&reply).context("busctl's JSON")?;
    let probe: Probe = serde_json::from_str(reply["data"][0].as_str().context("busctl's data")?)?;
    let target = probe
        .disks
        .iter()
        .find(|d| d.size_bytes == TARGET_BYTES)
        .with_context(|| {
            format!(
                "no disk of {TARGET_BYTES} bytes in the probe: {:?}",
                probe.disks.iter().map(|d| (&d.id, d.size_bytes)).collect::<Vec<_>>()
            )
        })?;
    Ok(target.id.clone())
}

/// The target has no partition table: nothing was written to it.
fn assert_untouched(vm: &mut Vm, dir: &Path) -> Result<()> {
    vm.wait_ssh(PAGE_TIMEOUT)
        .context("the live system did not accept the harness's SSH key")?;
    let out = vm.ssh_ok("lsblk -n -o NAME,TYPE,PTTYPE,FSTYPE /dev/vda; systemctl is-active ilex-installer-session.service || true")?;
    ensure!(
        !out.contains("part") && !out.contains("gpt"),
        "the refused installation wrote to the disk: {out}"
    );
    ensure!(
        dir.join("target.qcow2").is_file(),
        "the target disk image is gone"
    );
    Ok(())
}
