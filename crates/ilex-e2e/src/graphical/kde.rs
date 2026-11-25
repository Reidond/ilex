//! The KDE branch of `graphical::`: Plasma Login Manager, a Plasma session and
//! the GTK look the shell ships.
//!
//! 1. wait for `plasmalogin` and its greeter; there is no user yet, so the
//!    greeter shows only the clock (screenshot 01);
//! 2. create a test user over SSH and reboot, so the greeter lists the user;
//!    a key press raises its password prompt, the password is typed in;
//! 3. check the Plasma session over SSH, then the GTK look: the KDE dconf
//!    database, Breeze for GTK 3, and the portal values GTK 4 and libadwaita
//!    applications read, in light and dark. A GTK 3 window (the host's
//!    python3-gobject) is screenshotted in both (screenshots 05 and 06).
//!
//! libadwaita is not installed on the host: a libadwaita application is a
//! Flatpak. `graphical::apps` installs real ones from Flathub (network) and
//! screenshots them next to a GTK 3 Flatpak, see [`apps`].

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ilex_devkit::qemu::{self, Vm};
use ilex_vmctl::Vmctl;

use super::{
    USER, SESSION_TIMEOUT, no_failed_system_units, no_failed_user_units, session_active, settle, shot,
    test_password, wait_greeter, wait_session,
};
use crate::disk::vmtest;

const GTK3_PROBE: &[u8] = include_bytes!("gtk3-probe.py");

/// The unit that keeps the probe window open.
const PROBE_UNIT: &str = "ilex-gtk3-probe";

pub(super) fn session(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path, apps: bool) -> Result<()> {
    // 1. The greeter is up and has drawn something: with no user, the clock.
    wait_greeter(vm)?;
    let frame = settle(ctl)?;
    println!("ok   plasmalogin is active and the screen settled at {}x{}", frame.width, frame.height);
    shot(ctl, artifacts, "01-first-boot")?;

    // 2. A test user, created for this boot; reboot so the greeter lists it.
    let password = test_password();
    create_user(vm, &password)?;
    println!("ok   created test user {USER}");
    vm.reboot(vmtest::BOOT_TIMEOUT).context("rebooting with the test user in place")?;
    wait_greeter(vm)?;
    settle(ctl)?;
    shot(ctl, artifacts, "02-greeter")?;
    // The greeter starts as a clock over the wallpaper: any key raises the
    // user's password prompt, which has the focus.
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(3));
    shot(ctl, artifacts, "03-password-prompt")?;
    ctl.type_text(&password)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    wait_session(vm)?;
    vm.wait_for(
        "the Plasma shell in the user session",
        &format!("pgrep -u {USER} -x plasmashell"),
        SESSION_TIMEOUT,
    )?;
    settle(ctl)?;
    shot(ctl, artifacts, "04-desktop")?;

    let mut report = vmtest::Report::default();
    report.check("loginctl shows an active Wayland session", session_active(vm));
    report.check("the session is Plasma", session_is_plasma(vm));
    report.check("no user units failed", no_failed_user_units(vm));
    report.check("no system units failed", no_failed_system_units(vm));
    report.finish()?;

    if apps { apps::checks(vm, ctl, artifacts) } else { theming(vm, ctl, artifacts) }
}

/// The test account, with Plasma's configuration seeded.
fn create_user(vm: &Vm, password: &str) -> Result<()> {
    vm.ssh_ok(&format!(
        "useradd --create-home --shell /bin/bash {USER} && echo '{USER}:{password}' | chpasswd"
    ))?;
    seed_plasma_config(vm)
}

/// Plasma's screen locker and power management are switched off in the
/// account's configuration, so a slow run is not interrupted by a locked or
/// blanked screen.
pub(super) fn seed_plasma_config(vm: &Vm) -> Result<()> {
    vm.ssh_ok(&format!(
        "set -e; home=/var/home/{USER}; install -d -o {USER} -g {USER} -m 0700 $home/.config; \
         printf '[Daemon]\\nAutolock=false\\nLockOnResume=false\\nTimeout=0\\n' > $home/.config/kscreenlockerrc; \
         printf '[AC][Display]\\nDimDisplayWhenIdle=false\\nTurnOffDisplayWhenIdle=false\\n\\n[AC][SuspendAndShutdown]\\nAutoSuspendAction=0\\n' > $home/.config/powerdevilrc; \
         chown {USER}:{USER} $home/.config/kscreenlockerrc $home/.config/powerdevilrc; \
         restorecon -R $home"
    ))?;
    Ok(())
}

