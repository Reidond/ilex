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
//!   gives `zy` in the German layout; the field's text is printed only
//!   because this trial passes the fixed echo credential, the other trials do
//!   not and check that nothing typed there is printed); Enter in that field
//!   continues to the machine page.
//! * `iso::next_detect_board`: a VM that reports the MSI MEG X570 UNIFY's
//!   baseboard identity (only the board's pair of SMBIOS strings) has its own
//!   configuration, `ms-7c35.gnome`, preselected.
//! * `iso::next_detect_generic`: a VM no machine configuration matches has the
//!   edition's generic one preselected, with the reason.
//! * `iso::next_kernel_argument`: `ilex.composition=` typed into GRUB's editor
//!   (twice: the last wins) is honoured as the model decides, and printed on
//!   the serial console in the format the selection trials have always read.
//! * `iso::next_kernel_argument_unknown`: a tag the edition does not offer is
//!   reported with the model's list of offers (the notice's text names both
//!   offered tags and the page's rows are those two) and the page stays usable
//!   on the detected configuration.
//! * `iso::next_network`: the VM's network link is taken down (QMP `set_link`)
//!   before Continue on the welcome page: the network page appears (nothing
//!   reaches the registry, no cable is plugged in, no Wi-Fi adapter), and not
//!   the machine page; the link comes back, the page sees the registry answer
//!   by itself and Enter (Continue has the focus) goes on to the machine page.
//! * `iso::next_account`: the disk and administrator pages typed with the
//!   keyboard on a VM with a disk the installer could use; Enter in the last
//!   field asks the real helper to `Plan` the choices (it resolves the image at
//!   the registry), and the review page reports the plan checked. The password
//!   is on neither the serial console nor in the guest's journal.

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
use ilex_vmctl::qmp::Qmp;
use libtest_mimic::Trial;
use serde_json::json;

use super::{
    Edition, GRUB_COUNTDOWN, Guest, MEMORY, PAGE_TIMEOUT, blank_disk_of, complete_lines, console,
    diagnostics, field, is_marker, iso_path_of, trial, wait_for_text, wait_page, words,
};
use crate::common;
use crate::iso;
use crate::selinux;

/// The fixed public test credential (SMBIOS) that lets the interface print the
/// text of the layout test field (`ilex_installer_flow::markers`).
const ECHO_CREDENTIAL: &str = "ilex.installer.echo-test";
/// What a person might type in the layout test field: their intended
/// password. No trial without the echo credential may see it printed.
const TRY_SECRET: &str = "hunter2tryfield";
/// The tag the kernel argument names in `iso::next_kernel_argument`.
const OVERRIDE_TAG: &str = "ms-7c35.gnome";
/// A tag no edition offers.
const UNKNOWN_TAG: &str = "ms-7c35.kde";
/// The disk's passphrase the account trial types (the US keyboard's).
const PASSPHRASE: &str = crate::installer::apply::encrypted::US.keys;

/// The administrator's password the account trial types.
pub(super) const PASSWORD: &str = "correct horse battery";
/// How long the interface is given to answer one key press.
pub(super) const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn trials() -> Vec<Trial> {
    vec![
        trial("iso::next_welcome", welcome),
        trial("iso::next_detect_board", || {
            detect(
                "iso-next-board",
                true,
                "ms-7c35.gnome",
                "detected",
                ("info", "Recognised_this_computer_as_MSI"),
            )
        }),
        trial("iso::next_detect_generic", || {
            detect(
                "iso-next-generic",
                false,
                "generic.gnome",
                "not-recognised",
                ("info", "No_configuration_is_specific_to_this_computer"),
            )
        }),
        trial("iso::next_kernel_argument", || {
            kernel_argument("iso-next-karg", OVERRIDE_TAG)
        }),
        trial("iso::next_kernel_argument_unknown", || {
            kernel_argument("iso-next-karg-bad", UNKNOWN_TAG)
        }),
        trial("iso::next_account", account),
        trial("iso::next_network", network),
    ]
}

