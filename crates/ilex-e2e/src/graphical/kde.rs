//! The KDE branch of `graphical::`: Plasma Login Manager, a Plasma session and
//! the GTK look the shell ships.
//!
//! 1. wait for `plasmalogin` and its greeter; there is no user yet, so the
//!    greeter shows only the clock (screenshot 01);
//! 2. create a test user over SSH and reboot, so the greeter lists the user;
//!    a key press raises its password prompt, the password is typed in;
//! 3. check the Plasma session over SSH, then the GTK look: Plasma's look lives
//!    in a KDE-only database and nothing shared, the portal hands libadwaita
//!    Plasma's scheme and accent, and a GTK 3 window (the host's `python3-gobject`)
//!    resolves Breeze in light and dark (screenshots 05 and 06).
//!
//! libadwaita is not installed on a KDE-only host: a libadwaita application is a
//! Flatpak. `graphical::apps` installs real ones from Flathub (network), probes
//! what each toolkit resolved inside the sandbox, writes a setting with a
//! dconf-backed Flatpak and installs an application through Discover, see [`apps`].
//! With a homed account (`graphical::apps`) it also covers Plasma Login Manager
//! listing and unlocking an encrypted home.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ilex_devkit::qemu::{self, Vm};
use ilex_vmctl::{Button, Vmctl};

use super::{
    SESSION_TIMEOUT, USER, no_failed_system_units, no_failed_user_units, session_active, settle,
    shot, test_password, wait_greeter, wait_session,
};
use crate::common;
use crate::disk::vmtest;

const PROBE: &[u8] = include_bytes!("gtk-probe.py");

/// The unit that keeps a probe window open, one per window: the journal of an
/// earlier window must not answer for a later one.
const PROBE_UNIT: &str = "ilex-gtk-probe";

pub(super) fn session(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path, apps: bool) -> Result<()> {
    // 1. The greeter is up and has drawn something: with no user, the clock.
    wait_greeter(vm)?;
    let frame = settle(ctl)?;
    println!(
        "ok   plasmalogin is active and the screen settled at {}x{}",
        frame.width, frame.height
    );
    shot(ctl, artifacts, "01-first-boot")?;

    // 2. A test user, created for this boot; reboot so the greeter lists it.
    // The network test uses an encrypted systemd-homed account, as a daily
    // account is, and the greeter must list and unlock it.
    let password = test_password();
    if apps {
        create_homed_user(vm, &password)?;
    } else {
        create_user(vm, &password)?;
    }
    println!("ok   created test user {USER}");
    vm.reboot(vmtest::BOOT_TIMEOUT)
        .context("rebooting with the test user in place")?;
    wait_greeter(vm)?;
    settle(ctl)?;
    shot(ctl, artifacts, "02-greeter")?;
    // The greeter starts as a clock over the wallpaper: any key raises the
    // user's password prompt, which has the focus.
    login(vm, ctl, &password, artifacts)?;
    vm.wait_for(
        "the Plasma shell in the user session",
        &format!("pgrep -u {USER} -x plasmashell"),
        SESSION_TIMEOUT,
    )?;
    settle(ctl)?;
    shot(ctl, artifacts, "04-desktop")?;

    let mut report = vmtest::Report::default();
    report.check(
        "loginctl shows an active Wayland session",
        session_active(vm),
    );
    report.check("the session is Plasma", session_is_plasma(vm));
    report.check("no user units failed", no_failed_user_units(vm));
    report.check("no system units failed", no_failed_system_units(vm));
    if apps {
        report.check(
            "the homed account is unlocked at login and its home is mounted",
            homed_session(vm),
        );
    }
    report.finish()?;

    if apps {
        apps::checks(vm, ctl, artifacts)
    } else {
        theming(vm, ctl, artifacts)
    }
}

/// A key raises the user's prompt; the password is typed in.
fn login(vm: &mut Vm, ctl: &Vmctl, password: &str, artifacts: &Path) -> Result<()> {
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(3));
    shot(ctl, artifacts, "03-password-prompt")?;
    ctl.type_text(password)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    wait_session(vm)
}

