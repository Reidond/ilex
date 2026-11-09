//! `iso::` tier (opt-in, slow): install from the installer ISO.
//!
//! The plan's phase-5 done-when is "a machine installed from the ISO shows
//! `ghcr.io/<owner>/ilex:latest` in `bootc status`". This tier does exactly
//! that in QEMU/KVM with UEFI firmware:
//!
//! 1. Boot the ISO with nothing else attached and wait for Anaconda's welcome
//!    screen; save a screenshot (the installer must call itself ilex and show
//!    no pre-release banner).
//! 2. Boot it again onto a blank disk, with a test-only kickstart on a second
//!    disk labelled `OEMDRV`, which Anaconda picks up on its own. The kickstart
//!    only answers what the shipped `interactive-defaults.ks` leaves open (the
//!    language, the disk, the root account); the `bootc` command, and with it
//!    the payload and the image the machine tracks, are the shipped ones. Its
//!    `%pre` asks Anaconda itself what product it thinks it is and prints that
//!    on the serial console.
//! 3. Boot the installed disk and assert over SSH: `bootc status` names the
//!    booted image and tracks the target, the system reaches `running` with no
//!    failed units, and GDM is up (screenshot).
//!
//! It only exists when a filter names it (`cargo xtask test iso::`), because
//! it builds an ISO and runs an install: tens of minutes. `cargo xtask test
//! iso::` builds the ISO from the image under test first; `ILEX_ISO=<path>`
//! uses a ready-made one (then `ILEX_ISO_DIGEST` and `ILEX_ISO_TARGET` say what
//! it embeds and tracks, when known).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_devkit::{Workspace, cmd};
use ilex_vmctl::{Frame, Vmctl, WaitCond};
use libtest_mimic::{Failed, Trial};

use crate::common;
use crate::disk::vmtest::{self, Report};

/// Anaconda's welcome screen has to come up this soon after the ISO boots.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(300);
/// Partitioning, copying the payload out of the ISO's container storage and
/// installing the bootloader.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(1800);
/// The installer's display mode (`VmConfig::new`).
const INSTALLER_SCREEN: (u32, u32) = (1280, 800);
/// The installer keeps everything it writes in RAM, and importing the payload
/// into the new root first unpacks layers into its `/var/tmp`: a ~5 GB image
/// needs well over 6 GiB of guest memory.
const INSTALLER_MEMORY: &str = "16G";
/// What a bootc root needs: the system disk of the install (sparse).
const DISK_SIZE: &str = "40G";
/// What the installed machine tracks when nothing else is asked.
const DEFAULT_TARGET: &str = "ghcr.io/reidond/ilex:latest";

/// Printed on the serial console by the kickstart's `%post` once the payload
/// is installed; its absence means the installer gave up.
const INSTALL_DONE: &str = "ILEX-INSTALL-DONE";

/// Test-only answers, delivered on a disk labelled `OEMDRV` (Anaconda reads
/// `ks.cfg` from it without any boot argument). Anaconda only reads the
/// shipped `interactive-defaults.ks` when there is no kickstart (without it,
/// it falls back to an RPM install from a mirror), so this includes it: the
/// `bootc` command in it, and with it the payload and the image the machine
/// tracks, are the shipped ones.
const KICKSTART: &str = r#"# Test-only: unattended answers for what interactive-defaults.ks leaves open.
%include /usr/share/anaconda/interactive-defaults.ks
cmdline
lang en_US.UTF-8
keyboard us
timezone UTC --utc
rootpw --lock
zerombr
clearpart --all --initlabel
autopart
poweroff

# Ask Anaconda what product it thinks it is, with its own code, and say so on
# the serial console (the harness reads serial.log).
%pre --erroronfail
python3 - > /dev/ttyS0 2>&1 <<'PY'
from pyanaconda.core.product import get_product_values
print("ILEX-PRODUCT", get_product_values())
PY
%end

%post --nochroot
echo ILEX-INSTALL-DONE > /dev/ttyS0
%end
"#;

pub fn trials() -> Vec<Trial> {
    vec![Trial::test("iso::install", || {
        run().map_err(|e| Failed::from(format!("{e:#}")))
    })]
}