/// A running installer VM and where its evidence goes.
pub(super) struct Booted {
    pub(super) guest: Guest,
    pub(super) artifacts: PathBuf,
    pub(super) iso: PathBuf,
    pub(super) work: PathBuf,
}

/// Boot the ISO under Secure Boot on KVM, as the boot trials do, with the
/// given SMBIOS structures. A USB keyboard is attached when the trial edits
/// GRUB (some OVMF builds drop navigation keys from virtio-keyboard there).
fn boot(
    purpose: &'static str,
    usb_keyboard: bool,
    smbios: Vec<String>,
    disk_size: &str,
) -> Result<Booted> {
    boot_of(Edition::Gnome, purpose, usb_keyboard, smbios, disk_size)
}

/// The same for an edition's ISO.
pub(super) fn boot_of(
    edition: Edition,
    purpose: &'static str,
    usb_keyboard: bool,
    smbios: Vec<String>,
    disk_size: &str,
) -> Result<Booted> {
    let ws = common::workspace()?;
    let iso = iso_path_of(edition)?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let root = ws.state_dir().join("iso-next-fixtures").join(purpose);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let disk = root.join("blank.qcow2");
    blank_disk_of(&disk, disk_size)?;

    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    let mut config = VmConfig::new(purpose, &disk);
    config.snapshot = false;
    config.memory = memory;
    config.cdrom = Some(iso.clone());
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    // Provisioning is required, a qualified host is not (see `boot` above).
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    let provider = LinuxProvider::new(ws.clone())?
        .with_usb_keyboard(usb_keyboard)
        .with_smbios(smbios);
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
pub(super) fn wait_line(
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
        super::ensure_firmware_output(vm)?;
        if started.elapsed() > timeout {
            bail!("no {what} within {timeout:?}; serial: {text}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

pub(super) fn wait_page_marker(
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

pub(super) fn wait_event(
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
pub(super) fn welcome_up(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    welcome_up_of(Edition::Gnome, vm, ctl, serial, artifacts, name)
}

/// The same for an edition's interface.
pub(super) fn welcome_up_of(
    edition: Edition,
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    let page = wait_page_marker(vm, serial, "welcome", PAGE_TIMEOUT)?;
    ensure!(
        field(&page, "edition")? == edition.id()
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
    // The fixed test credential that lets the interface print what is typed in
    // the layout test field: without it nothing of that field is printed.
    let echo = vec![format!(
        "type=11,value=io.systemd.credential:{}=1",
        ECHO_CREDENTIAL
    )];
    let mut booted = boot("iso-next-welcome", false, echo, "1G")?;
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_welcome(vm, &artifacts)
        .and_then(|()| selinux::check_waiting(vm, &artifacts, "next-welcome"));
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
/// the keyboard alone moves on. On the way the layout test field gets what a
/// person might type there, their password: it must never be printed.
pub(super) fn continue_to_machine(ctl: &Vmctl) -> Result<()> {
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    ctl.type_text(TRY_SECRET)?;
    ctl.key("Tab")?;
    ctl.key("Return")
}

/// Nothing typed in the layout test field reached the serial console, and no
/// `event=try` line was printed: without the test credential the field is not
/// even read.
pub(super) fn no_try_text(serial: &Path) -> Result<()> {
    let text = console(serial)?;
    ensure!(
        !text.contains(TRY_SECRET)
            && !complete_lines(&text).any(|l| is_marker(l, "ILEX-INSTALLER", Some("event"))
                && words(l).get("event").is_some_and(|e| e == "try")),
        "the layout test field's text was printed on a boot without the test credential: {text}"
    );
    Ok(())
}

fn detect(
    purpose: &'static str,
    board: bool,
    expected: &str,
    origin: &str,
    notice: (&str, &str),
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
    let mut booted = boot(purpose, false, smbios, "1G")?;
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_machine(vm, &artifacts, purpose, expected, origin, notice)
        .and_then(|()| selinux::check_waiting(vm, &artifacts, &format!("next-{purpose}")));
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
    notice: (&str, &str),
) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    welcome_up(vm, &ctl, &serial, artifacts, name)?;
    continue_to_machine(&ctl)?;
    let machine = wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
    check_machine(vm, &serial, &machine, expected, origin)?;
    check_notice(vm, &serial, notice)?;
    no_try_text(&serial)?;
    wait_page(vm, &ctl, artifacts, name)?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}.png")))?;
    Ok(())
}

/// The notice the machine page shows (`event=notice`), with the severity and
/// the text (spaces are underscores in a marker) it must have.
fn check_notice(vm: &mut Vm, serial: &Path, (severity, text): (&str, &str)) -> Result<()> {
    let notice = wait_event(vm, serial, "notice", EVENT_TIMEOUT, |_| true)?;
    ensure!(
        field(&notice, "severity")? == severity && field(&notice, "text")?.contains(text),
        "the machine page's notice is {notice:?}, expected {severity} with `{text}`"
    );
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
        field(machine, "offered")? == "2"
            && field(machine, "choices")? == "generic.gnome,ms-7c35.gnome",
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
    let mut booted = boot(purpose, true, vec![], "1G")?;
    let artifacts = booted.artifacts.clone();
    let (iso, work) = (booted.iso.clone(), booted.work.clone());
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_kernel_argument(vm, &artifacts, &iso, &work, purpose, tag)
        .and_then(|()| selinux::check_waiting(vm, &artifacts, &format!("next-{purpose}")));
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
        check_notice(
            vm,
            &serial,
            ("info", "The_boot_options_chose_this_configuration"),
        )?;
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
            field(&machine, "origin")? == "bad-override"
                && field(&machine, "selected")? == "generic.gnome",
            "an unknown tag on the page: {machine:?}"
        );
        // The rows shown are the edition's offers, and the notice carries the
        // model's list of them: both tags the edition offers, in its order.
        ensure!(
            field(&machine, "choices")? == "generic.gnome,ms-7c35.gnome",
            "the rows shown are not the edition's offers: {machine:?}"
        );
        check_notice(
            vm,
            &serial,
            (
                "error",
                &format!("unknown_composition_'{tag}';_offered:_generic.gnome,_ms-7c35.gnome"),
            ),
        )?;
    }
    no_try_text(&serial)?;
    wait_page(vm, &ctl, artifacts, name)?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}.png")))?;
    Ok(())
}

/// The network link of the VM's NIC, as a cable plugged in or pulled out.
fn link(vm: &Vm, up: bool) -> Result<()> {
    Qmp::connect(&vm.info.qmp_sock)?.execute("set_link", json!({"name": "internet", "up": up}))?;
    Ok(())
}

/// The network page, by taking the link down and bringing it back.
fn network() -> Result<()> {
    let mut booted = boot("iso-next-network", false, vec![], "1G")?;
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_network(vm, &artifacts)
        .and_then(|()| selinux::check_waiting(vm, &artifacts, "next-network"));
    if outcome.is_err() {
        diagnostics(vm, &artifacts, "network");
    }
    outcome
}

fn observe_network(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    welcome_up(vm, &ctl, &serial, artifacts, "network")?;
    // The cable is pulled out; NetworkManager takes a moment to see it go.
    link(vm, false)?;
    std::thread::sleep(Duration::from_secs(8));
    continue_to_machine(&ctl)?;
    let network = wait_page_marker(vm, &serial, "network", EVENT_TIMEOUT)?;
    ensure!(
        field(&network, "check")? == "offline"
            && field(&network, "wifi")? == "false"
            && field(&network, "networks")? == "0",
        "the network page of a VM without a link: {network:?}"
    );
    let look = wait_event(vm, &serial, "network", EVENT_TIMEOUT, |_| true)?;
    ensure!(
        field(&look, "result")? == "offline" && field(&look, "wired")? == "unplugged",
        "the look at a VM without a link: {look:?}"
    );
    wait_page(vm, &ctl, artifacts, "network-offline")?;
    ctl.save_screenshot(&artifacts.join("next-network-offline.png"))?;
    // Enter does not go on, and the machine page is not shown.
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(4));
    ensure!(
        !console(&serial)?.contains("ILEX-INSTALLER page=machine"),
        "the machine page was shown without a network"
    );
    // The cable is plugged in again: the page looks every few seconds, and the
    // registry answers once NetworkManager has an address.
    link(vm, true)?;
    let online = wait_event(vm, &serial, "network", Duration::from_secs(120), |w| {
        w.get("result").is_some_and(|r| r == "online")
    })?;
    ensure!(
        field(&online, "wired")? == "up",
        "the look at a VM with its link back: {online:?}"
    );
    wait_page(vm, &ctl, artifacts, "network-online")?;
    ctl.save_screenshot(&artifacts.join("next-network-online.png"))?;
    // Continue has the focus: Enter goes on.
    ctl.key("Return")?;
    wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
    wait_page(vm, &ctl, artifacts, "network-machine")?;
    ctl.save_screenshot(&artifacts.join("next-network-machine.png"))?;
    no_try_text(&serial)?;
    Ok(())
}

/// The administrator page by keyboard, against the real helper: the name and
/// the password twice, Enter in the last field, and the helper's `Plan` (the
/// image's manifest is read from the registry, so the guest needs `ghcr.io`).
/// The VM has a 40 GiB disk, which the installer can use.
fn account() -> Result<()> {
    let mut booted = boot("iso-next-account", false, vec![], "40G")?;
    let artifacts = booted.artifacts.clone();
    let vm = booted.guest.vm.running_mut()?;
    let outcome = observe_account(vm, &artifacts)
        .and_then(|()| selinux::check_waiting(vm, &artifacts, "next-account"));
    if outcome.is_err() {
        diagnostics(vm, &artifacts, "account");
    }
    outcome
}

fn observe_account(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    welcome_up(vm, &ctl, &serial, artifacts, "account")?;
    continue_to_machine(&ctl)?;
    wait_page_marker(vm, &serial, "machine", EVENT_TIMEOUT)?;
    // The page's marker comes when its window is mapped; Continue gets the
    // focus when the page has finished sliding in (`shown`). A press sent
    // before then goes to the previous page's field, so wait for the settled
    // page like the other trials do.
    wait_page(vm, &ctl, artifacts, "account-machine")?;
    // The machine page has Continue focused: Enter presses it. The disk page
    // comes next, with the VM's one disk offered, and has Continue focused too.
    ctl.key("Return")?;
    let disk = wait_page_marker(vm, &serial, "disk", EVENT_TIMEOUT)?;
    ensure!(
        field(&disk, "offered")? == "1" && field(&disk, "unavailable")? == "0",
        "the disk page of a VM with one blank disk: {disk:?}"
    );
    wait_page(vm, &ctl, artifacts, "account-disk")?;
    ctl.save_screenshot(&artifacts.join("next-account-disk.png"))?;
    ctl.key("Return")?;
    // The encryption page: encryption is on and Continue waits for a
    // passphrase typed twice (the passphrase field has the focus; a password
    // row has two stops, its text and the button that shows what was typed).
    let encryption = wait_page_marker(vm, &serial, "encryption", EVENT_TIMEOUT)?;
    ensure!(
        field(&encryption, "encryption")? == "on" && field(&encryption, "keyboard")? == "us",
        "the encryption page: {encryption:?}"
    );
    wait_page(vm, &ctl, artifacts, "account-encryption")?;
    ctl.save_screenshot(&artifacts.join("next-account-encryption-empty.png"))?;
    // Enter in the empty field does not continue.
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(2));
    ensure!(
        !console(&serial)?.contains("ILEX-INSTALLER page=account"),
        "Enter on an empty passphrase continued"
    );
    ctl.type_text(PASSPHRASE)?;
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    // A confirmation that is not the passphrase does not continue either.
    ctl.type_text("not the passphrase")?;
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(2));
    ensure!(
        !console(&serial)?.contains("ILEX-INSTALLER page=account"),
        "Enter on a mismatched confirmation continued"
    );
    wait_page(vm, &ctl, artifacts, "account-encryption-mismatch")?;
    ctl.save_screenshot(&artifacts.join("next-account-encryption-mismatch.png"))?;
    // Select all and retype it right.
    ctl.key("ctrl+a")?;
    ctl.type_text(PASSPHRASE)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.save_screenshot(&artifacts.join("next-account-encryption-filled.png"))?;
    ctl.key("Return")?;
    wait_page_marker(vm, &serial, "account", EVENT_TIMEOUT)?;
    wait_page(vm, &ctl, artifacts, "account")?;
    ctl.save_screenshot(&artifacts.join("next-account-empty.png"))?;

    // The name has the focus; Tab moves down the fields. A password row has
    // two stops: its text and the button that shows what was typed.
    ctl.type_text("ilexmaint")?;
    ctl.key("Tab")?;
    ctl.type_text(PASSWORD)?;
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    ctl.type_text(PASSWORD)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.save_screenshot(&artifacts.join("next-account-filled.png"))?;
    // Enter in the last field continues to the daily user page, where nothing
    // is chosen and Continue has the focus: Enter then has the helper plan.
    ctl.key("Return")?;
    let profile = wait_page_marker(vm, &serial, "profile", EVENT_TIMEOUT)?;
    ensure!(
        field(&profile, "daily")? == "none",
        "the daily user page: {profile:?}"
    );
    wait_page(vm, &ctl, artifacts, "account-profile")?;
    ctl.save_screenshot(&artifacts.join("next-account-profile.png"))?;
    ctl.key("Return")?;
    let plan = wait_event(vm, &serial, "plan", Duration::from_secs(240), |_| true)?;
    ensure!(
        field(&plan, "result")? == "ok",
        "the helper did not plan the choices: {plan:?}"
    );
    let review = wait_page_marker(vm, &serial, "review", EVENT_TIMEOUT)?;
    ensure!(
        field(&review, "check")? == "ok",
        "the review page does not report the plan checked: {review:?}"
    );
    wait_page(vm, &ctl, artifacts, "account-review")?;
    ctl.save_screenshot(&artifacts.join("next-account-review.png"))?;
    // The password is nowhere on the serial console...
    let text = console(&serial)?;
    ensure!(
        !text.contains(PASSWORD) && !text.contains(PASSPHRASE) && !text.contains("Holly"),
        "the password or the passphrase reached the serial console"
    );
    no_try_text(&serial)?;
    // ...and nowhere in the guest's journal, where the interface's stdout (the
    // same marker lines) and everything else the boot logged is. The patterns
    // are written so that the command line that searches does not match
    // itself. The journal must hold the interface's own lines, or the count
    // would prove nothing.
    vm.wait_ssh(Duration::from_secs(180))
        .context("sshd did not start to read the journal")?;
    let count = vm.ssh_ok(
        "journalctl -b --no-pager -o cat > /tmp/journal.txt; \
         echo markers=$(grep -c -e '^ILEX-INSTALLER page=review edition=gnome check=ok' /tmp/journal.txt); \
         echo password=$(grep -c -e '[c]orrect horse battery' -e '[h]orse batt' /tmp/journal.txt); \
         echo passphrase=$(grep -c -E -e '[P]lant-the-' -e '[b]erries' -e '[n]ot the passphrase' /tmp/journal.txt); \
         echo tried=$(grep -c -e '[h]unter2tryfield' /tmp/journal.txt); \
         echo focus=$(grep -c -e 'GtkText - [d]id not receive a focus-out' /tmp/journal.txt); \
         rm -f /tmp/journal.txt",
    )?;
    ensure!(
        !count.contains("markers=0")
            && count.contains("password=0")
            && count.contains("passphrase=0")
            && count.contains("tried=0")
            && count.contains("focus=0"),
        "the journal holds the review page's marker, no password or test-field text and no \
         GTK focus warning (a focused field made insensitive): {count}"
    );
    Ok(())
}