/// The test account, with Plasma's configuration seeded.
fn create_user(vm: &Vm, password: &str) -> Result<()> {
    vm.ssh_ok(&format!(
        "useradd --create-home --shell /bin/bash {USER} && echo '{USER}:{password}' | chpasswd"
    ))?;
    seed_plasma_config(vm, &format!("/var/home/{USER}"))
}

/// The same account as an encrypted systemd-homed home, created through the
/// lifecycle's guest driver and left locked.
fn create_homed_user(vm: &Vm, password: &str) -> Result<()> {
    // Large enough for what the application tests install in the user's own
    // Flatpak installation: Discover's user-scope install brings its own copy of
    // the runtime (about a gigabyte), which the default 2 GiB home cannot hold.
    crate::homed::guest(
        vm,
        serde_json::json!({"action":"create", "name":USER, "admin":true, "secrets":[password], "disk_size":8}),
    )?;
    crate::homed::guest(
        vm,
        serde_json::json!({"action":"activate", "name":USER, "secrets":[password]}),
    )?;
    seed_plasma_config(vm, &format!("/home/{USER}"))?;
    vm.ssh_ok(&format!("homectl deactivate {USER}"))?;
    Ok(())
}

/// Plasma's screen locker and power management are switched off in the
/// account's configuration, so a slow run is not interrupted by a locked or
/// blanked screen.
pub(super) fn seed_plasma_config(vm: &Vm, home: &str) -> Result<()> {
    vm.ssh_ok(&format!(
        "set -e; home={home}; install -d -o {USER} -g {USER} -m 0700 $home/.config; \
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

/// Plasma Login Manager unlocked a LUKS home: its filesystem is mounted for the
/// account and the user manager runs from it.
fn homed_session(vm: &Vm) -> Result<()> {
    let state = vm.ssh_ok(&format!("homectl inspect {USER} | grep -E '^ +State:'"))?;
    let home = home(vm)?;
    if !state.contains("active") || !home.starts_with("/home/") {
        bail!("homed state `{state}`, home `{home}`");
    }
    vm.ssh_ok(&format!("mountpoint -q {home}"))?;
    println!("     {state}; home {home}");
    Ok(())
}

/// The account's home as the session sees it.
pub(super) fn home(vm: &Vm) -> Result<String> {
    vm.ssh_ok(&format!("getent passwd {USER} | cut -d: -f6"))
}

/// The ssh command that runs `cmd` as the test user, in their session: the user
/// manager's bus.
pub(super) fn user_cmd(uid: &str, cmd: &str) -> String {
    format!(
        "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/{uid} \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus {cmd}"
    )
}

/// Run `cmd` as the test user, on the bus of their session.
pub(super) fn user(vm: &Vm, cmd: &str) -> Result<String> {
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    vm.ssh_ok(&user_cmd(&uid, cmd))
}

/// Run `cmd` as an application the session launches: a transient unit of the
/// user manager, so with exactly the manager's environment (what Plasma or
/// GNOME exported into it), its output captured.
pub(super) fn app(vm: &Vm, cmd: &str) -> Result<String> {
    user(
        vm,
        &format!("systemd-run --user --pipe --wait --quiet {cmd}"),
    )
}

/// The portal's `org.freedesktop.appearance` value, as `busctl` prints it.
pub(super) fn appearance(vm: &Vm, key: &str) -> Result<String> {
    user(
        vm,
        &format!(
            "busctl --user call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
             org.freedesktop.portal.Settings Read ss org.freedesktop.appearance {key}"
        ),
    )
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
    let plasma = user(
        vm,
        "kreadconfig6 --file kdeglobals --group Colors:Selection --key BackgroundNormal",
    )?;
    let plasma: Vec<f64> = plasma
        .split(',')
        .map(|v| v.trim().parse::<f64>().map(|c| c / 255.0))
        .collect::<Result<_, _>>()?;
    if got.len() != 3
        || plasma.len() != 3
        || got.iter().zip(&plasma).any(|(a, b)| (a - b).abs() > 0.01)
    {
        bail!("the portal's accent {got:?} is not Plasma's {plasma:?}");
    }
    Ok(())
}

// ---- What a toolkit resolved -------------------------------------------------

/// A `gtkprobe` line: what a toolkit resolved in a window, as `key=value` fields.
#[derive(Debug)]
pub(super) struct Probe {
    line: String,
}

impl Probe {
    fn parse(output: &str) -> Result<Probe> {
        output
            .lines()
            .find(|l| l.starts_with("gtkprobe "))
            .map(|l| Probe {
                line: l.trim().to_string(),
            })
            .with_context(|| format!("the probe printed no result:\n{output}"))
    }

    pub(super) fn field(&self, key: &str) -> Option<&str> {
        self.line
            .split_whitespace()
            .find_map(|f| f.strip_prefix(key).and_then(|f| f.strip_prefix('=')))
    }

    /// The window background's relative brightness, 0 (black) to 1 (white).
    fn brightness(&self) -> Result<f64> {
        let bg = self.field("bg").context("the probe has no background")?;
        let hex = bg.strip_prefix('#').filter(|h| h.len() == 6);
        let hex = hex.with_context(|| format!("not a colour: `{bg}` in `{}`", self.line))?;
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).map(f64::from);
        Ok((0.299 * channel(0)? + 0.587 * channel(2)? + 0.114 * channel(4)?) / 255.0)
    }

    pub(super) fn describe(&self) -> &str {
        &self.line
    }

    /// The toolkit resolved `theme` (when given), a `dark` or light window
    /// background, and the `icons` (when given). The theme name alone proves
    /// little: a theme that is not found leaves the name set and the colours
    /// at the toolkit's own default.
    pub(super) fn expect(
        &self,
        theme: Option<&str>,
        icons: Option<&str>,
        dark: bool,
    ) -> Result<()> {
        for (key, want) in [("theme", theme), ("icons", icons)] {
            if let Some(want) = want
                && self.field(key) != Some(want)
            {
                bail!("{key} is not `{want}`: {}", self.line);
            }
        }
        let brightness = self.brightness()?;
        if (brightness < 0.5) != dark {
            bail!(
                "the window background is {} (brightness {brightness:.2}), expected {}: {}",
                self.field("bg").unwrap_or("?"),
                if dark { "dark" } else { "light" },
                self.line
            );
        }
        Ok(())
    }
}

fn push_probe(vm: &Vm) -> Result<()> {
    vm.ssh_ok(&format!(
        "printf %s {} | base64 -d > /tmp/gtk-probe.py && chmod 644 /tmp/gtk-probe.py",
        qemu::base64(PROBE)
    ))?;
    Ok(())
}

/// What `toolkit` (`gtk3`, `gtk4` or `adw`) resolves as an application of the
/// session, or inside the sandbox of the Flatpak `sandbox`.
pub(super) fn probe(vm: &Vm, toolkit: &str, sandbox: Option<&str>) -> Result<Probe> {
    push_probe(vm)?;
    let cmd = match sandbox {
        None => format!("python3 -I /tmp/gtk-probe.py {toolkit} 2"),
        Some(app) => format!(
            "flatpak run --filesystem=/tmp:ro --command=python3 {app} /tmp/gtk-probe.py {toolkit} 2"
        ),
    };
    Probe::parse(&app(vm, &cmd)?)
}

/// Open the probe's window, for a screenshot; returns what it resolved, read
/// from the unit's journal.
pub(super) fn show(
    vm: &mut Vm,
    ctl: &Vmctl,
    artifacts: &Path,
    name: &str,
    toolkit: &str,
    sandbox: Option<&str>,
) -> Result<Probe> {
    push_probe(vm)?;
    // A previous window is closed first; the unit is not a Plasma child.
    let _ = user(
        vm,
        &format!("systemctl --user stop '{PROBE_UNIT}-*.service'"),
    );
    let unit = format!("{PROBE_UNIT}-{name}");
    let inner = match sandbox {
        None => format!("python3 -I /tmp/gtk-probe.py {toolkit} 280 window"),
        Some(app) => format!(
            "flatpak run --filesystem=/tmp:ro --command=python3 {app} /tmp/gtk-probe.py {toolkit} 280 window"
        ),
    };
    user(
        vm,
        &format!("systemd-run --user --collect --quiet --unit={unit} -p RuntimeMaxSec=300 {inner}"),
    )?;
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    let seen = vm.wait_for(
        "the probe window to report",
        &user_cmd(
            &uid,
            &format!("journalctl --user -u {unit}.service --no-pager -o cat | grep '^gtkprobe '"),
        ),
        Duration::from_secs(90),
    )?;
    let probe = Probe::parse(&seen)?;
    println!("     {}", probe.describe());
    std::thread::sleep(Duration::from_secs(4));
    settle(ctl)?;
    shot(ctl, artifacts, name)?;
    user(vm, &format!("systemctl --user stop {unit}.service"))?;
    Ok(probe)
}

// ---- Plasma's look -----------------------------------------------------------

/// The KDE-only database's `org.gnome.desktop.interface` keys, as dconf dumps them.
fn kde_database(vm: &Vm) -> Result<String> {
    user(
        vm,
        "env DCONF_PROFILE=ilex-kde dconf dump /org/gnome/desktop/interface/",
    )
}

/// Wait until the sync has put `gtk-theme` into the KDE-only database.
fn wait_gtk_theme(vm: &mut Vm, theme: &str) -> Result<()> {
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    vm.wait_for(
        &format!("the KDE database to hold gtk-theme {theme}"),
        &user_cmd(
            &uid,
            &format!("env DCONF_PROFILE=ilex-kde dconf read /org/gnome/desktop/interface/gtk-theme | grep -x \"'{theme}'\""),
        ),
        Duration::from_secs(60),
    )
    .map(drop)
}

/// Plasma's choices exist nowhere a desktop of the same home reads: not in the
/// user's own dconf database, not in a GTK settings file.
pub(super) fn nothing_shared(vm: &Vm) -> Result<()> {
    let shared = user(vm, "dconf dump /org/gnome/desktop/interface/")?;
    if shared.contains("Breeze") || shared.contains("breeze") {
        bail!("the user's own database holds Plasma's keys:\n{shared}");
    }
    let home = home(vm)?;
    // The settings files GTK reads from the home (an application may create the
    // directories for its own bookmarks).
    for path in [
        ".config/gtk-3.0/settings.ini",
        ".config/gtk-3.0/gtk.css",
        ".config/gtk-3.0/colors.css",
        ".config/gtk-4.0/settings.ini",
        ".config/gtk-4.0/gtk.css",
        ".gtkrc-2.0",
        ".config/xsettingsd/xsettingsd.conf",
    ] {
        if vm.ssh(&format!("test -e {home}/{path}"))?.status.success() {
            bail!("{home}/{path} exists: Plasma wrote a shared GTK file");
        }
    }
    Ok(())
}

/// The session's environment carries what a Plasma session needs of GTK, and
/// nothing a Flatpak sandbox or a later GNOME session could trip over.
fn manager_environment(vm: &Vm) -> Result<()> {
    let env = user(vm, "systemctl --user show-environment")?;
    if !env.lines().any(|l| l == "GTK_USE_PORTAL=1") {
        bail!("GTK_USE_PORTAL=1 is not in the session environment:\n{env}");
    }
    if env
        .lines()
        .any(|l| l.starts_with("DCONF_PROFILE=") || l.starts_with("GTK_THEME="))
    {
        bail!("the session environment carries a profile or theme:\n{env}");
    }
    Ok(())
}

/// The GTK settings portal reads the KDE-only database, because it started in a
/// Plasma session.
fn portal_reads_kde_database(vm: &Vm) -> Result<()> {
    // Reading a GTK-owned key starts the portal if it has not run yet.
    user(
        vm,
        "busctl --user call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
         org.freedesktop.portal.Settings Read ss org.gnome.desktop.interface gtk-theme",
    )?;
    let profile = vm.ssh_ok(&format!(
        "for pid in $(pgrep -u {USER} -f '^/usr/libexec/xdg-desktop-portal-gtk'); do \
           tr '\\0' '\\n' < /proc/$pid/environ | grep '^DCONF_PROFILE='; done"
    ))?;
    if profile.trim() != "DCONF_PROFILE=ilex-kde" {
        bail!("the GTK portal's DCONF_PROFILE is `{profile}`");
    }
    Ok(())
}

/// The user's own Flathub remote, from the committed trust file: what Discover
/// lists and installs from.
fn flathub_remote(vm: &mut Vm) -> Result<()> {
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    vm.wait_for(
        "the user's Flathub remote",
        &user_cmd(&uid, "flatpak remotes --user --columns=name,url | grep -x -P 'flathub\\thttps://dl.flathub.org/repo/'"),
        Duration::from_secs(60),
    )?;
    let home = home(vm)?;
    let config = vm.ssh_ok(&format!("cat {home}/.local/share/flatpak/repo/config"))?;
    if !config.lines().any(|l| l == "gpg-verify=true") {
        bail!("the user's Flathub remote does not verify signatures:\n{config}");
    }
    Ok(())
}

/// Switch the session to Breeze Dark or Light and wait until the portal and the
/// KDE-only database say so.
pub(super) fn colours(vm: &mut Vm, dark: bool) -> Result<()> {
    let (scheme, theme, portal) = if dark {
        ("BreezeDark", "Breeze-Dark", 1)
    } else {
        ("BreezeLight", "Breeze", 2)
    };
    // A Qt tool: it needs the session's display, so it runs as an application.
    app(vm, &format!("plasma-apply-colorscheme {scheme}"))?;
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    vm.wait_for(
        &format!("the portal to report scheme {portal}"),
        &user_cmd(
            &uid,
            &format!(
                "busctl --user call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
                 org.freedesktop.portal.Settings Read ss org.freedesktop.appearance color-scheme | grep -x 'v v u {portal}'"
            ),
        ),
        Duration::from_secs(60),
    )?;
    wait_gtk_theme(vm, theme)
}

/// Quit Plasma the way a user does, through its own logout, and wait for the
/// greeter (never `loginctl terminate-user`: that kills Plasma's own shutdown).
pub(super) fn logout(vm: &mut Vm) -> Result<()> {
    user(
        vm,
        "busctl --user call org.kde.Shutdown /Shutdown org.kde.Shutdown logout",
    )?;
    vm.wait_for(
        "the Plasma session to end",
        &format!("! loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\"' | grep -q ."),
        SESSION_TIMEOUT,
    )
    .map(drop)?;
    wait_greeter(vm)
}

/// The GTK look of the Plasma session, light and dark.
pub(super) fn theming(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    let mut report = vmtest::Report::default();
    report.check(
        "the session environment has GTK_USE_PORTAL and no profile or theme",
        manager_environment(vm),
    );
    report.check("the Plasma-only sync and session watch are running", {
        let mut inactive = Vec::new();
        for unit in ["ilex-kde-gtk.service", "ilex-kde-session.service"] {
            let state = user(vm, &format!("systemctl --user is-active {unit} || true"))?;
            if state != "active" {
                inactive.push(format!("{unit} is {state}"));
            }
        }
        if inactive.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("{}", inactive.join(", ")))
        }
    });
    report.check("the KDE database holds Plasma's Breeze look", {
        wait_gtk_theme(vm, "Breeze")?;
        let keys = kde_database(vm)?;
        if keys.contains("icon-theme='breeze'") && keys.contains("color-scheme='prefer-light'") {
            Ok(())
        } else {
            Err(anyhow!("KDE keys:\n{keys}"))
        }
    });
    report.check(
        "Plasma wrote no shared GTK file and no key of the user's own database",
        nothing_shared(vm),
    );
    report.check(
        "the GTK settings portal reads the KDE database",
        portal_reads_kde_database(vm),
    );
    report.check(
        "the user has a Flathub remote of their own (Discover's)",
        flathub_remote(vm),
    );
    report.check(
        "the portal gives GTK 4 and libadwaita Plasma's light scheme and accent",
        portal_follows(vm, 2),
    );
    report.finish()?;
    println!(
        "     light: portal {} accent {}",
        appearance(vm, "color-scheme")?,
        appearance(vm, "accent-color")?
    );

    probe(vm, "gtk3", None)?.expect(Some("Breeze"), Some("breeze"), false)?;
    show(vm, ctl, artifacts, "05-gtk3-breeze-light", "gtk3", None)?.expect(
        Some("Breeze"),
        Some("breeze"),
        false,
    )?;

    // The user picks Breeze Dark in Plasma; GTK follows without help.
    colours(vm, true)?;
    let mut report = vmtest::Report::default();
    report.check(
        "the portal follows Plasma's dark scheme",
        portal_follows(vm, 1),
    );
    report.check("still nothing shared", nothing_shared(vm));
    report.finish()?;
    probe(vm, "gtk3", None)?.expect(Some("Breeze-Dark"), Some("breeze"), true)?;
    show(vm, ctl, artifacts, "06-gtk3-breeze-dark", "gtk3", None)?.expect(
        Some("Breeze-Dark"),
        Some("breeze"),
        true,
    )?;
    println!(
        "     dark: portal {} accent {}",
        appearance(vm, "color-scheme")?,
        appearance(vm, "accent-color")?
    );
    Ok(())
}

