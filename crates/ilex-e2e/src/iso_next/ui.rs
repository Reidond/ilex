//! The installer's own interface on the real ISO: its first pages, driven by
//! real key input through QMP, observed through the serial markers the
//! interface prints (`ILEX-INSTALLER page=… `, `ILEX-INSTALLER event=…`,
//! `ILEX-COMPOSITION …`) and through screenshots. The root helper is the real
//! one: its probe of this VM's SMBIOS identity, disks and registry reachability
//! is what the pages show.
//!
//! * `iso::next_welcome`: the welcome page draws; the keyboard layout list is
//!   opened, searched and chosen with the keyboard; GNOME Kiosk switches the
//!   session's layout at run time (typing `yz` into the page's test field
//!   gives `zy` in the German layout); Enter in that field continues to the
//!   machine page.
//! * `iso::next_detect_board`: a VM that reports the MSI MEG X570 UNIFY's
//!   baseboard identity (only the board's pair of SMBIOS strings) has its own
//!   configuration, `ms-7c35.gnome`, preselected.
//! * `iso::next_detect_generic`: a VM no machine configuration matches has the
//!   edition's generic one preselected, with the reason.
//! * `iso::next_kernel_argument`: `ilex.composition=` typed into GRUB's editor
//!   (twice: the last wins) is honoured as the model decides, and printed on
//!   the serial console in the format the selection trials have always read.
//! * `iso::next_kernel_argument_unknown`: a tag the edition does not offer is
//!   reported with the model's list of offers and the page stays usable on the
//!   detected configuration.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::core::{Capability, Requirement, SecurityLaunch, VmProvider};
use ilex_devkit::installer;
use ilex_devkit::provider::LinuxProvider;
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::Vmctl;
use libtest_mimic::Trial;

use super::{
    GRUB_COUNTDOWN, Guest, MEMORY, PAGE_TIMEOUT, blank_disk, complete_lines, console, diagnostics,
    field, is_marker, iso_path, trial, wait_for_text, wait_page, words,
};
use crate::common;
use crate::iso;

/// The tag the kernel argument names in `iso::next_kernel_argument`.
const OVERRIDE_TAG: &str = "ms-7c35.gnome";
/// A tag no edition offers.
const UNKNOWN_TAG: &str = "ms-7c35.kde";
/// How long the interface is given to answer one key press.
const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn trials() -> Vec<Trial> {
    vec![
        trial("iso::next_welcome", welcome),
        trial("iso::next_detect_board", || {
            detect("iso-next-board", true, "ms-7c35.gnome", "detected")
        }),
        trial("iso::next_detect_generic", || {
            detect("iso-next-generic", false, "generic.gnome", "not-recognised")
        }),
        trial("iso::next_kernel_argument", || {
            kernel_argument("iso-next-karg", OVERRIDE_TAG)
        }),
        trial("iso::next_kernel_argument_unknown", || {
            kernel_argument("iso-next-karg-bad", UNKNOWN_TAG)
        }),
    ]
}

/// A running installer VM and where its evidence goes.
struct Booted {
    guest: Guest,
    artifacts: PathBuf,
    iso: PathBuf,
    work: PathBuf,
}

/// Boot the ISO under Secure Boot on KVM, as the boot trials do, with the
/// given SMBIOS structures. A USB keyboard is attached when the trial edits
/// GRUB (some OVMF builds drop navigation keys from virtio-keyboard there).
fn boot(purpose: &'static str, usb_keyboard: bool, smbios: Vec<String>) -> Result<Booted> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let root = ws.state_dir().join("iso-next-fixtures").join(purpose);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let disk = root.join("blank.qcow2");
    blank_disk(&disk)?;

    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    let mut config = VmConfig::new(purpose, &disk);
    config.snapshot = false;
    config.memory = memory;
    config.cdrom = Some(iso.clone());
    config.smbios = smbios;
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    // Provisioning is required, a qualified host is not (see `boot` above).
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    let provider = LinuxProvider::new(ws.clone())?.with_usb_keyboard(usb_keyboard);
    let caps = provider.probe(&spec)?;
    caps.require_provisioning(Capability::FirmwareEnforcement)?;
    let vm = provider.create(spec)?;
    let work = root.join("work");
    fs::create_dir_all(&work)?;
    let mut guest = Guest { provider, vm, root };
    guest.provider.start(&mut guest.vm)?;
    ensure!(
        guest.vm.running_mut()?.info.secure_boot,
        "the VM was not started with Secure Boot"
    );
    Ok(Booted {
        guest,
        artifacts,
        iso,
        work,
    })
}