fn session_is_plasma(vm: &Vm) -> Result<()> {
    let env = user(vm, "systemctl --user show-environment")?;
    for want in ["XDG_CURRENT_DESKTOP=KDE", "XDG_SESSION_TYPE=wayland"] {
        if !env.lines().any(|l| l == want) {
            bail!("the session environment lacks {want}:\n{env}");
        }
    }
    Ok(())
}

/// Run `cmd` as the test user, in their session: the user manager's
/// environment (Wayland socket, `DCONF_PROFILE`, ...) and bus.
pub(super) fn user(vm: &Vm, cmd: &str) -> Result<String> {
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    let env = vm.ssh_ok(&format!(
        "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/{uid} systemctl --user show-environment"
    ))?;
    let exports: String = env
        .lines()
        .filter(|l| {
            ["DCONF_PROFILE=", "WAYLAND_DISPLAY=", "XDG_CURRENT_DESKTOP=", "XDG_SESSION_TYPE=", "DBUS_SESSION_BUS_ADDRESS="]
                .iter()
                .any(|p| l.starts_with(p))
        })
        .map(|l| format!("{} ", qemu_quote(l)))
        .collect();
    vm.ssh_ok(&format!(
        "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/{uid} \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus {exports} {cmd}"
    ))
}

fn qemu_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The portal's `org.freedesktop.appearance` value, as `busctl` prints it.
fn appearance(vm: &Vm, key: &str) -> Result<String> {
    user(
        vm,
        &format!(
            "busctl --user call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
             org.freedesktop.portal.Settings Read ss org.freedesktop.appearance {key}"
        ),
    )
}

/// The GTK look of the Plasma session, light and dark.
pub(super) fn theming(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    let mut report = vmtest::Report::default();
    report.check("Plasma's session selects the KDE dconf database", {
        let env = user(vm, "systemctl --user show-environment")?;
        if env.lines().any(|l| l == "DCONF_PROFILE=ilex-kde") { Ok(()) } else { Err(anyhow!("DCONF_PROFILE is not ilex-kde:\n{env}")) }
    });
    report.check("kde-gtk-config chose the Breeze GTK theme, once", {
        vm.wait_for(
            "gtkTheme to be Breeze",
            &format!(
                "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/$(id -u {USER}) \
                 busctl --user call org.kde.kded6 /modules/gtkconfig org.kde.GtkConfig gtkTheme | grep -x 's \"Breeze\"'"
            ),
            Duration::from_secs(120),
        )
        .map(drop)
    });
    report.check("Plasma's GTK settings stay out of the shared dconf database", {
        // The GNOME `user` database holds none of what kde-gtk-config writes.
        let shared = user(vm, "env -u DCONF_PROFILE dconf dump /org/gnome/desktop/interface/")?;
        let kde = user(vm, "env DCONF_PROFILE=ilex-kde dconf dump /org/gnome/desktop/interface/")?;
        if kde.contains("gtk-theme='Breeze'") && kde.contains("icon-theme='breeze'") && shared.trim().is_empty() {
            Ok(())
        } else {
            Err(anyhow!("shared interface keys:\n{shared}\nKDE interface keys:\n{kde}"))
        }
    });
    report.check("the portal gives GTK 4 and libadwaita Plasma's light scheme and accent", portal_follows(vm, 2));
    report.finish()?;
    println!("     light: portal {} accent {}", appearance(vm, "color-scheme")?, appearance(vm, "accent-color")?);

    gtk3_window(vm, ctl, artifacts, "05-gtk3-breeze-light", &["theme=Breeze", "icons=breeze", "dark=False"])?;

    // The user picks Breeze Dark in Plasma; GTK follows without help.
    user(vm, "plasma-apply-colorscheme BreezeDark")?;
    vm.wait_for(
        "the portal to report the dark scheme",
        &format!(
            "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/$(id -u {USER}) \
             busctl --user call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
             org.freedesktop.portal.Settings Read ss org.freedesktop.appearance color-scheme | grep -x 'v v u 1'"
        ),
        Duration::from_secs(60),
    )?;
    let mut report = vmtest::Report::default();
    report.check("the portal follows Plasma's dark scheme", portal_follows(vm, 1));
    report.check("kde-gtk-config follows it too, for GTK 3 and GTK 4", {
        let kde = user(vm, "env DCONF_PROFILE=ilex-kde dconf dump /org/gnome/desktop/interface/")?;
        let g3 = vm.ssh_ok(&format!("cat /var/home/{USER}/.config/gtk-3.0/settings.ini"))?;
        let g4 = vm.ssh_ok(&format!("cat /var/home/{USER}/.config/gtk-4.0/settings.ini"))?;
        let dark = "gtk-application-prefer-dark-theme=true";
        if kde.contains("color-scheme='prefer-dark'") && g3.contains(dark) && g4.contains(dark) {
            Ok(())
        } else {
            Err(anyhow!("kde keys:\n{kde}\ngtk-3.0:\n{g3}\ngtk-4.0:\n{g4}"))
        }
    });
    report.finish()?;
    gtk3_window(vm, ctl, artifacts, "06-gtk3-breeze-dark", &["theme=Breeze", "icons=breeze", "dark=True"])?;
    println!("     dark: portal {} accent {}", appearance(vm, "color-scheme")?, appearance(vm, "accent-color")?);
    Ok(())
}