/// `graphical::apps` on KDE: real GTK 3 and libadwaita Flatpaks from Flathub
/// in the Plasma session, beside the host-side checks of [`theming`]; the
/// preinstalled GTK 3 theme extensions; a dconf-backed Flatpak; and Discover.
/// Needs the network, and installs a few Flatpaks rather than the shell's list
/// (`ilexctl apps sync` of the whole list is a separate run).
mod apps {
    use super::*;

    /// Flathub downloads the GNOME runtimes (about two gigabytes).
    const INSTALL: Duration = Duration::from_secs(2400);

    /// The refs of the shell's GTK 3 theme extensions, from its manifest.
    fn extension_refs() -> Result<Vec<String>> {
        let manifest = common::manifest("shells", "kde", "shell.toml")?;
        manifest
            .get("apps")
            .and_then(|a| a.get("flatpak-extension"))
            .and_then(|a| a.as_array())
            .context("the KDE shell declares no Flatpak extensions")?
            .iter()
            .map(|e| {
                Ok(format!(
                    "{}//{}",
                    e.get("id")
                        .and_then(|v| v.as_str())
                        .context("extension id")?,
                    e.get("branch")
                        .and_then(|v| v.as_str())
                        .context("extension branch")?
                ))
            })
            .collect()
    }