/// The `key=value` words of the first complete line of the serial console
/// that `matches`, waiting for it. Fails fast when the VM exited or the
/// session gave up.
fn wait_line(
    vm: &mut Vm,
    serial: &Path,
    what: &str,
    timeout: Duration,
    name: &str,
    matches: impl Fn(&BTreeMap<String, String>) -> bool,
) -> Result<BTreeMap<String, String>> {
    let started = Instant::now();
    loop {
        let text = console(serial)?;
        if let Some(found) = complete_lines(&text)
            .filter(|line| is_marker(line, name, None))
            .map(words)
            .find(|w| matches(w))
        {
            return Ok(found);
        }
        if name != "ILEX-INSTALLER-FALLBACK"
            && complete_lines(&text).any(|l| is_marker(l, "ILEX-INSTALLER-FALLBACK", None))
        {
            bail!("the session gave up while waiting for {what}; serial: {text}");
        }
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} while waiting for {what}; serial: {text}");
        }
        if started.elapsed() > timeout {
            bail!("no {what} within {timeout:?}; serial: {text}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn wait_page_marker(
    vm: &mut Vm,
    serial: &Path,
    page: &str,
    timeout: Duration,
) -> Result<BTreeMap<String, String>> {
    wait_line(
        vm,
        serial,
        &format!("ILEX-INSTALLER page={page}"),
        timeout,
        "ILEX-INSTALLER",
        |w| w.get("page").is_some_and(|p| p == page),
    )
}

fn wait_event(
    vm: &mut Vm,
    serial: &Path,
    event: &str,
    timeout: Duration,
    also: impl Fn(&BTreeMap<String, String>) -> bool,
) -> Result<BTreeMap<String, String>> {
    wait_line(
        vm,
        serial,
        &format!("ILEX-INSTALLER event={event}"),
        timeout,
        "ILEX-INSTALLER",
        |w| w.get("event").is_some_and(|e| e == event) && also(w),
    )
}

/// The welcome page is up and the helper answered the probe: the page is
/// drawn, with its marker and the probe's result.
fn welcome_up(vm: &mut Vm, ctl: &Vmctl, serial: &Path, artifacts: &Path, name: &str) -> Result<()> {
    let page = wait_page_marker(vm, serial, "welcome", PAGE_TIMEOUT)?;
    ensure!(
        field(&page, "edition")? == "gnome"
            && field(&page, "language")? == "en_US.UTF-8"
            && field(&page, "keyboard")? == "us",
        "the welcome page starts on {page:?}"
    );
    let probe = wait_event(vm, serial, "probe", EVENT_TIMEOUT, |_| true)?;
    ensure!(
        field(&probe, "result")? == "ok",
        "the helper's probe did not succeed: {probe:?}"
    );
    wait_page(vm, ctl, artifacts, name)?;
    Ok(())
}

fn welcome() -> Result<()> {
    let mut booted = boot("iso-next-welcome", false, vec![])?;
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_welcome(vm, &artifacts);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, "welcome");
    }
    outcome
}

fn observe_welcome(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    welcome_up(vm, &ctl, &serial, artifacts, "welcome")?;
    ctl.save_screenshot(&artifacts.join("next-welcome.png"))?;

    // The first control has the focus: Tab reaches the keyboard layout row,
    // Enter opens its list, typing searches it, Down and Enter choose.
    ctl.key("Tab")?;
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.save_screenshot(&artifacts.join("next-welcome-layouts.png"))?;
    ctl.type_text("german (no dead")?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.save_screenshot(&artifacts.join("next-welcome-layouts-search.png"))?;
    ctl.key("Down")?;
    ctl.key("Return")?;
    // GNOME Kiosk applies the layout a moment after the call: the interface
    // reports it once the compositor's selected source is the one asked for.
    let applied = wait_event(vm, &serial, "layout", EVENT_TIMEOUT, |w| {
        w.get("layout").is_some_and(|l| l == "de-nodeadkeys")
    })?;
    ensure!(
        field(&applied, "applied")? == "true",
        "GNOME Kiosk did not take the German layout: {applied:?}"
    );

    // The page's test field: the person types, the layout at work shows.
    // `y` and `z` swap places between the US and German layouts, and the
    // harness's keys are US keycodes.
    ctl.key("Tab")?;
    ctl.type_text("yz")?;
    let tried = wait_event(vm, &serial, "try", EVENT_TIMEOUT, |w| {
        w.get("text").is_some_and(|t| t == "zy")
    })
    .context(
        "typing y then z under the German layout did not give z then y: the session's layout \
         was not switched at run time",
    )?;
    ensure!(
        field(&tried, "layout")? == "de-nodeadkeys",
        "the test field reports {tried:?}"
    );
    std::thread::sleep(Duration::from_secs(1));
    ctl.save_screenshot(&artifacts.join("next-welcome-german.png"))?;

    // Enter in the field continues when the page is valid.
    ctl.key("Return")?;
    let machine = wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
    ensure!(
        field(&machine, "selected")? == "generic.gnome"
            && field(&machine, "origin")? == "not-recognised",
        "the machine page on a VM nothing matches: {machine:?}"
    );
    wait_page(vm, &ctl, artifacts, "machine")?;
    ctl.save_screenshot(&artifacts.join("next-welcome-machine.png"))?;
    Ok(())
}

