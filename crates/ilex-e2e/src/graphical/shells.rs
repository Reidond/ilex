//! `graphical::shells`: one image, GNOME and KDE, one home.
//!
//! The image has both shells with GNOME as the default, so GDM is the greeter
//! and offers Plasma as a session. The test account logs into Plasma first,
//! with everything the KDE shell does to a home turned up (Breeze GTK theme,
//! dark scheme), then into GNOME with the same home. GNOME must still look
//! like GNOME: its GSettings are untouched and a GTK 3 window is Adwaita.
//!
//! Select it explicitly (`ilexctl test graphical::shells --shell gnome --shell kde`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::Vmctl;
use libtest_mimic::{Failed, Trial};

use super::kde::{self, user};
use super::{
    SESSION_TIMEOUT, USER, clear_artifacts, create_user, no_failed_user_units, session_active, settle, shot,
    test_password, wait_greeter,
};
use crate::common;
use crate::disk::vmtest;

pub fn trial() -> Trial {
    Trial::test("graphical::shells", || run().map_err(|e| Failed::from(format!("{e:#}"))))
}

fn run() -> Result<()> {
    let selection = common::selection()?;
    if selection.default_shell.as_deref() != Some("gnome") || !selection.shells.iter().any(|s| s == "kde") {
        bail!(
            "graphical::shells needs shells gnome (default) and kde, got {:?}",
            selection.shells
        );
    }
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "graphical-shells")?;
    clear_artifacts(&artifacts);
    let disk = vmtest::disk(&ws)?;
    let mut vm = vmtest::boot(&ws, VmConfig::graphical("graphical-shells", disk))?;
    let ctl = Vmctl::from_info(vm.info.clone());
    let result = session(&mut vm, &ctl, &artifacts);
    if result.is_err() {
        let _ = ctl.save_screenshot(&artifacts.join("failure.png"));
        if let Ok(journal) = vm.ssh("journalctl -b --no-pager -n 500 -o short-monotonic") {
            let _ = qemu::write_artifact(&artifacts, "failure-journal.txt", &String::from_utf8_lossy(&journal.stdout));
        }
        println!("artifacts: {}", artifacts.display());
    }
    result
}

fn session(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    wait_greeter(vm)?;
    settle(ctl)?;
    shot(ctl, artifacts, "01-first-boot")?;

    // The account, with GNOME's initial setup marked done, Plasma's idle
    // handling off and Plasma as the session GDM starts.
    let password = test_password();
    create_user(vm, &password)?;
    kde::seed_plasma_config(vm)?;
    println!("ok   created test user {USER}");
    // GDM's greeter lists AccountsService users, so the user is created first,
    // the session chosen through AccountsService, and the guest restarted.
    choose_session(vm, "plasma")?;
    vm.reboot(vmtest::BOOT_TIMEOUT).context("rebooting with the test user in place")?;
    wait_greeter(vm)?;
    settle(ctl)?;
    shot(ctl, artifacts, "02-gdm-login")?;

    // 1. Plasma, started by GDM.
    login(vm, ctl, &password)?;
    vm.wait_for(
        "the Plasma shell in the user session",
        &format!("pgrep -u {USER} -x plasmashell"),
        SESSION_TIMEOUT,
    )?;
    settle(ctl)?;
    shot(ctl, artifacts, "03-plasma-from-gdm")?;
    let mut report = vmtest::Report::default();
    report.check("GDM started a Plasma Wayland session", session_active(vm));
    report.finish()?;
    kde::theming(vm, ctl, artifacts)?;
    // Plasma writes the GTK preference of its last colour scheme into the home's
    // gtk-3.0/gtk-4.0 settings.ini, which every desktop reads: a dark scheme
    // left behind makes GNOME's non-libadwaita GTK applications dark (CLAUDE.md,
    // "KDE shell"). This test leaves Plasma light, as most sessions end.
    kde::light(vm)?;

    // 2. The same home in GNOME.
    vm.ssh_ok(&format!("loginctl terminate-user {USER}"))?;
    wait_greeter(vm)?;
    choose_session(vm, "gnome")?;
    settle(ctl)?;
    login(vm, ctl, &password)?;
    vm.wait_for(
        "gnome-shell in the user session",
        &format!("pgrep -u {USER} -x gnome-shell"),
        SESSION_TIMEOUT,
    )?;
    settle(ctl)?;
    // First-login noise (welcome tour, updater prompt): clear what is there.
    ctl.key("Escape")?;
    std::thread::sleep(Duration::from_secs(2));
    ctl.key("Escape")?;
    settle(ctl)?;
    shot(ctl, artifacts, "07-gnome-same-home")?;
    gnome_unchanged(vm, ctl, artifacts)
}

/// GDM picks the user's session from AccountsService.
fn choose_session(vm: &Vm, session: &str) -> Result<()> {
    vm.ssh_ok(&format!(
        "set -e; path=$(busctl --system call org.freedesktop.Accounts /org/freedesktop/Accounts \
           org.freedesktop.Accounts FindUserByName s {USER} | awk '{{print $2}}' | tr -d '\"'); \
         busctl --system call org.freedesktop.Accounts \"$path\" org.freedesktop.Accounts.User SetSession s {session}; \
         busctl --system call org.freedesktop.Accounts \"$path\" org.freedesktop.Accounts.User SetSessionType s wayland"
    ))?;
    Ok(())
}

/// The user tile is focused: Return opens its password prompt.
fn login(vm: &mut Vm, ctl: &Vmctl, password: &str) -> Result<()> {
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(3));
    ctl.type_text(password)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    vm.wait_for(
        "a graphical session for the test user",
        &format!("loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\"' | grep ."),
        SESSION_TIMEOUT,
    )?;
    Ok(())
}

/// What Plasma left in the home does not restyle GNOME.
fn gnome_unchanged(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    let mut report = vmtest::Report::default();
    report.check("the session is GNOME", {
        let env = user(vm, "systemctl --user show-environment")?;
        if env.lines().any(|l| l == "XDG_CURRENT_DESKTOP=GNOME") && !env.lines().any(|l| l.starts_with("DCONF_PROFILE=")) {
            Ok(())
        } else {
            Err(anyhow!("session environment:\n{env}"))
        }
    });
    report.check("GNOME's own dconf database holds no interface setting", {
        let keys = user(vm, "dconf dump /org/gnome/desktop/interface/")?;
        if keys.trim().is_empty() { Ok(()) } else { Err(anyhow!("Plasma wrote into GNOME's database:\n{keys}")) }
    });
    report.check("GNOME keeps its GTK, icon and cursor themes and its colour scheme", {
        let mut wrong = Vec::new();
        for (key, want) in [
            ("gtk-theme", "'Adwaita'"),
            ("icon-theme", "'Adwaita'"),
            ("cursor-theme", "'Adwaita'"),
            ("color-scheme", "'default'"),
        ] {
            let got = user(vm, &format!("gsettings get org.gnome.desktop.interface {key}"))?;
            if got != want {
                wrong.push(format!("{key}: {got}, expected {want}"));
            }
        }
        if wrong.is_empty() { Ok(()) } else { Err(anyhow!("{}", wrong.join("; "))) }
    });
    report.check("no user units failed", no_failed_user_units(vm));
    report.finish()?;
    // The GTK 3 window reads the home's ~/.config/gtk-3.0 too.
    kde::gtk3_window(vm, ctl, artifacts, "08-gtk3-in-gnome", &["theme=Adwaita", "icons=Adwaita", "dark=False"])
}