    pub(super) fn checks(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
        theming(vm, ctl, artifacts)?;

        let extensions = extension_refs()?.join(" ");
        vm.ssh_ok(&format!(
            "systemd-run --unit=ilex-kde-flatpaks --collect --quiet flatpak install --system -y --noninteractive flathub \
             org.gnome.Calculator org.gnome.gedit ca.desrt.dconf-editor {extensions}"
        ))?;
        vm.wait_for(
            "the Flatpaks to be installed",
            "! systemctl is-active --quiet ilex-kde-flatpaks.service && flatpak info --system org.gnome.Calculator \
             && flatpak info --system org.gnome.gedit && flatpak info --system ca.desrt.dconf-editor \
             && flatpak info --system org.gtk.Gtk3theme.Breeze && flatpak info --system org.gtk.Gtk3theme.Breeze-Dark",
            INSTALL,
        )?;
        println!(
            "ok   installed a libadwaita, a GTK 3 and a dconf-backed Flatpak and the GTK 3 theme extensions"
        );

        // In the session, still Breeze Dark from `theming`: what each toolkit
        // resolves inside its sandbox.
        sandboxes(vm, true)?;
        breeze_alone(vm)?;
        // The two applications, for the picture.
        for (unit, app) in [
            ("ilex-flatpak-calculator", "org.gnome.Calculator"),
            ("ilex-flatpak-gedit", "org.gnome.gedit"),
        ] {
            user(
                vm,
                &format!("systemd-run --user --collect --quiet --unit={unit} flatpak run {app}"),
            )?;
        }
        vm.wait_for(
            "both Flatpaks to be running",
            // comm is cut at 15 characters: match the command line instead.
            &format!("pgrep -u {USER} -f '^gnome-calculator' && pgrep -u {USER} -f '^gedit'"),
            SESSION_TIMEOUT,
        )?;
        std::thread::sleep(Duration::from_secs(5));
        settle(ctl)?;
        shot(ctl, artifacts, "07-flatpaks-dark")?;

        // And in light.
        colours(vm, false)?;
        std::thread::sleep(Duration::from_secs(5));
        settle(ctl)?;
        shot(ctl, artifacts, "08-flatpaks-light")?;
        sandboxes(vm, false)?;

        dconf_flatpak(vm)?;
        discover(vm, ctl, artifacts)
    }