/// Tab through the welcome page's three controls to Continue and press it:
/// the keyboard alone moves on.
fn continue_to_machine(ctl: &Vmctl) -> Result<()> {
    for _ in 0..3 {
        ctl.key("Tab")?;
    }
    ctl.key("Return")
}

fn detect(
    purpose: &'static str,
    board: bool,
    expected: &str,
    origin: &str,
) -> Result<()> {
    let smbios = if board {
        // The MSI MEG X570 UNIFY, with only the baseboard's pair of strings
        // (the system's are what a prebuilt or a firmware fault would lose).
        let ws = common::workspace()?;
        let table = installer::table(&ws.root, None)?;
        iso::smbios(&table, Some("ms-7c35"), true)?
    } else {
        vec![]
    };
    let mut booted = boot(purpose, false, smbios)?;
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_machine(vm, &artifacts, purpose, expected, origin);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, purpose);
    }
    outcome
}

fn observe_machine(
    vm: &mut Vm,
    artifacts: &Path,
    name: &str,
    expected: &str,
    origin: &str,
) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    welcome_up(vm, &ctl, &serial, artifacts, name)?;
    continue_to_machine(&ctl)?;
    let machine = wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
    check_machine(vm, &serial, &machine, expected, origin)?;
    wait_page(vm, &ctl, artifacts, name)?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}.png")))?;
    Ok(())
}

/// The machine page's marker and the serial line the selection trials read.
fn check_machine(
    vm: &mut Vm,
    serial: &Path,
    machine: &BTreeMap<String, String>,
    expected: &str,
    origin: &str,
) -> Result<()> {
    ensure!(
        field(machine, "selected")? == expected && field(machine, "origin")? == origin,
        "the machine page preselected {machine:?}, expected {expected} ({origin})"
    );
    ensure!(
        field(machine, "offered")? == "2",
        "the edition offers {machine:?}, expected generic and ms-7c35 (no desktop choice)"
    );
    // The line starts with the tag and a reason in parentheses; the target is
    // what the installed system tracks.
    let target = format!("ghcr.io/reidond/ilex:{expected}");
    let started = Instant::now();
    loop {
        let text = console(serial)?;
        let line = complete_lines(&text).find(|l| {
            l.starts_with(&format!("ILEX-COMPOSITION {expected} ("))
                && l.ends_with(&format!("installs {target}"))
        });
        if let Some(line) = line {
            println!("iso::next machine page: {line}");
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(20) || vm.exited()?.is_some() {
            bail!(
                "no `ILEX-COMPOSITION {expected} (…) installs {target}` line on the serial console: {text}"
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn kernel_argument(purpose: &'static str, tag: &str) -> Result<()> {
    let mut booted = boot(purpose, true, vec![])?;
    let artifacts = booted.artifacts.clone();
    let (iso, work) = (booted.iso.clone(), booted.work.clone());
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_kernel_argument(vm, &artifacts, &iso, &work, purpose, tag);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, purpose);
    }
    outcome
}

fn observe_kernel_argument(
    vm: &mut Vm,
    artifacts: &Path,
    iso: &Path,
    work: &Path,
    name: &str,
    tag: &str,
) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    // The countdown is the point at which GRUB starts accepting edit keys.
    wait_for_text(vm, &serial, GRUB_COUNTDOWN, PAGE_TIMEOUT)?;
    iso::edit_kernel_line(
        iso,
        work,
        &format!(" ilex.composition=invalid-first-choice ilex.composition={tag}"),
        &ctl,
        artifacts,
        &format!("next-{name}-grub-edit.png"),
        vm,
        &serial,
    )?;
    welcome_up(vm, &ctl, &serial, artifacts, name)?;
    continue_to_machine(&ctl)?;
    let machine = wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
    if tag == OVERRIDE_TAG {
        // The last argument wins; the edition offers it; the page says it
        // came from the boot options.
        check_machine(vm, &serial, &machine, tag, "chosen")?;
        let text = console(&serial)?;
        ensure!(
            complete_lines(&text).any(|l| l
                == format!(
                    "ILEX-COMPOSITION {tag} (chosen at boot) installs ghcr.io/reidond/ilex:{tag}"
                )),
            "the boot argument's serial line is not the model's: {text}"
        );
    } else {
        // A tag this edition does not offer is the model's error, with its
        // list of offers; the detection's choice stays selected.
        ensure!(
            field(&machine, "origin")? == "bad-override" && field(&machine, "selected")? == "generic.gnome",
            "an unknown tag on the page: {machine:?}"
        );
    }
    wait_page(vm, &ctl, artifacts, name)?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}.png")))?;
    Ok(())
}
