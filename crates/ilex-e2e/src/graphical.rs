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
//! Then the display scaling checks (see [`scaling`]).
//!
//! Assertions come from SSH, never from pixel diffs: font and antialiasing
//! changes between Fedora updates would make those flaky. Screenshots are
//! evidence for review.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::{Frame, Vmctl, WaitCond};
use libtest_mimic::{Failed, Trial};

use crate::common;
use crate::disk::vmtest;

const USER: &str = "ilextest";
/// The user's stored display configuration (what GNOME Settings writes).
const MONITORS_XML: &str = "/var/home/ilextest/.config/monitors.xml";
/// Software-rendered 4K is slow, and the suite runs with other VMs busy:
/// give each wait generously.
const SESSION_TIMEOUT: Duration = Duration::from_secs(300);
/// The guest's final display mode (`VmConfig::graphical`).
const FRAMEBUFFER: (u32, u32) = (3840, 2160);

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
    println!("ok   gdm is active and the screen settled at {}x{}", frame.width, frame.height);
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
    wait_session(vm)?;
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
    report.finish()?;

    scaling(vm, ctl, artifacts, &password)
}

/// Display scaling is GNOME's own, so this tests that the image keeps it.
///
/// mutter 51 runs in logical layout mode (fractional scales; the old
/// `scale-monitor-framebuffer` switch is gone) and gives every monitor
/// without a stored configuration a default scale from the panel's physical
/// DPI (target 110 DPI, 135 for panels under 20 inches, nearest scale offered
/// for the mode). Checked here: fractional scaling is on, an unconfigured
/// monitor gets mutter's own pick, and a configuration the user stored in
/// `~/.config/monitors.xml` wins over it, also across a reboot.
///
/// The VM cannot present a real panel: virtio-gpu reports ~100 DPI and
/// ignores DRM debugfs `edid_override`, so the automatic pick here is 1.0 and
/// the scaled desktop (screenshot 06) comes from the stored choice.
fn scaling(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path, password: &str) -> Result<()> {
    let auto = display_state(vm)?;
    println!(
        "     {} {}x{}@{}: layout {}, scale {}, mutter's pick {}, offered {:?}",
        auto.connector, auto.width, auto.height, auto.rate, auto.layout_mode, auto.scale, auto.preferred, auto.supported
    );
    let mut report = vmtest::Report::default();
    report.check("fractional scaling is on by default (logical layout, 1.5 offered)", {
        if auto.layout_mode != "logical" {
            Err(anyhow!("layout mode is `{}`", auto.layout_mode))
        } else if !auto.supported.iter().any(|s| (s - 1.5).abs() < 0.01) {
            Err(anyhow!("1.5 is not offered for {}x{}", auto.width, auto.height))
        } else {
            Ok(())
        }
    });
    report.check("an unconfigured monitor gets mutter's own DPI-based scale", {
        if (auto.scale - auto.preferred).abs() > 0.001 {
            Err(anyhow!("scale is {} but mutter prefers {}", auto.scale, auto.preferred))
        } else if (auto.scale - 1.0).abs() > 0.001 {
            // The virtual panel is ~100 DPI on 44 inches: 100 / 110 rounds to 1.0.
            Err(anyhow!("the ~100 DPI virtual screen should stay at 1.0, not {}", auto.scale))
        } else {
            Ok(())
        }
    });
    report.check("nothing is stored in monitors.xml yet", {
        let stored = vm.ssh(&format!("test -e {MONITORS_XML}"))?;
        if stored.status.success() { Err(anyhow!("{MONITORS_XML} exists")) } else { Ok(()) }
    });
    report.finish()?;

    // The user's choice, in the file Settings would write: 150% for this monitor.
    let xml = monitors_xml(&auto, 1.5);
    vm.ssh_ok(&format!(
        "echo {} | base64 -d > {MONITORS_XML} && chown {USER}:{USER} {MONITORS_XML}",
        qemu::base64(xml.as_bytes())
    ))?;
    println!("ok   stored a 150% choice in {MONITORS_XML}");

    // A fresh login after a reboot, so the whole stack restarts, must honour it.
    vm.reboot(vmtest::BOOT_TIMEOUT).context("rebooting with the stored display config")?;
    wait_gdm(vm)?;
    settle(ctl)?;
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(3));
    ctl.type_text(password)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    wait_session(vm)?;
    settle(ctl)?;
    // First-login noise (welcome tour, updater prompt): clear what is there.
    ctl.key("Escape")?;
    std::thread::sleep(Duration::from_secs(2));
    ctl.key("Escape")?;
    settle(ctl)?;
    // The overview (search bar, dock) shows the scale better than a bare wallpaper.
    ctl.key("super")?;
    settle(ctl)?;
    shot(ctl, artifacts, "06-scaled-desktop")?;

    let mut report = vmtest::Report::default();
    report.check("a stored user choice wins over the automatic scale", {
        let now = display_state(vm)?;
        println!("     after the reboot: scale {} (mutter's own pick would be {})", now.scale, now.preferred);
        if (now.scale - 1.5).abs() < 0.01 { Ok(()) } else { Err(anyhow!("scale is {}, expected the stored 1.5", now.scale)) }
    });
    report.check("loginctl shows an active Wayland session", session_active(vm));
    report.check("no user units failed", no_failed_user_units(vm));
    report.finish()
}