fn run() -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let artifacts = qemu::artifact_dir(&ws, "iso")?;
    clear_artifacts(&artifacts);
    let work = ws.state_dir().join("iso-test");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let mut report = Report::default();
    report.check(
        "Anaconda welcome screen",
        welcome(&ws, &iso, &work, &artifacts),
    );

    let disk = work.join("ilex.qcow2");
    let vars = work.join("OVMF_VARS.fd");
    let installed = unattended_install(&ws, &iso, &disk, &vars, &work, &artifacts, &mut report);
    if installed {
        installed_system(&ws, &disk, &vars, &artifacts, &mut report)?;
    }
    println!("artifacts: {}", artifacts.display());
    report.finish()
}

fn iso_path() -> Result<PathBuf> {
    let Some(path) = std::env::var_os("ILEX_ISO").filter(|p| !p.is_empty()) else {
        bail!("ILEX_ISO is not set: run `cargo xtask test iso::`, which builds the ISO first");
    };
    let path = PathBuf::from(path);
    if !path.is_file() {
        bail!("ILEX_ISO={} is not a file", path.display());
    }
    Ok(path)
}

fn target() -> String {
    std::env::var("ILEX_ISO_TARGET")
        .ok()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| DEFAULT_TARGET.to_string())
}

// ---------------------------------------------------------------------------
// 1. Anaconda welcome screen
// ---------------------------------------------------------------------------

