//! `graphical::` tier: A 4K virtio-vga VM driven over QMP; assertions over SSH.
//!
//! The plan's four steps, on one VM (headless, 3840x2160):
//!
//! 1. boot the qcow2 from `bcvk to-disk`;
//! 2. wait for `gdm` over SSH, then until the screen is not blank;
//! 3. create a test user over SSH (no users are baked into the image), log in
//!    by typing into the greeter through QMP, then check the session over
//!    SSH: `loginctl` shows it and no user units failed;
//! 4. save a screenshot per step as a test artifact
//!    (`<state dir>/artifacts/graphical/`, or `$ILEX_ARTIFACTS/graphical/`).
//!
//! A fresh install has no user, so GDM's first screen is GNOME Initial Setup
//! (screenshot 01). GDM only offers the login screen when a user exists at
//! the time it starts, and restarting it under a running greeter leaves the
//! display dead, so the test creates the user and reboots the guest once.
//!
//! Assertions come from SSH, never from pixel diffs: font and antialiasing
//! changes between Fedora updates would make those flaky. Screenshots are
//! evidence for review.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::{Frame, Vmctl, WaitCond};
use libtest_mimic::{Failed, Trial};

use crate::common;
use crate::disk::vmtest;

const USER: &str = "ilextest";
/// Software-rendered 4K is slow: give each wait generously.
const SESSION_TIMEOUT: Duration = Duration::from_secs(240);

pub fn trials() -> Vec<Trial> {
    vec![Trial::test("graphical::login", || run().map_err(|e| Failed::from(format!("{e:#}"))))]
}

fn run() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "graphical")?;
    clear_artifacts(&artifacts);

    // 1. Boot the installed disk.
    let disk = vmtest::disk(&ws)?;
    let mut vm = vmtest::boot(&ws, VmConfig::graphical("graphical", disk))?;
    let ctl = Vmctl::from_info(vm.info.clone());

    let result = session(&mut vm, &ctl, &artifacts);
    if result.is_err() {
        // Evidence for whoever reads the failure.
        let _ = ctl.save_screenshot(&artifacts.join("failure.png"));
        if let Ok(journal) = vm.ssh("journalctl -b --no-pager -n 500 -o short-monotonic") {
            let _ = qemu::write_artifact(
                &artifacts,
                "failure-journal.txt",
                &String::from_utf8_lossy(&journal.stdout),
            );
        }
        println!("artifacts: {}", artifacts.display());
    }
    result
}

fn session(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    // 2. GDM is up and has drawn something. On a fresh install that is GNOME
    // Initial Setup, run by GDM because there is no user yet.
    wait_gdm(vm)?;
    let frame = settle(ctl)?;
    println!("ok   gdm is active and the screen is not blank ({}x{})", frame.width, frame.height);
    if (frame.width, frame.height) != (3840, 2160) {
        bail!("expected a 3840x2160 framebuffer, got {}x{}", frame.width, frame.height);
    }
    shot(ctl, artifacts, "01-first-boot")?;

    // 3. A test user, created over SSH for this boot only; reboot so GDM
    // starts with a user and shows the real login screen.
    let password = test_password();
    create_user(vm, &password)?;
    println!("ok   created test user {USER}");
    vm.reboot(vmtest::BOOT_TIMEOUT).context("rebooting with the test user in place")?;
    wait_gdm(vm)?;
    settle(ctl)?;
    let sessions = vm.ssh_ok("loginctl list-sessions --no-legend")?;
    if sessions.contains("gnome-initial-setup") {
        bail!("GDM still shows initial setup although {USER} exists:\n{sessions}");
    }
    println!("ok   GDM shows the login screen, not initial setup");
    shot(ctl, artifacts, "02-gdm-login")?;

    // The user tile is focused: Return opens its password prompt.
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(3));
    ctl.type_text(&password)?;
    std::thread::sleep(Duration::from_secs(1));
    shot(ctl, artifacts, "03-password")?;
    ctl.key("Return")?;

    vm.wait_for(
        "a graphical session for the test user",
        &format!("loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\"' | grep ."),
        SESSION_TIMEOUT,
    )?;
    vm.wait_for(
        "gnome-shell in the user session",
        &format!("pgrep -u {USER} -x gnome-shell"),
        SESSION_TIMEOUT,
    )?;
    settle(ctl)?;
    shot(ctl, artifacts, "04-session")?;

    // New accounts land in the overview behind a welcome dialog (and
    // possibly a polkit prompt from the software updater); clear them for
    // the last picture. Best effort: nothing asserts on these keys.
    ctl.key("Escape")?;
    std::thread::sleep(Duration::from_secs(2));
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(2));
    ctl.key("Escape")?;
    settle(ctl)?;
    shot(ctl, artifacts, "05-desktop")?;

    // Assertions over SSH.
    let mut report = vmtest::Report::default();
    report.check("loginctl shows an active Wayland session", session_active(vm));
    report.check("no user units failed", no_failed_user_units(vm));
    report.check("no system units failed", no_failed_system_units(vm));
    report.finish()
}