/// What `gdctl show --modes --properties` says about the (single) monitor.
#[derive(Debug, Default)]
struct DisplayState {
    layout_mode: String,
    connector: String,
    vendor: String,
    product: String,
    serial: String,
    width: u32,
    height: u32,
    /// Refresh rate of the current mode as printed, e.g. `75.000`.
    rate: String,
    /// Scale of the logical monitor.
    scale: f64,
    /// Scale mutter would choose for the current mode (from the panel's DPI).
    preferred: f64,
    /// Scales mutter offers for the current mode.
    supported: Vec<f64>,
}

fn display_state(vm: &Vm) -> Result<DisplayState> {
    let text = as_user(vm, "gdctl show --modes --properties")?;
    parse_display_state(&text).with_context(|| format!("parsing gdctl output:\n{text}"))
}

fn parse_display_state(text: &str) -> Result<DisplayState> {
    let mut st = DisplayState::default();
    let (mut mode, mut pref, mut supported) = (String::new(), 0.0, Vec::new());
    let mut have_scale = false;
    for line in text.lines() {
        // Strip the tree-drawing prefix.
        let t = line.trim_start_matches(|c: char| " │├└─".contains(c)).trim_end();
        if let Some(rest) = t.strip_prefix("Monitor ") {
            if st.connector.is_empty() {
                st.connector = rest.split_whitespace().next().unwrap_or("").to_string();
            }
        } else if let Some(v) = t.strip_prefix("Vendor: ") {
            st.vendor = v.to_string();
        } else if let Some(v) = t.strip_prefix("Product: ") {
            st.product = v.to_string();
        } else if let Some(v) = t.strip_prefix("Serial: ") {
            st.serial = v.to_string();
        } else if t.contains('x') && t.contains('@') && t.starts_with(|c: char| c.is_ascii_digit()) {
            mode = t.to_string();
        } else if let Some(v) = t.strip_prefix("Preferred scale: ") {
            pref = v.trim().parse()?;
        } else if let Some(v) = t.strip_prefix("Supported scales: ") {
            supported = v
                .trim_matches(|c| c == '[' || c == ']')
                .split(',')
                .map(|s| s.trim().parse::<f64>())
                .collect::<Result<_, _>>()?;
        } else if t.starts_with("is-current") && t.ends_with("yes") {
            let (dims, rate) = mode.split_once('@').context("current mode has no refresh rate")?;
            let (w, h) = dims.split_once('x').context("current mode has no size")?;
            st.width = w.parse()?;
            st.height = h.parse()?;
            st.rate = rate.to_string();
            st.preferred = pref;
            st.supported = supported.clone();
        } else if let Some(v) = t.strip_prefix("Scale: ") {
            if !have_scale {
                st.scale = v.trim().parse()?;
                have_scale = true;
            }
        } else if t.starts_with("layout-mode") {
            st.layout_mode = t.split_whitespace().last().unwrap_or("").to_string();
        }
    }
    if st.connector.is_empty() || st.width == 0 || !have_scale || st.layout_mode.is_empty() {
        bail!("incomplete display state: {st:?}");
    }
    Ok(st)
}