/// Boot the ISO interactively and save Anaconda's welcome screen.
fn welcome(ws: &Workspace, iso: &Path, work: &Path, artifacts: &Path) -> Result<()> {
    let disk = work.join("welcome.qcow2");
    blank_disk(&disk)?;
    let mut config = VmConfig::new("iso-welcome", &disk);
    config.snapshot = false;
    config.memory = "6G".into();
    config.vcpus = 4;
    config.cdrom = Some(iso.to_path_buf());
    config.efi_vars = Some(work.join("welcome-OVMF_VARS.fd"));
    let mut vm = Vm::boot(ws, &config)?;
    let ctl = Vmctl::from_info(vm.info.clone());

    let started = Instant::now();
    let mut previous = None;
    let frame = loop {
        if let Some(status) = vm.exited()? {
            bail!("the installer VM exited with {status} before Anaconda came up");
        }
        if let Ok(frame) = ctl.screenshot() {
            let fp = frame.fingerprint();
            // Anaconda's window is light; GRUB, the kernel's console and the
            // splash are dark. Two identical frames in a row: it has settled.
            if (frame.width, frame.height) == INSTALLER_SCREEN
                && is_light(&frame)
                && previous == Some(fp)
            {
                break frame;
            }
            previous = Some(fp);
        }
        if started.elapsed() > WELCOME_TIMEOUT {
            let _ = ctl.save_screenshot(&artifacts.join("01-anaconda-welcome-failure.png"));
            bail!("Anaconda's welcome screen did not appear within {WELCOME_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_secs(3));
    };
    let path = artifacts.join("01-anaconda-welcome.png");
    ctl.save_screenshot(&path)?;
    println!(
        "     screenshot {} ({}x{})",
        path.display(),
        frame.width,
        frame.height
    );
    Ok(())
}

/// Is the most common colour of the picture light (Anaconda's window)?
fn is_light(frame: &Frame) -> bool {
    use std::collections::HashMap;
    let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
    for px in frame.rgb.as_chunks::<3>().0.iter().step_by(97) {
        *counts.entry(*px).or_default() += 1;
    }
    let Some((colour, n)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
        return false;
    };
    let total = (frame.rgb.len() / 3).div_ceil(97) as u32;
    let luma = (u32::from(colour[0]) * 3 + u32::from(colour[1]) * 6 + u32::from(colour[2])) / 10;
    luma > 200 && n * 5 > total
}

// ---------------------------------------------------------------------------
// 2. Unattended install
// ---------------------------------------------------------------------------

/// Run the installer to completion. Returns whether the disk is installed.
fn unattended_install(
    ws: &Workspace,
    iso: &Path,
    disk: &Path,
    vars: &Path,
    work: &Path,
    artifacts: &Path,
    report: &mut Report,
) -> bool {
    let install = || -> Result<()> {
        blank_disk(disk)?;
        let oemdrv = kickstart_disk(work)?;
        let mut config = VmConfig::new("iso-install", disk);
        config.snapshot = false;
        config.memory = INSTALLER_MEMORY.into();
        config.vcpus = 4;
        config.cdrom = Some(iso.to_path_buf());
        config.extra_disks = vec![oemdrv];
        config.efi_vars = Some(vars.to_path_buf());
        let mut vm = Vm::boot(ws, &config)?;
        let ctl = Vmctl::from_info(vm.info.clone());
        let serial = vm.info.dir.join("serial.log");

        let started = Instant::now();
        let mut last_shot = started;
        let status = loop {
            if let Some(status) = vm.exited()? {
                break status;
            }
            if last_shot.elapsed() > Duration::from_secs(20) {
                // The last picture with something on it is the evidence if
                // the install fails (the machine goes black as it powers off).
                if let Ok(png) = ctl.screenshot_png()
                    && Frame::decode(&png).is_ok_and(|f| !f.is_blank())
                {
                    let _ = fs::write(artifacts.join("02-installing.png"), png);
                }
                last_shot = Instant::now();
            }
            if started.elapsed() > INSTALL_TIMEOUT {
                let _ = ctl.save_screenshot(&artifacts.join("02-install-timeout.png"));
                bail!(
                    "the install did not finish within {INSTALL_TIMEOUT:?} (serial log: {})",
                    serial.display()
                );
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        println!(
            "     installer finished after {:?} with {status}",
            started.elapsed()
        );
        let console = fs::read_to_string(&serial).unwrap_or_default();
        let _ = fs::write(artifacts.join("installer-serial.txt"), &console);
        if !status.success() {
            bail!("the installer VM exited with {status}");
        }
        // A failed install also powers the machine off (`poweroff`); only the
        // `%post` of a finished one prints this.
        if !console.contains(INSTALL_DONE) {
            bail!(
                "the installer powered off without finishing the install (no {INSTALL_DONE} on the serial console; see 02-installing.png)"
            );
        }
        // The kickstart's `%pre` printed Anaconda's own idea of the product.
        let product = console
            .lines()
            .find_map(|l| {
                l.split_once("ILEX-PRODUCT ")
                    .map(|(_, rest)| rest.trim().to_string())
            })
            .context("the installer printed no ILEX-PRODUCT line on the serial console")?;
        println!("     installer product: {product}");
        if !(product.contains("is_final_release=True") && product.contains("name='ilex'")) {
            bail!("the installer does not identify as a final ilex release: {product}");
        }
        Ok(())
    };
    let result = install();
    let ok = result.is_ok();
    report.check("unattended install from the ISO", result);
    ok
}

/// A sparse blank qcow2 to install onto.
fn blank_disk(path: &Path) -> Result<()> {
    cmd::require("qemu-img")?;
    let _ = fs::remove_file(path);
    cmd::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(path)
            .arg(DISK_SIZE),
    )
}

/// A small disk labelled `OEMDRV` holding `ks.cfg`.
fn kickstart_disk(work: &Path) -> Result<PathBuf> {
    let dir = work.join("oemdrv");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("ks.cfg"), KICKSTART)?;
    let image = work.join("oemdrv.img");
    let _ = fs::remove_file(&image);
    fs::File::create(&image)?.set_len(16 << 20)?;
    cmd::require("mkfs.ext4")?;
    cmd::run(
        Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "OEMDRV", "-d"])
            .arg(&dir)
            .arg(&image),
    )?;
    Ok(image)
}

// ---------------------------------------------------------------------------
// 3. The installed machine
// ---------------------------------------------------------------------------