    /// `org.gtk.Gtk3theme.Breeze-Dark` is end of life on Flathub, which names
    /// `org.gtk.Gtk3theme.Breeze` as its replacement. The replacement's dark
    /// colours are a variant GTK 3 loads only for `gtk-application-prefer-dark-theme`,
    /// which the settings portal does not carry (and a `settings.ini` for it would
    /// be read by every desktop of the home), so with the replacement alone a
    /// GTK 3 Flatpak stays light in a dark Plasma. The old extension stays in the
    /// shell's list for as long as this holds; if it stops holding, drop it.
    fn breeze_alone(vm: &mut Vm) -> Result<()> {
        vm.ssh_ok(
            "flatpak uninstall --system -y --noninteractive org.gtk.Gtk3theme.Breeze-Dark//3.22",
        )?;
        let alone = probe(vm, "gtk3", Some("org.gnome.gedit"));
        // Put it back before judging, so a failure leaves the guest as it was.
        vm.ssh_ok(
            "flatpak install --system -y --noninteractive flathub org.gtk.Gtk3theme.Breeze-Dark//3.22",
        )?;
        let alone = alone?;
        println!(
            "     Plasma dark, org.gtk.Gtk3theme.Breeze alone: {}",
            alone.describe()
        );
        if alone.expect(None, None, true).is_ok() {
            bail!(
                "a GTK 3 Flatpak resolves a dark look with org.gtk.Gtk3theme.Breeze alone ({}): \
                 drop org.gtk.Gtk3theme.Breeze-Dark from shells/kde/shell.toml",
                alone.describe()
            );
        }
        println!("ok   with Breeze alone a GTK 3 Flatpak stays light in a dark Plasma: Breeze-Dark stays preinstalled");
        Ok(())
    }