/// A `monitors.xml` (version 2) storing `scale` for the monitor and mode in `st`.
fn monitors_xml(st: &DisplayState, scale: f64) -> String {
    format!(
        "<monitors version=\"2\">\n\
         \x20 <configuration>\n\
         \x20   <layoutmode>logical</layoutmode>\n\
         \x20   <logicalmonitor>\n\
         \x20     <x>0</x>\n\
         \x20     <y>0</y>\n\
         \x20     <scale>{scale}</scale>\n\
         \x20     <primary>yes</primary>\n\
         \x20     <monitor>\n\
         \x20       <monitorspec>\n\
         \x20         <connector>{}</connector>\n\
         \x20         <vendor>{}</vendor>\n\
         \x20         <product>{}</product>\n\
         \x20         <serial>{}</serial>\n\
         \x20       </monitorspec>\n\
         \x20       <mode>\n\
         \x20         <width>{}</width>\n\
         \x20         <height>{}</height>\n\
         \x20         <rate>{}</rate>\n\
         \x20       </mode>\n\
         \x20     </monitor>\n\
         \x20   </logicalmonitor>\n\
         \x20 </configuration>\n\
         </monitors>\n",
        st.connector, st.vendor, st.product, st.serial, st.width, st.height, st.rate
    )
}

/// Run `cmd` as the test user, inside their session bus.
fn as_user(vm: &Vm, cmd: &str) -> Result<String> {
    let uid = vm.ssh_ok(&format!("id -u {USER}"))?;
    vm.ssh_ok(&format!(
        "runuser -u {USER} -- env XDG_RUNTIME_DIR=/run/user/{uid} \
         DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/{uid}/bus {cmd}"
    ))
}

/// A user session exists and its shell is running.
fn wait_session(vm: &mut Vm) -> Result<()> {
    vm.wait_for(
        "a graphical session for the test user",
        &format!("loginctl list-sessions --no-legend | awk '$3 == \"{USER}\" && $6 == \"user\"' | grep ."),
        SESSION_TIMEOUT,
    )?;
    vm.wait_for("gnome-shell in the user session", &format!("pgrep -u {USER} -x gnome-shell"), SESSION_TIMEOUT)?;
    Ok(())
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

/// Wait until the display shows something at the full framebuffer size and
/// has stopped changing.
///
/// Firmware, GRUB and Plymouth draw at their own mode (1280x800), and under
/// load mutter's modeset to 3840x2160 can come late, so the first non-blank
/// frame of a boot is not the one to judge: wait for the size too.
fn settle(ctl: &Vmctl) -> Result<Frame> {
    let cond = WaitCond {
        nonblank: true,
        size: Some(FRAMEBUFFER),
        stable: Some(Duration::from_secs(5)),
        ..WaitCond::default()
    };
    let (_, frame) = ctl
        .wait_screen(&cond, SESSION_TIMEOUT)
        .with_context(|| format!("waiting for a settled {}x{} screen", FRAMEBUFFER.0, FRAMEBUFFER.1))?;
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
    let failed = as_user(vm, "systemctl --user --failed --no-legend --plain")?;
    if failed.is_empty() { Ok(()) } else { bail!("failed user units:\n{failed}") }
}

fn no_failed_system_units(vm: &Vm) -> Result<()> {
    let failed = vm.ssh_ok("systemctl --failed --no-legend --plain")?;
    if failed.is_empty() { Ok(()) } else { bail!("failed units:\n{failed}") }
}
