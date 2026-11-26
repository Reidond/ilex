//! `graphical::shells`: one image, GNOME and KDE, one home, one user manager.
//!
//! The image has both shells with GNOME as the default, so GDM is the greeter
//! and offers Plasma as a session. The test account (lingering, so one user
//! manager outlives every session, as it does with an SSH or TTY login) goes
//! through four sessions, each ended with the desktop's own logout:
//!
//! 1. Plasma, left in Breeze Dark with everything the KDE shell does to a home
//!    turned up (Breeze GTK theme, dark scheme);
//! 2. GNOME, which must still look like GNOME: GTK 3, GTK 4 and libadwaita
//!    applications are light and Adwaita, GNOME's database is untouched, and
//!    Plasma left nothing in the home or the manager that GNOME reads;
//! 3. GNOME made dark, with Adwaita-dark and a purple accent;
//! 4. Plasma again, which must be Breeze Dark with Plasma's accent, not
//!    GNOME's choices, while GNOME's own settings stay as GNOME left them.
//!
//! Select it explicitly (`ilexctl test graphical::shells --shell gnome --shell kde`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::Vmctl;
use libtest_mimic::{Failed, Trial};

use super::kde::{self, probe, show, user};
use super::{
    SESSION_TIMEOUT, USER, clear_artifacts, create_user, no_failed_user_units, session_active,
    settle, shot, test_password, wait_greeter,
};
use crate::common;
use crate::disk::vmtest;

pub fn trial() -> Trial {
    Trial::test("graphical::shells", || {
        run().map_err(|e| Failed::from(format!("{e:#}")))
    })
}

fn run() -> Result<()> {
    let selection = common::selection()?;
    if selection.default_shell.as_deref() != Some("gnome")
        || !selection.shells.iter().any(|s| s == "kde")
    {
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
    wait_greeter(vm)?;
    settle(ctl)?;
    shot(ctl, artifacts, "01-first-boot")?;

    // The account, with GNOME's initial setup marked done, Plasma's idle
    // handling off and a user manager that outlives its sessions.
    let password = test_password();
    create_user(vm, &password)?;
    kde::seed_plasma_config(vm, &format!("/var/home/{USER}"))?;
    vm.ssh_ok(&format!("loginctl enable-linger {USER}"))?;
    println!("ok   created test user {USER} (lingering)");
    // GDM's greeter lists AccountsService users, so the user is created first,
    // the session chosen through AccountsService, and the guest restarted.
    choose_session(vm, "plasma")?;
    vm.reboot(vmtest::BOOT_TIMEOUT)
        .context("rebooting with the test user in place")?;
    wait_greeter(vm)?;
    settle(ctl)?;
    shot(ctl, artifacts, "02-gdm-login")?;

    // 1. Plasma, started by GDM, ending in Breeze Dark.
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
    // The host's GTK 4 and libadwaita applications (GNOME's, in this image).
    toolkits(vm, ToolkitLook::Plasma { dark: true })?;
    show(vm, ctl, artifacts, "10-plasma-libadwaita-dark", "adw", None)?;
    let leftovers = user(vm, "dconf dump /org/gnome/desktop/interface/")?;
    println!("     Plasma ends dark; the user's own database holds: {}", leftovers.trim());

    // 2. A normal logout, then the same home in GNOME.
    kde::logout(vm)?;
    let mut report = vmtest::Report::default();
    report.check("a Plasma logout leaves nothing in the user manager", manager_clean(vm));
    report.check("and nothing in the home that GNOME reads", kde::nothing_shared(vm));
    report.finish()?;
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
    shot(ctl, artifacts, "11-gnome-same-home")?;
    gnome_unchanged(vm, ctl, artifacts)?;

    // 3. GNOME's own choices: dark, Adwaita-dark, a purple accent.
    gnome_dark(vm)?;
    toolkits(vm, ToolkitLook::GnomeDark)?;
    show(vm, ctl, artifacts, "13-gnome-gtk3-dark", "gtk3", None)?;
    gnome_logout(vm)?;

    // 4. Plasma again: still Breeze Dark with Plasma's accent, GNOME's
    // choices unreached; and GNOME's own database as GNOME left it.
    choose_session(vm, "plasma")?;
    settle(ctl)?;
    login(vm, ctl, &password)?;
    vm.wait_for(
        "the Plasma shell in the user session",
        &format!("pgrep -u {USER} -x plasmashell"),
        SESSION_TIMEOUT,
    )?;
    settle(ctl)?;
    shot(ctl, artifacts, "14-plasma-after-gnome")?;
    plasma_unchanged(vm)?;
    toolkits(vm, ToolkitLook::Plasma { dark: true })?;
    show(vm, ctl, artifacts, "15-plasma-gtk3-after-gnome", "gtk3", None)?
        .expect(Some("Breeze-Dark"), Some("breeze"), true)?;
    Ok(())
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
        &format!(
            "loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\"' | grep ."
        ),
        SESSION_TIMEOUT,
    )?;
    Ok(())
}