    /// GTK 3, GTK 4 and libadwaita as sandboxed applications: the theme and the
    /// colours the toolkit resolved, not the names the portal handed it.
    fn sandboxes(vm: &mut Vm, dark: bool) -> Result<()> {
        let mut report = vmtest::Report::default();
        let (gtk3_theme, label) = if dark {
            ("Breeze-Dark", "dark")
        } else {
            ("Breeze", "light")
        };
        report.check(
            &format!("a GTK 3 Flatpak resolves {gtk3_theme} ({label}) from the theme extension"),
            probe(vm, "gtk3", Some("org.gnome.gedit")).and_then(|p| {
                p.expect(Some(gtk3_theme), Some("breeze"), dark)
                    .map(|()| println!("     {}", p.describe()))
            }),
        );
        report.check(
            &format!("a GTK 4 Flatpak is {label}"),
            probe(vm, "gtk4", Some("org.gnome.Calculator")).and_then(|p| {
                p.expect(None, None, dark)
                    .map(|()| println!("     {}", p.describe()))
            }),
        );
        report.check(
            &format!("a libadwaita Flatpak is {label}, with Plasma's accent"),
            probe(vm, "adw", Some("org.gnome.Calculator")).and_then(|p| {
                println!("     {}", p.describe());
                p.expect(None, None, dark)?;
                // Plasma's selection blue is nearest libadwaita's blue.
                if p.field("adw_dark") != Some(if dark { "True" } else { "False" })
                    || p.field("accent") != Some("blue")
                {
                    bail!("libadwaita did not follow the portal: {}", p.describe());
                }
                Ok(())
            }),
        );
        report.finish()
    }