/// `gdm` is active and a greeter-class session (login screen or initial
/// setup) exists.
fn wait_gdm(vm: &mut Vm) -> Result<()> {
    vm.wait_for("gdm.service to be active", "systemctl is-active gdm", SESSION_TIMEOUT)?;
    vm.wait_for(
        "a GDM greeter session",
        "loginctl list-sessions --no-legend | grep -w greeter",
        SESSION_TIMEOUT,
    )?;
    Ok(())
}

/// Wait until the display shows something and has stopped changing.
fn settle(ctl: &Vmctl) -> Result<Frame> {
    let cond = WaitCond {
        nonblank: true,
        stable: Some(Duration::from_secs(5)),
        ..WaitCond::default()
    };
    let (_, frame) = ctl.wait_screen(&cond, SESSION_TIMEOUT).context("waiting for the screen to settle")?;
    Ok(frame)
}

/// Save `<name>.png` under `artifacts` and say so.
fn shot(ctl: &Vmctl, artifacts: &Path, name: &str) -> Result<()> {
    let path = artifacts.join(format!("{name}.png"));
    let frame = ctl.save_screenshot(&path)?;
    println!("     screenshot {} ({}x{})", path.display(), frame.width, frame.height);
    Ok(())
}

fn clear_artifacts(artifacts: &Path) {
    let Ok(entries) = std::fs::read_dir(artifacts) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "png" || x == "txt") {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// A throwaway password for this run only.
fn test_password() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ilex-{:x}", nanos & 0xffff_ffff_ffff)
}

/// The test account, with GNOME's first-login wizard marked done.
fn create_user(vm: &Vm, password: &str) -> Result<()> {
    vm.ssh_ok(&format!(
        "useradd --create-home --shell /bin/bash {USER} && \
         echo '{USER}:{password}' | chpasswd && \
         install -d -o {USER} -g {USER} -m 0700 /var/home/{USER}/.config && \
         echo yes > /var/home/{USER}/.config/gnome-initial-setup-done && \
         chown {USER}:{USER} /var/home/{USER}/.config/gnome-initial-setup-done"
    ))?;
    Ok(())
}

fn session_active(vm: &Vm) -> Result<()> {
    let id = vm.ssh_ok(&format!(
        "loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\" {{print $1; exit}}'"
    ))?;
    if id.is_empty() {
        bail!("loginctl shows no user session for {USER}");
    }
    let props = vm.ssh_ok(&format!("loginctl show-session {id} -p Type -p Class -p State -p Active -p Name"))?;
    println!("     session {id}: {}", props.replace('\n', ", "));
    for want in ["Type=wayland", "Class=user", "State=active", "Active=yes", &format!("Name={USER}")] {
        if !props.lines().any(|l| l == want) {
            bail!("session {id} lacks {want}:\n{props}");
        }
    }
    Ok(())
}

fn no_failed_user_units(vm: &Vm) -> Result<()> {
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    let failed = vm.ssh_ok(&format!(
        "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/{uid} \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus \
         systemctl --user --failed --no-legend --plain"
    ))?;
    if failed.is_empty() { Ok(()) } else { bail!("failed user units:\n{failed}") }
}

fn no_failed_system_units(vm: &Vm) -> Result<()> {
    let failed = vm.ssh_ok("systemctl --failed --no-legend --plain")?;
    if failed.is_empty() { Ok(()) } else { bail!("failed units:\n{failed}") }
}