/// GNOME's own logout, with no confirmation dialog, then the greeter.
fn gnome_logout(vm: &mut Vm) -> Result<()> {
    user(
        vm,
        "busctl --user call org.gnome.SessionManager /org/gnome/SessionManager org.gnome.SessionManager Logout u 1",
    )?;
    vm.wait_for(
        "the GNOME session to end",
        &format!("! loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\"' | grep -q ."),
        SESSION_TIMEOUT,
    )?;
    wait_greeter(vm)
}

/// After Plasma's logout the lingering user manager holds no trace of Plasma:
/// its target is down and the environment it exported is gone.
fn manager_clean(vm: &Vm) -> Result<()> {
    let env = user(vm, "systemctl --user show-environment")?;
    let plasma = user(vm, "systemctl --user is-active plasma-workspace.target")
        .unwrap_or_else(|_| "inactive".into());
    let stale: Vec<_> = env
        .lines()
        .filter(|l| {
            ["GTK_USE_PORTAL=", "DCONF_PROFILE=", "GTK_THEME="]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .collect();
    if !stale.is_empty() || plasma == "active" {
        bail!("the user manager still holds {stale:?}, plasma-workspace.target {plasma}");
    }
    Ok(())
}

/// What a toolkit session is expected to look like.
enum ToolkitLook {
    /// Plasma's: Breeze for GTK 3, libadwaita with Plasma's blue accent.
    Plasma { dark: bool },
    /// GNOME as `gnome_dark` leaves it: Adwaita-dark, a purple accent.
    GnomeDark,
}

/// The host's GTK 3, GTK 4 and libadwaita applications resolve what `look` says.
fn toolkits(vm: &Vm, look: ToolkitLook) -> Result<()> {
    let mut report = vmtest::Report::default();
    match look {
        ToolkitLook::Plasma { dark } => {
            let theme = if dark { "Breeze-Dark" } else { "Breeze" };
            report.check("GTK 3 is Breeze", probe(vm, "gtk3", None).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(Some(theme), Some("breeze"), dark)
            }));
            report.check("GTK 4 follows the scheme", probe(vm, "gtk4", None).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(None, None, dark)
            }));
            report.check("libadwaita follows the scheme with Plasma's accent", probe(vm, "adw", None).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(None, None, dark)?;
                if p.field("accent") != Some("blue") {
                    bail!("libadwaita's accent is not Plasma's blue: {}", p.describe());
                }
                Ok(())
            }));
        }
        ToolkitLook::GnomeDark => {
            report.check("GTK 3 is Adwaita-dark", probe(vm, "gtk3", None).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(Some("Adwaita-dark"), Some("Adwaita"), true)
            }));
            report.check("GTK 4 is dark", probe(vm, "gtk4", None).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(None, None, true)
            }));
            report.check("libadwaita is dark with GNOME's purple accent", probe(vm, "adw", None).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(None, None, true)?;
                if p.field("accent") != Some("purple") {
                    bail!("libadwaita's accent is not purple: {}", p.describe());
                }
                Ok(())
            }));
        }
    }
    report.finish()
}