    /// A Flatpak that stores its settings in dconf writes one under Plasma and
    /// reads it back: no session-wide profile may reach the sandbox, whose
    /// runtime has no such profile and would lose the write.
    fn dconf_flatpak(vm: &mut Vm) -> Result<()> {
        // Started as a session application: the manager's environment, as a
        // launch from Plasma's menu has.
        let set = "flatpak run --command=gsettings ca.desrt.dconf-editor";
        let before = app(
            vm,
            &format!("{set} get ca.desrt.dconf-editor.Settings show-warning"),
        )?;
        let want = if before.trim() == "true" {
            "false"
        } else {
            "true"
        };
        app(
            vm,
            &format!("{set} set ca.desrt.dconf-editor.Settings show-warning {want}"),
        )?;
        let read = app(
            vm,
            &format!("{set} get ca.desrt.dconf-editor.Settings show-warning"),
        )?;
        let host = user(vm, "dconf read /ca/desrt/dconf-editor/show-warning")?;
        let sandbox_env = app(
            vm,
            "flatpak run --command=sh ca.desrt.dconf-editor -c 'printf \"%s\" \"${DCONF_PROFILE:-none}\"'",
        )?;
        println!(
            "     dconf-editor: wrote {want}, sandbox reads {read}, the user's database holds {host}, DCONF_PROFILE in the sandbox: {sandbox_env}"
        );
        if read.trim() != want || host.trim() != want || sandbox_env.trim() != "none" {
            bail!("a dconf-backed Flatpak lost its setting under Plasma");
        }
        Ok(())
    }