/// `color-scheme` is `want` (1 dark, 2 light) and the accent is Plasma's.
fn portal_follows(vm: &Vm, want: u32) -> Result<()> {
    let scheme = appearance(vm, "color-scheme")?;
    if scheme != format!("v v u {want}") {
        bail!("color-scheme is `{scheme}`, expected `v v u {want}`");
    }
    // `v v (ddd) r g b`, 0..1; Plasma's accent is its selection background.
    let accent = appearance(vm, "accent-color")?;
    let got: Vec<f64> = accent
        .split_whitespace()
        .skip(3)
        .map(|v| v.parse::<f64>())
        .collect::<Result<_, _>>()
        .with_context(|| format!("parsing the accent `{accent}`"))?;
    let plasma = user(vm, "kreadconfig6 --file kdeglobals --group Colors:Selection --key BackgroundNormal")?;
    let plasma: Vec<f64> = plasma.split(',').map(|v| v.trim().parse::<f64>().map(|c| c / 255.0)).collect::<Result<_, _>>()?;
    if got.len() != 3 || plasma.len() != 3 || got.iter().zip(&plasma).any(|(a, b)| (a - b).abs() > 0.01) {
        bail!("the portal's accent {got:?} is not Plasma's {plasma:?}");
    }
    Ok(())
}

/// Open the GTK 3 probe in the session, check what it resolved, screenshot it.
pub(super) fn gtk3_window(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path, name: &str, expect: &[&str]) -> Result<()> {
    let script = qemu::base64(GTK3_PROBE);
    vm.ssh_ok(&format!(
        "printf %s {script} | base64 -d > /tmp/ilex-gtk3-probe.py && chmod 644 /tmp/ilex-gtk3-probe.py"
    ))?;
    // A previous window is closed first; the unit is not a Plasma child.
    let _ = user(vm, &format!("systemctl --user stop {PROBE_UNIT}.service"));
    user(
        vm,
        &format!(
            "systemd-run --user --collect --quiet --unit={PROBE_UNIT} -p RuntimeMaxSec=300 \
             python3 -I /tmp/ilex-gtk3-probe.py 280"
        ),
    )?;
    let seen = vm.wait_for(
        "the GTK 3 probe to report",
        &format!(
            "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/$(id -u {USER}) \
             journalctl --user -u {PROBE_UNIT}.service --no-pager -o cat | grep '^gtk3 theme='"
        ),
        Duration::from_secs(60),
    )?;
    println!("     {seen}");
    if let Some(missing) = expect.iter().find(|want| !seen.contains(**want)) {
        bail!("GTK 3 resolved `{seen}`, expected `{missing}` (all of {expect:?})");
    }
    std::thread::sleep(Duration::from_secs(4));
    settle(ctl)?;
    shot(ctl, artifacts, name)?;
    user(vm, &format!("systemctl --user stop {PROBE_UNIT}.service"))?;
    Ok(())
}

/// `graphical::apps` on KDE: real GTK 3 and libadwaita Flatpaks from Flathub
/// in the Plasma session. Needs the network.
mod apps {
    use super::*;

    pub(super) fn checks(_vm: &mut Vm, _ctl: &Vmctl, _artifacts: &Path) -> Result<()> {
        bail!("graphical::apps on KDE is not implemented yet")
    }
}