/// What Plasma left in the home does not restyle GNOME.
fn gnome_unchanged(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    let mut report = vmtest::Report::default();
    report.check("the session is GNOME and carries nothing of Plasma's", {
        let env = user(vm, "systemctl --user show-environment")?;
        if env.lines().any(|l| l == "XDG_CURRENT_DESKTOP=GNOME")
            && !env.lines().any(|l| {
                ["DCONF_PROFILE=", "GTK_USE_PORTAL=", "GTK_THEME="].iter().any(|p| l.starts_with(p))
            })
        {
            Ok(())
        } else {
            Err(anyhow!("session environment:\n{env}"))
        }
    });
    report.check("GNOME's own dconf database holds no interface setting", {
        let keys = user(vm, "dconf dump /org/gnome/desktop/interface/")?;
        if keys.trim().is_empty() {
            Ok(())
        } else {
            Err(anyhow!("Plasma wrote into GNOME's database:\n{keys}"))
        }
    });
    report.check(
        "GNOME keeps its GTK, icon and cursor themes and its colour scheme",
        {
            let mut wrong = Vec::new();
            for (key, want) in [
                ("gtk-theme", "'Adwaita'"),
                ("icon-theme", "'Adwaita'"),
                ("cursor-theme", "'Adwaita'"),
                ("color-scheme", "'default'"),
            ] {
                let got = user(
                    vm,
                    &format!("gsettings get org.gnome.desktop.interface {key}"),
                )?;
                if got != want {
                    wrong.push(format!("{key}: {got}, expected {want}"));
                }
            }
            if wrong.is_empty() {
                Ok(())
            } else {
                Err(anyhow!("{}", wrong.join("; ")))
            }
        },
    );
    report.check("no GTK settings file in the home", kde::nothing_shared(vm));
    report.check("no user units failed", no_failed_user_units(vm));
    report.finish()?;
    // GNOME's GTK 3, GTK 4 and libadwaita applications: light and Adwaita,
    // after a Plasma session that ended in Breeze Dark.
    let mut report = vmtest::Report::default();
    report.check("GTK 3 is Adwaita and light", probe(vm, "gtk3", None).and_then(|p| {
        println!("     {}", p.describe());
        p.expect(Some("Adwaita"), Some("Adwaita"), false)
    }));
    report.check("GTK 4 is light", probe(vm, "gtk4", None).and_then(|p| {
        println!("     {}", p.describe());
        p.expect(None, None, false)
    }));
    report.check("libadwaita is light with GNOME's blue accent", probe(vm, "adw", None).and_then(|p| {
        println!("     {}", p.describe());
        p.expect(None, None, false)?;
        if p.field("accent") != Some("blue") || p.field("adw_dark") != Some("False") {
            bail!("libadwaita is not GNOME's light blue: {}", p.describe());
        }
        Ok(())
    }));
    report.finish()?;
    show(vm, ctl, artifacts, "12-gtk3-in-gnome", "gtk3", None)?
        .expect(Some("Adwaita"), Some("Adwaita"), false)
}

/// GNOME's choices: the dark scheme, Adwaita-dark for GTK 3 applications and a
/// purple accent. Plasma could only show them by reading GNOME's database.
fn gnome_dark(vm: &Vm) -> Result<()> {
    for (key, value) in [
        ("color-scheme", "prefer-dark"),
        ("gtk-theme", "Adwaita-dark"),
        ("accent-color", "purple"),
    ] {
        user(vm, &format!("gsettings set org.gnome.desktop.interface {key} {value}"))?;
    }
    // GNOME's portal answers from the new values.
    std::thread::sleep(Duration::from_secs(3));
    Ok(())
}

/// Plasma, after GNOME's dark session, is Plasma's.
fn plasma_unchanged(vm: &mut Vm) -> Result<()> {
    let mut report = vmtest::Report::default();
    report.check("Plasma's session carries GTK_USE_PORTAL and no profile or theme", {
        let env = user(vm, "systemctl --user show-environment")?;
        let has = |p: &str| env.lines().any(|l| l.starts_with(p));
        if env.lines().any(|l| l == "XDG_CURRENT_DESKTOP=KDE")
            && has("GTK_USE_PORTAL=1")
            && !has("DCONF_PROFILE=")
            && !has("GTK_THEME=")
        {
            Ok(())
        } else {
            Err(anyhow!("session environment:\n{env}"))
        }
    });
    report.check("the KDE database still holds Breeze Dark", {
        let keys = user(vm, "env DCONF_PROFILE=ilex-kde dconf dump /org/gnome/desktop/interface/")?;
        if keys.contains("gtk-theme='Breeze-Dark'") && keys.contains("icon-theme='breeze'") {
            Ok(())
        } else {
            Err(anyhow!("KDE keys:\n{keys}"))
        }
    });
    report.check("GNOME's own database is as GNOME left it", {
        let keys = user(vm, "dconf dump /org/gnome/desktop/interface/")?;
        if keys.contains("gtk-theme='Adwaita-dark'") && keys.contains("accent-color='purple'") {
            Ok(())
        } else {
            Err(anyhow!("GNOME's keys:\n{keys}"))
        }
    });
    report.check("the GTK portal reads the KDE database again", {
        let profile = vm.ssh_ok(&format!(
            "for pid in $(pgrep -u {USER} -f '^/usr/libexec/xdg-desktop-portal-gtk'); do \
               tr '\\0' '\\n' < /proc/$pid/environ | grep '^DCONF_PROFILE='; done"
        ))?;
        if profile.trim() == "DCONF_PROFILE=ilex-kde" { Ok(()) } else { Err(anyhow!("`{profile}`")) }
    });
    report.finish()
}