    /// Install an application through Discover itself, as a user would: the
    /// application's page, then the Install button. It must land in the user's
    /// own installation, from the user's Flathub remote, without the system
    /// catalogue or a prompt Discover cannot satisfy.
    fn discover(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
        // The catalogue Discover shows is the remote's AppStream data; fetch it
        // as GNOME Software's test does. Discover loads its backends after it
        // starts, and an application page asked for before that is "not found":
        // open the window first, give it time to load the catalogue, then ask the
        // running instance for the page (a second launch forwards to it).
        user(vm, "flatpak update --user --appstream -y --noninteractive")?;
        user(
            vm,
            "systemd-run --user --collect --quiet --unit=ilex-discover plasma-discover",
        )?;
        // Discover animates (a spinner while it loads, a carousel after): the
        // screen never settles, so these are timed pictures, not settled ones.
        std::thread::sleep(Duration::from_secs(45));
        shot(ctl, artifacts, "09-discover-home")?;
        user(
            vm,
            "systemd-run --user --collect --quiet --unit=ilex-discover-open plasma-discover appstream://org.gnome.baobab",
        )?;
        std::thread::sleep(Duration::from_secs(8));
        shot(ctl, artifacts, "10-discover-page")?;
        // "Install from Flathub (user)": the button of the application's page
        // in Discover's default window.
        ctl.click(1868, 522, Button::Left, 1)?;
        vm.wait_for(
            "Discover to install Disk Usage Analyzer for the user",
            &format!(
                "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/$(id -u {USER}) flatpak info --user org.gnome.baobab"
            ),
            Duration::from_secs(600),
        )?;
        std::thread::sleep(Duration::from_secs(5));
        shot(ctl, artifacts, "11-discover-installed")?;
        let mut report = vmtest::Report::default();
        report.check(
            "the application went to the user installation, not the system's",
            {
                let system = vm.ssh("flatpak info --system org.gnome.baobab")?;
                if system.status.success() {
                    Err(anyhow!("Disk Usage Analyzer is also installed system-wide"))
                } else {
                    Ok(())
                }
            },
        );
        report.check(
            "Discover brought the application's runtime along (it does not reuse the system's)",
            {
                let runtimes = user(vm, "flatpak list --user --runtime --columns=application")?;
                println!(
                    "     the user installation now holds: {}",
                    runtimes.replace('\n', ", ")
                );
                if runtimes.contains("org.gnome.Platform") {
                    Ok(())
                } else {
                    Err(anyhow!(
                        "no GNOME runtime in the user installation:\n{runtimes}"
                    ))
                }
            },
        );
        report.check("no system catalogue was fetched for Discover", {
            let catalogue = vm.ssh("test -e /var/lib/flatpak/appstream/flathub")?;
            if catalogue.status.success() {
                Err(anyhow!("the system Flathub catalogue exists"))
            } else {
                Ok(())
            }
        });
        report.finish()?;
        user(vm, "systemctl --user stop ilex-discover.service")?;
        Ok(())
    }
}