fn installed_system(
    ws: &Workspace,
    disk: &Path,
    vars: &Path,
    artifacts: &Path,
    report: &mut Report,
) -> Result<()> {
    // Same firmware variables as the installer: the boot entry it wrote is
    // there. A throwaway overlay keeps the installed disk pristine.
    let mut config = VmConfig::new("iso", disk);
    config.efi_vars = Some(vars.to_path_buf());
    let mut vm = match vmtest::boot(ws, config) {
        Ok(vm) => vm,
        Err(e) => {
            report.check("installed system boots and accepts SSH", Err(e));
            return Ok(());
        }
    };
    let ctl = Vmctl::from_info(vm.info.clone());
    report.check("installed system boots and accepts SSH", Ok(()));

    report.check(
        "bootc tracks the target image",
        bootc_status(&vm, artifacts),
    );
    report.check("system reaches `running`", system_running(&vm));
    report.check("no failed units", no_failed_units(&vm));
    report.check("GDM is up", gdm(&mut vm, &ctl, artifacts));
    if report.has_failures() {
        let _ = ctl.save_screenshot(&artifacts.join("failure.png"));
        if let Ok(journal) = vm.ssh("journalctl -b --no-pager -n 500 -o short-monotonic") {
            let _ = qemu::write_artifact(
                artifacts,
                "failure-journal.txt",
                &String::from_utf8_lossy(&journal.stdout),
            );
        }
    }
    Ok(())
}

/// `bootc status` names a booted image that is the target, and (when the
/// harness knows it) the digest the ISO embedded.
fn bootc_status(vm: &Vm, artifacts: &Path) -> Result<()> {
    let json = vm.ssh_ok("bootc status --json")?;
    let _ = qemu::write_artifact(artifacts, "bootc-status.json", &json);
    let _ = vm.ssh("bootc status").map(|o| {
        qemu::write_artifact(
            artifacts,
            "bootc-status.txt",
            &String::from_utf8_lossy(&o.stdout),
        )
    });
    let status: serde_json::Value = serde_json::from_str(&json).context("parsing bootc status")?;
    let booted = &status["status"]["booted"];
    if booted.is_null() {
        bail!("bootc status shows no booted deployment: {json}");
    }
    let image = &booted["image"];
    let name = image["image"]["image"].as_str().unwrap_or("?");
    let transport = image["image"]["transport"].as_str().unwrap_or("?");
    let digest = image["imageDigest"].as_str().unwrap_or("?");
    println!("     booted image: {transport}:{name} ({digest})");
    println!("     spec.image: {}", status["spec"]["image"]);
    let wanted = target();
    if name != wanted {
        bail!("the machine tracks {name}, expected {wanted}");
    }
    if let Some(expected) = std::env::var("ILEX_ISO_DIGEST")
        .ok()
        .filter(|d| !d.is_empty())
        && digest != expected
    {
        bail!("the machine booted {digest}, but the ISO embeds {expected}");
    }
    Ok(())
}

fn system_running(vm: &Vm) -> Result<()> {
    let out = vm.ssh("timeout 300 systemctl is-system-running --wait")?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state == "running" {
        Ok(())
    } else {
        bail!("system state is `{state}`")
    }
}

fn no_failed_units(vm: &Vm) -> Result<()> {
    let failed = vm.ssh_ok("systemctl --failed --no-legend --plain")?;
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("failed units:\n{failed}")
    }
}

/// GDM is active with a greeter session, and its login screen is drawn.
fn gdm(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(300);
    vm.wait_for(
        "gdm.service to be active",
        "systemctl is-active gdm",
        TIMEOUT,
    )?;
    vm.wait_for(
        "a GDM greeter session",
        "loginctl list-sessions --no-legend | grep -w greeter",
        TIMEOUT,
    )?;
    let cond = WaitCond {
        nonblank: true,
        size: Some(INSTALLER_SCREEN),
        stable: Some(Duration::from_secs(5)),
        ..WaitCond::default()
    };
    ctl.wait_screen(&cond, TIMEOUT)
        .context("waiting for GDM's login screen")?;
    let path = artifacts.join("03-gdm.png");
    let frame = ctl.save_screenshot(&path)?;
    println!(
        "     screenshot {} ({}x{})",
        path.display(),
        frame.width,
        frame.height
    );
    Ok(())
}

fn clear_artifacts(artifacts: &Path) {
    let Ok(entries) = fs::read_dir(artifacts) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension()
            .is_some_and(|x| x == "png" || x == "txt" || x == "json")
        {
            let _ = fs::remove_file(p);
        }
    }
}
