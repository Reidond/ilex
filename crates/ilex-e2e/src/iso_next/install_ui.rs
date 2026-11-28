//! The installer's own interface installing for real, on the real ISO: the
//! disk, review, installing, done and problem pages, by key input only
//! (QMP, through `ilex-vmctl`), against the real helper and the real registry
//! (the guest needs `ghcr.io`). Every page is read off the serial console
//! (`ILEX-INSTALLER page=…`, `event=…`) and a screenshot of it is kept.
//!
//! * `iso::next_keyboard_install`: a complete interactive installation by
//!   keyboard: the welcome, machine, disk, encryption (the passphrase typed
//!   twice) and administrator pages, the review,
//!   the erase confirmation (cancelled once with Escape, then accepted), the
//!   installing page to the end, the done page's Power off; then the disk that
//!   was installed boots on its own, asks for the passphrase at its boot
//!   prompt (typed through QMP) and every `installer::install` assertion holds
//!   (the digest the plan pinned, the origin that enforces the signature
//!   policy, the administrator's password and `sudo`, the answers applied, the
//!   system running, GDM up), with the root on the mapped LUKS2 volume. Neither
//!   the password nor the passphrase is on the serial console or in the
//!   guest's journal.
//! * `iso::next_reattach`: the interface is killed while the helper's `Apply` is
//!   still checking (a fixed test credential holds it for a minute after it took
//!   its lock, as a slow registry would: `State` then says `starting` over the
//!   previous record). First that `Apply` is refused: the target disk's partition
//!   table is changed from the guest in the window, so the helper refuses it
//!   (`DiskChanged`, recorded in `State` with the plan's id) and writes nothing; the
//!   restarted interface, which found the installing page with every stage
//!   waiting, goes back to the welcome page with the "did not start" sentence (not
//!   the problem page, and never an earlier attempt's outcome), and its next
//!   walk-through is confirmed. Then the interface is killed in that second
//!   window, and the compositor in the middle of the image stage: each time the
//!   session comes back on the installing page (not on the welcome page; the first
//!   time with every stage waiting), the installation goes on undisturbed and the
//!   done page follows. (The session unit allows three starts in five minutes: the
//!   start-limit counter is reset before each kill.)
//! * `iso::next_problem`: the image stage fails (a fixed test credential makes
//!   it fail before `bootc` runs, as a pull that fails does) and the problem
//!   page says so; the log is saved to a USB stick by keyboard and the file is
//!   read back from the stick (no password in it); every other device is
//!   refused (the installation's disk, the medium the installer started from,
//!   a fixed disk, a stick with nothing the helper can write to, a name that is
//!   not a device, another user); Try again returns to the review and the
//!   install is confirmed and fails again; and a helper killed during an
//!   installation shows the problem page from what its successor recorded.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::cmd;
use ilex_devkit::core::{Capability, Requirement, SecurityLaunch, VmProvider};
use ilex_devkit::installer_model::plan::ErrorCode;
use ilex_devkit::installer_model::probe::Probe;
use ilex_devkit::installer_model::recovery;
use ilex_devkit::installer_model::state::{Outcome, State};
use ilex_devkit::provider::LinuxProvider;
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::Vmctl;
use libtest_mimic::Trial;

use super::security;
use super::ui::{EVENT_TIMEOUT, continue_to_machine, no_try_text, welcome_up};
use super::{
    Guest, KILL_COMPOSITOR, KILL_INSTALLER, PAGE_TIMEOUT, complete_lines, console, diagnostics,
    field, is_marker, iso_path, trial, wait_firmware_output, wait_page, words,
};
use crate::common;
use crate::installer::apply::encrypted::{self, US};
use crate::installer::apply::{
    ADMIN, Expected, PASSWORD, busctl, check_installed, document, gdm_screenshot, refusal, ssh,
};
use crate::selinux;

/// The target's virtual size: what the helper's minimum asks for, with room.
const TARGET_BYTES: u64 = 41 << 30;
/// The pull and everything after it, on a contended runner.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(40 * 60);
/// The ISO's VM: the pull streams into the target and the helper, bootc and
/// the page cache share the rest.
const MEMORY: &str = "6G";
const MARKER: &str = "ILEX-INSTALLER";

pub(super) fn trials() -> Vec<Trial> {
    vec![
        trial("iso::next_keyboard_install", keyboard_install),
        trial("iso::next_reattach", reattach),
        trial("iso::next_problem", problem),
    ]
}

// --- the machine ---------------------------------------------------------------------

/// What a trial boots.
struct Setup<'a> {
    /// Names the fixtures and the VM (kept short: it names Unix sockets).
    name: &'a str,
    purpose: &'static str,
    /// The test fault (`ilex.installer.fault`): `image` fails the image stage.
    fault: Option<&'a str>,
    /// The ISO as a removable USB stick (the live medium is then `removable`).
    usb_medium: bool,
    /// Make a FAT stick, a stick with nothing on it and a fixed spare disk.
    spares: bool,
}

struct Rig {
    guest: Guest,
    artifacts: PathBuf,
    root: PathBuf,
    target: PathBuf,
    /// The FAT stick and the empty stick, when there are any.
    sticks: Vec<PathBuf>,
}

fn boot(setup: &Setup<'_>) -> Result<Rig> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let root = ws.state_dir().join("iso-next-fixtures").join(setup.name);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let target = root.join("target.raw");
    fs::File::create(&target)?.set_len(TARGET_BYTES)?;
    let mut sticks = Vec::new();
    let mut spare = None;
    if setup.spares {
        for tool in ["mkfs.fat", "mcopy", "mdir", "sfdisk", "tar"] {
            cmd::require(tool)?;
        }
        let fat = root.join("stick-fat.img");
        fat_stick(&fat)?;
        let blank = root.join("stick-blank.img");
        fs::File::create(&blank)?.set_len(64 << 20)?;
        sticks = vec![fat, blank];
        let disk = root.join("spare.raw");
        fs::File::create(&disk)?.set_len(2 << 30)?;
        spare = Some(disk);
    }
    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    let mut config = VmConfig::new(setup.purpose, &target);
    config.snapshot = false;
    config.memory = memory;
    config.cdrom = Some(iso);
    config.extra_disks = spare.into_iter().collect();
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    // A fixed test credential the helper imports: it can only make the
    // installer fail.
    let smbios: Vec<String> = setup
        .fault
        .map(|fault| format!("type=11,value=io.systemd.credential:ilex.installer.fault={fault}"))
        .into_iter()
        .collect();
    let provider = LinuxProvider::new(ws.clone())?
        .with_disk_serials(true)
        .with_usb_medium(setup.usb_medium)
        .with_usb_sticks(sticks.clone())
        .with_smbios(smbios);
    let caps = provider.probe(&spec)?;
    caps.require_provisioning(Capability::FirmwareEnforcement)?;
    let vm = provider.create(spec)?;
    let scratch = root.join("vm");
    fs::create_dir_all(&scratch)?;
    let mut guest = Guest {
        provider,
        vm,
        root: scratch,
    };
    guest.provider.start(&mut guest.vm)?;
    let running = guest.vm.running_mut()?;
    ensure!(
        running.info.secure_boot,
        "the VM was not started with Secure Boot"
    );
    wait_firmware_output(running)?;
    Ok(Rig {
        guest,
        artifacts,
        root,
        target,
        sticks,
    })
}

/// A 64 MiB stick with a table and one FAT32 partition, as a stick from a shop
/// is (an image with no table has no partition for the helper to write to).
fn fat_stick(path: &Path) -> Result<()> {
    fs::File::create(path)?.set_len(64 << 20)?;
    let mut sfdisk = Command::new("sfdisk");
    sfdisk
        .arg("--quiet")
        .arg(path)
        .stdin(std::process::Stdio::piped());
    let mut child = sfdisk.spawn().context("starting sfdisk")?;
    std::io::Write::write_all(
        child.stdin.as_mut().context("sfdisk's input")?,
        b"label: dos\nstart=2048, type=c\n",
    )?;
    ensure!(child.wait()?.success(), "sfdisk could not write the table");
    // The partition starts at sector 2048: 1 MiB.
    cmd::run(
        Command::new("mkfs.fat")
            .args(["-F", "32", "-n", "ILEXLOG", "--offset", "2048"])
            .arg(path),
    )?;
    Ok(())
}

/// The names of the files on the stick's FAT partition.
fn stick_files(path: &Path) -> Result<Vec<String>> {
    let out = Command::new("mdir")
        .args(["-b", "-i"])
        .arg(format!("{}@@1M", path.display()))
        .arg("::")
        .output()
        .context("running mdir")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim().trim_start_matches("::/").to_owned())
        .filter(|l| !l.is_empty())
        .collect())
}

// --- reading the serial console ---------------------------------------------------------

/// The `n`th complete installer marker (1-based) that `matches`, waiting for it.
/// Fails at once when the VM exited or the session gave up.
fn nth_marker(
    vm: &mut Vm,
    serial: &Path,
    what: &str,
    n: usize,
    timeout: Duration,
    matches: impl Fn(&BTreeMap<String, String>) -> bool,
) -> Result<BTreeMap<String, String>> {
    let started = Instant::now();
    loop {
        let text = console(serial)?;
        let found: Vec<_> = complete_lines(&text)
            .filter(|l| is_marker(l, MARKER, None))
            .map(words)
            .filter(|w| matches(w))
            .collect();
        if let Some(line) = found.get(n - 1) {
            return Ok(line.clone());
        }
        if complete_lines(&text).any(|l| is_marker(l, "ILEX-INSTALLER-FALLBACK", None)) {
            bail!("the session gave up while waiting for {what}; serial: {text}");
        }
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} while waiting for {what}; serial: {text}");
        }
        if started.elapsed() > timeout {
            bail!("no {what} (the {n}th) within {timeout:?}; serial: {text}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn is_page(page: &'static str) -> impl Fn(&BTreeMap<String, String>) -> bool {
    move |w| w.get("page").is_some_and(|p| p == page)
}

fn is_event(
    event: &'static str,
    key: &'static str,
    value: &'static str,
) -> impl Fn(&BTreeMap<String, String>) -> bool {
    move |w| {
        w.get("event").is_some_and(|e| e == event)
            && (key.is_empty() || w.get(key).is_some_and(|v| v == value))
    }
}

/// How many markers match.
fn count(serial: &Path, matches: impl Fn(&BTreeMap<String, String>) -> bool) -> Result<usize> {
    let text = console(serial)?;
    Ok(complete_lines(&text)
        .filter(|l| is_marker(l, MARKER, None))
        .map(words)
        .filter(|w| matches(w))
        .count())
}

// --- the pages --------------------------------------------------------------------------

/// The pages from the welcome page to the review page, by keyboard: Continue
/// is focused on the machine and disk pages (Enter presses it), the name has
/// the focus on the administrator page, and Enter in the last field asks the
/// helper to check the choices. `reviews` is how many review pages came
/// before (a later one is another check).
fn to_review(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
    unavailable: &str,
    encrypted: bool,
) -> Result<()> {
    welcome_up(vm, ctl, serial, artifacts, name)?;
    from_welcome(vm, ctl, serial, artifacts, name, unavailable, encrypted)
}

/// The same from a welcome page that is up (a walk-through's second round: the
/// pages' markers that came before are not the ones waited for).
fn from_welcome(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
    unavailable: &str,
    encrypted: bool,
) -> Result<()> {
    let before = |page: &'static str| count(serial, is_page(page));
    let (machines, disks, encryptions, accounts, reviews) = (
        before("machine")?,
        before("disk")?,
        before("encryption")?,
        before("account")?,
        before("review")?,
    );
    continue_to_machine(ctl)?;
    nth_marker(
        vm,
        serial,
        "the machine page",
        machines + 1,
        EVENT_TIMEOUT,
        is_page("machine"),
    )?;
    wait_page(vm, ctl, artifacts, &format!("{name}-machine"))?;
    ctl.key("Return")?;
    let disk = nth_marker(
        vm,
        serial,
        "the disk page",
        disks + 1,
        EVENT_TIMEOUT,
        is_page("disk"),
    )?;
    ensure!(
        field(&disk, "offered")? == "1" && field(&disk, "unavailable")? == unavailable,
        "the disk page of a VM with one disk that can be installed to and {unavailable} that \
         cannot: {disk:?}"
    );
    ensure!(
        field(&disk, "disk")? == "virtio-ilex-disk-0",
        "the disk the page chose: {disk:?}"
    );
    wait_page(vm, ctl, artifacts, &format!("{name}-disk"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-disk.png")))?;
    ctl.key("Return")?;
    // The encryption page: on, with the layout the passphrase is typed in. The
    // passphrase field has the focus; a password row has two stops (its text
    // and the button that shows what was typed).
    let encryption = nth_marker(
        vm,
        serial,
        "the encryption page",
        encryptions + 1,
        EVENT_TIMEOUT,
        is_page("encryption"),
    )?;
    ensure!(
        field(&encryption, "encryption")? == "on" && field(&encryption, "keyboard")? == "us",
        "the encryption page: {encryption:?}"
    );
    wait_page(vm, ctl, artifacts, &format!("{name}-encryption"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-encryption.png")))?;
    if encrypted {
        ctl.type_text(US.keys)?;
        ctl.key("Tab")?;
        ctl.key("Tab")?;
        ctl.type_text(US.keys)?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.key("Return")?;
    } else {
        // The trials that are not about the encryption (they would stop at the
        // recovery key page it brings) turn it off, as a person does: from the
        // passphrase back to the switch (the security chip's row has nothing to
        // focus on a computer that cannot use it), Space, then Continue.
        let off = count(serial, is_event("encryption", "encryption", "off"))?;
        ctl.key("shift+Tab")?;
        ctl.key("space")?;
        nth_marker(
            vm,
            serial,
            "encryption turned off",
            off + 1,
            EVENT_TIMEOUT,
            is_event("encryption", "encryption", "off"),
        )?;
        std::thread::sleep(Duration::from_secs(1));
        ctl.save_screenshot(&artifacts.join(format!("next-{name}-encryption-off.png")))?;
        // Continue is the button at the bottom right of every page (the focus
        // is nowhere after the passphrase fields went away, and Tab from
        // nowhere starts at the header's Back button).
        // The pointer is moved in first: a surface that appeared since the last
        // click (the interface was restarted) gets no `enter` for a pointer that
        // did not move.
        ctl.move_to(1000, 600)?;
        std::thread::sleep(Duration::from_millis(500));
        ctl.click(1226, 770, ilex_vmctl::Button::Left, 1)?;
    }
    nth_marker(
        vm,
        serial,
        "the administrator page",
        accounts + 1,
        EVENT_TIMEOUT,
        is_page("account"),
    )?;
    wait_page(vm, ctl, artifacts, &format!("{name}-account"))?;
    // The name has the focus. A password row has two stops: its text and the
    // button that shows what was typed.
    ctl.type_text(ADMIN)?;
    ctl.key("Tab")?;
    ctl.type_text(PASSWORD)?;
    ctl.key("Tab")?;
    ctl.key("Tab")?;
    ctl.type_text(PASSWORD)?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    let review = nth_marker(
        vm,
        serial,
        "the review page",
        reviews + 1,
        Duration::from_secs(240),
        is_page("review"),
    )?;
    ensure!(
        field(&review, "check")? == "ok",
        "the plan was not checked: {review:?}"
    );
    wait_page(vm, ctl, artifacts, &format!("{name}-review"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-review.png")))?;
    Ok(())
}

/// The destructive button (it has the focus on the review page) and the erase
/// dialog: Enter opens it, Tab moves from the safe answer to the other, Enter
/// takes it.
fn confirm_erase(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    confirm_erase_with(vm, ctl, serial, artifacts, name, true)
}

/// The same; `wait_for_begin` false returns as soon as the dialog was accepted
/// (the helper may be holding `Apply`).
fn confirm_erase_with(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
    wait_for_begin: bool,
) -> Result<()> {
    let shown = |serial: &Path| count(serial, is_event("confirm", "state", "shown"));
    let accepted = count(serial, is_event("confirm", "state", "accepted"))?;
    let begun = count(serial, is_event("install", "result", "started"))?;
    let cancelled = count(serial, is_event("confirm", "state", "cancelled"))?;
    let mut opened = shown(serial)?;
    for attempt in 1..=3 {
        ctl.key("Return")?;
        opened += 1;
        nth_marker(
            vm,
            serial,
            "the erase dialog",
            opened,
            EVENT_TIMEOUT,
            is_event("confirm", "state", "shown"),
        )?;
        if attempt == 1 {
            std::thread::sleep(Duration::from_secs(2));
            ctl.save_screenshot(&artifacts.join(format!("next-{name}-erase-dialog.png")))?;
        }
        for _ in 0..attempt {
            ctl.key("Tab")?;
        }
        ctl.key("Return")?;
        let started = Instant::now();
        let confirmed = loop {
            if count(serial, is_event("confirm", "state", "accepted"))? > accepted {
                break true;
            }
            if count(serial, is_event("confirm", "state", "cancelled"))? >= cancelled + attempt {
                break false;
            }
            if let Some(status) = vm.exited()? {
                bail!("the VM exited with {status} at the erase dialog");
            }
            ensure!(
                started.elapsed() < Duration::from_secs(60),
                "the erase dialog gave no answer"
            );
            std::thread::sleep(Duration::from_secs(1));
        };
        if confirmed && !wait_for_begin {
            return Ok(());
        }
        if confirmed {
            // The helper took the installation: `Apply` answered. (Nothing
            // else is waited for in minutes if it did not.)
            nth_marker(
                vm,
                serial,
                "the helper beginning the installation",
                begun + 1,
                Duration::from_secs(180),
                is_event("install", "result", "started"),
            )
            .with_context(|| {
                format!(
                    "the erase was confirmed and the installation did not begin; serial markers: {}",
                    console(serial)
                        .map(|t| t
                            .lines()
                            .filter(|l| l.starts_with("ILEX-INSTALLER event=install"))
                            .collect::<Vec<_>>()
                            .join(" | "))
                        .unwrap_or_default()
                )
            })?;
            return Ok(());
        }
    }
    bail!("the erase could not be confirmed with the keyboard")
}

/// Everything the helper says about the installation, from the guest.
fn helper_state(vm: &Vm) -> Result<State> {
    Ok(serde_json::from_value(document(
        &ssh(vm, &busctl("ilex-installer", "State", &[]))?,
        "State",
    )?)?)
}

/// Ask the helper (from the guest) until its `State` satisfies `wanted`.
fn wait_state(
    vm: &Vm,
    what: &str,
    timeout: Duration,
    wanted: impl Fn(&State) -> bool,
) -> Result<State> {
    let started = Instant::now();
    loop {
        let state = helper_state(vm)?;
        if wanted(&state) {
            return Ok(state);
        }
        ensure!(
            started.elapsed() < timeout,
            "the helper never said {what} within {timeout:?}: {state:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn helper_probe(vm: &Vm) -> Result<Probe> {
    Ok(serde_json::from_value(document(
        &ssh(vm, &busctl("ilex-installer", "Probe", &[]))?,
        "Probe",
    )?)?)
}

/// The password is nowhere on the serial console or in the guest's journal.
fn no_password(vm: &mut Vm, serial: &Path, key: Option<&str>) -> Result<()> {
    let text = console(serial)?;
    ensure!(
        !text.contains(PASSWORD)
            && !text.contains("orrect horse:")
            && !text.contains(US.passphrase)
            && !text.contains("Holly"),
        "the password or the passphrase reached the serial console"
    );
    if let Some(key) = key {
        // Not a group of it either: the page's markers name the groups it asks
        // for by number.
        ensure!(
            recovery::groups(key).iter().all(|g| !text.contains(g)),
            "a group of the recovery key reached the serial console"
        );
    }
    no_try_text(serial)?;
    vm.wait_ssh(Duration::from_secs(180))
        .context("sshd did not start to read the journal")?;
    // Patterns written so that the command line that searches does not match
    // itself.
    let count = vm.ssh_ok(
        "journalctl -b --no-pager -o cat > /tmp/journal.txt; \
         echo password=$(grep -c -E -e '[C]orrect horse:' -e '[s]taple' /tmp/journal.txt); \
         echo passphrase=$(grep -c -E -e '[P]lant-the-' -e '[b]erries' /tmp/journal.txt); \
         echo markers=$(grep -c -e '^ILEX-INSTALLER event=stage' /tmp/journal.txt); \
         rm -f /tmp/journal.txt",
    )?;
    ensure!(
        count.contains("password=0")
            && count.contains("passphrase=0")
            && !count.contains("markers=0"),
        "the journal holds the interface's markers and no password or passphrase: {count}"
    );
    if let Some(key) = key {
        let found = vm.ssh_ok(&format!(
            "journalctl -b --no-pager -o cat | grep -cF -e \"$(printf %s {} | base64 -d)\" || true; \
             grep -rlaF -e \"$(printf %s {} | base64 -d)\" /run/ilex-installer 2>/dev/null | wc -l",
            qemu::base64(key.as_bytes()),
            qemu::base64(key.as_bytes()),
        ))?;
        ensure!(
            found.split_whitespace().all(|n| n == "0"),
            "the recovery key is in the journal or the helper's run directory: {found}"
        );
    }
    Ok(())
}

/// The recovery key page, driven the way a person who cannot be shown the key
/// again drives it: the trial takes the key from the helper while the interface
/// is stopped (the helper hands it over once, so this window finds it taken: the
/// page that a restarted window shows), then types two groups wrongly (the helper
/// refuses them and the fields empty) and rightly. The key is in the trial's
/// memory only; no screenshot is kept of a page with a group typed on it.
fn recovery_page(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<String> {
    // Stopped before the helper has made the key (it does after the volume is
    // open, some seconds after `Apply`), continued once the trial has the key.
    let key = security::take_key(vm).context("the trial taking the key")?;
    vm.ssh_ok("pkill -CONT -x ilex-installer")?;
    let page = nth_marker(
        vm,
        serial,
        "the recovery page",
        1,
        EVENT_TIMEOUT,
        is_page("recovery"),
    )?;
    ensure!(
        field(&page, "key")? == "gone",
        "the recovery page of an interface that was not given the key: {page:?}"
    );
    let asked: Vec<usize> = field(&page, "asked")?
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .context("the groups the page asks for")?;
    ensure!(
        asked.len() == 2 && asked == recovery::asked(&asked_plan(vm)?),
        "the groups asked for: {asked:?}"
    );
    wait_page(vm, ctl, artifacts, &format!("{name}-recovery"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-recovery.png")))?;
    let groups = recovery::groups(&key);
    let refused = count(serial, is_event("recovery", "state", "refused"))?;
    // The right shape, the wrong letters: the helper refuses.
    let reversed = |n: usize| groups[n - 1].chars().rev().collect::<String>();
    ctl.type_text(&reversed(asked[0]))?;
    ctl.key("Tab")?;
    ctl.type_text(&reversed(asked[1]))?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the helper's refusal",
        refused + 1,
        EVENT_TIMEOUT,
        is_event("recovery", "state", "refused"),
    )?;
    std::thread::sleep(Duration::from_secs(2));
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-recovery-refused.png")))?;
    // The fields are empty and the focus is on the second: back to the first.
    ctl.key("shift+Tab")?;
    ctl.type_text(groups[asked[0] - 1])?;
    ctl.key("Tab")?;
    ctl.type_text(groups[asked[1] - 1])?;
    std::thread::sleep(Duration::from_secs(1));
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the confirmation",
        1,
        EVENT_TIMEOUT,
        is_event("recovery", "state", "confirmed"),
    )?;
    Ok(key)
}

/// The plan the helper is installing (the groups asked for follow its id).
fn asked_plan(vm: &Vm) -> Result<String> {
    helper_state(vm)?
        .applied
        .context("the helper has no installation")
}

/// Wait for the installation to end and the done page: the stages' markers say
/// how it went. On the way, once the image is being written, the screen is saved.
/// With `recovery` the installation has a recovery key, which is taken and
/// confirmed on the way ([`recovery_page`]); it is returned.
fn wait_done(
    vm: &mut Vm,
    ctl: &Vmctl,
    serial: &Path,
    artifacts: &Path,
    name: &str,
    recovery: bool,
) -> Result<Option<String>> {
    nth_marker(
        vm,
        serial,
        "the installing page",
        1,
        EVENT_TIMEOUT,
        is_page("installing"),
    )?;
    std::thread::sleep(Duration::from_secs(3));
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-installing.png")))?;
    let key = if recovery {
        // The interface is stopped while the key is made: the helper makes it
        // some seconds after `Apply`, and this window would take it first.
        vm.ssh_ok("pkill -STOP -x ilex-installer")?;
        Some(recovery_page(vm, ctl, serial, artifacts, name)?)
    } else {
        None
    };
    nth_marker(
        vm,
        serial,
        "the image stage",
        1,
        Duration::from_secs(600),
        |w| {
            is_event("stage", "stage", "image")(w)
                && w.get("status").is_some_and(|s| s == "running")
        },
    )?;
    std::thread::sleep(Duration::from_secs(45));
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-installing-image.png")))?;
    let outcome = nth_marker(
        vm,
        serial,
        "the end of the installation",
        1,
        INSTALL_TIMEOUT,
        is_event("outcome", "", ""),
    )?;
    ensure!(
        field(&outcome, "result")? == "success",
        "the installation did not succeed: {outcome:?}; serial: {}",
        console(serial)?
    );
    nth_marker(
        vm,
        serial,
        "the done page",
        1,
        EVENT_TIMEOUT,
        is_page("done"),
    )?;
    wait_page(vm, ctl, artifacts, &format!("{name}-done"))?;
    ctl.save_screenshot(&artifacts.join(format!("next-{name}-done.png")))?;
    Ok(key)
}

// --- iso::next_keyboard_install ----------------------------------------------------------

fn keyboard_install() -> Result<()> {
    let mut rig = boot(&Setup {
        name: "iso-next-keys",
        purpose: "iso-next-keys",
        fault: None,
        usb_medium: false,
        spares: false,
    })?;
    let artifacts = rig.artifacts.clone();
    let target = rig.target.clone();
    let vm = rig.guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let started = Instant::now();
    let outcome = install_by_keys(vm, &serial, &artifacts);
    let (digest, key) = match outcome {
        Ok(done) => done,
        Err(error) => {
            diagnostics(vm, &artifacts, "keyboard-install");
            return Err(error);
        }
    };
    println!(
        "iso::next keyboard install: installed and powered off in {:?}",
        started.elapsed()
    );
    // The machine is off; the disk it installed boots on its own.
    drop(rig);
    let ws = common::workspace()?;
    let mut config = VmConfig::new("iso-next-keys-t", &target);
    config.snapshot = true;
    config.memory = "4G".into();
    let mut installed = Vm::boot(&ws, &config)?;
    // The disk is encrypted: the passphrase is typed at its boot prompt.
    encrypted::unlock(&mut installed, US.keys, &artifacts, "next-keyboard-install")
        .context("unlocking the installed disk at its prompt")?;
    installed
        .wait_ssh(PAGE_TIMEOUT)
        .context("the unlocked disk did not come up")?;
    let save = |file: &str, text: &str| {
        let _ = qemu::write_artifact(&artifacts, &format!("next-keyboard-install-{file}"), text);
    };
    check_installed(
        &save,
        &installed,
        &Expected {
            digest: &digest,
            target: "ghcr.io/reidond/ilex:generic.gnome",
            host: "ilex",
            zone: "Etc/UTC",
            locale: "en_US.UTF-8",
            keymap: "us",
            layout: "us",
            admin: ADMIN,
            password: PASSWORD,
            stamp: &[],
        },
    )?;
    // A recovery key was made and confirmed; the computer cannot use the chip
    // (Secure Boot is off here), so the disk asked for the passphrase.
    encrypted::booted_encrypted_with_key(&installed, None, &US, &save)?;
    // The key opens the volume, and is nowhere on the installed system.
    let found = installed.ssh_ok(&format!(
        "printf %s {} | base64 -d | cryptsetup open --test-passphrase --key-file=- /dev/disk/by-partlabel/ilex-root; echo key_rc=$?; \
         echo leaks=$(grep -rlaF -e \"$(printf %s {} | base64 -d)\" /var/log /etc /var/lib/ilex 2>/dev/null | wc -l)",
        qemu::base64(key.as_bytes()),
        qemu::base64(key.as_bytes()),
    ))?;
    ensure!(
        found.contains("key_rc=0") && found.contains("leaks=0"),
        "the recovery key on the installed system: {found}"
    );
    gdm_screenshot(
        &mut installed,
        &artifacts.join("next-keyboard-install-gdm.png"),
    )?;
    Ok(())
}

/// Every page by key, to Power off; the digest the helper installed.
fn install_by_keys(vm: &mut Vm, serial: &Path, artifacts: &Path) -> Result<(String, String)> {
    let ctl = Vmctl::from_info(vm.info.clone());
    to_review(vm, &ctl, serial, artifacts, "keyboard-install", "0", true)?;
    // Escape closes the dialog: nothing is asked of the helper.
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the erase dialog",
        1,
        EVENT_TIMEOUT,
        is_event("confirm", "state", "shown"),
    )?;
    // The dialog is on screen once it has finished opening.
    std::thread::sleep(Duration::from_secs(2));
    ctl.save_screenshot(&artifacts.join("next-keyboard-install-erase-dialog.png"))?;
    ctl.key("Escape")?;
    nth_marker(
        vm,
        serial,
        "the cancelled dialog",
        1,
        EVENT_TIMEOUT,
        is_event("confirm", "state", "cancelled"),
    )?;
    ensure!(
        helper_state(vm)?.applied.is_none(),
        "the helper began after the dialog was cancelled"
    );
    ensure!(
        count(serial, is_event("install", "", ""))? == 0,
        "Apply was asked after the dialog was cancelled"
    );
    // Then the erase is confirmed (a second dialog: its second `shown`).
    confirm_erase(vm, &ctl, serial, artifacts, "keyboard-install")?;
    let key = wait_done(vm, &ctl, serial, artifacts, "keyboard-install", true)?
        .context("the installation had no recovery key")?;
    let state = helper_state(vm)?;
    let summary = state
        .summary
        .context("the helper has no summary of what it installed")?;
    ensure!(
        matches!(state.outcome, Some(Outcome::Success))
            && summary.digest.starts_with("sha256:")
            && summary.composition == "generic.gnome"
            && summary.disk == "virtio-ilex-disk-0",
        "the helper's record: {summary:?} {:?}",
        state.outcome
    );
    no_password(vm, serial, Some(&key))?;
    // Enforcing and not one denial through the whole installation, asked before
    // the machine goes down.
    selinux::check(vm, artifacts, "next-keyboard-install")?;
    // The done page's Restart has the focus; Tab reaches Power off. The
    // machine going down is the proof (a Restart would leave QEMU running): its
    // last serial lines may not reach the log before the VM is gone.
    ctl.key("Tab")?;
    ctl.key("Return")?;
    let started = Instant::now();
    loop {
        if vm.exited()?.is_some() {
            break;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(300),
            "the machine did not power off after Power off"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    Ok((summary.digest, key))
}

// --- iso::next_reattach --------------------------------------------------------------------

fn reattach() -> Result<()> {
    let mut rig = boot(&Setup {
        name: "iso-next-reattach",
        purpose: "iso-next-reat",
        // `Apply` waits after it took the helper's lock: the window the
        // restarts are made in.
        fault: Some("slow-apply"),
        usb_medium: false,
        spares: false,
    })?;
    let artifacts = rig.artifacts.clone();
    let target = rig.target.clone();
    let vm = rig.guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let outcome = observe_reattach(vm, &serial, &artifacts, &target);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, "reattach");
    }
    outcome
}

/// The session unit gives up after three starts in five minutes: this resets
/// that counter (and only that), so that each kill is a restart of its own.
const RESET_START_LIMIT: &str = "systemctl reset-failed ilex-installer-session.service";

/// What the target looks like to the guest, as the helper names it.
const TARGET_DEVICE: &str = "/dev/disk/by-id/virtio-ilex-disk-0";

fn observe_reattach(vm: &mut Vm, serial: &Path, artifacts: &Path, target: &Path) -> Result<()> {
    let ctl = Vmctl::from_info(vm.info.clone());
    to_review(vm, &ctl, serial, artifacts, "reattach", "0", false)?;
    confirm_erase_with(vm, &ctl, serial, artifacts, "reattach", false)?;
    // The helper took `Apply` (it holds the lock for a minute, checking): its
    // state says so over the previous record, and the interface that asked is
    // still waiting for its answer, on the review page.
    let checking = wait_state(
        vm,
        "an Apply that is checking",
        Duration::from_secs(60),
        |s| s.starting.is_some(),
    )?;
    ensure!(
        !checking.active && checking.applied.is_none() && checking.refusal.is_none(),
        "State in the window of Apply is not the one before it: {checking:?}"
    );
    let refused_plan = checking.starting.clone().context("no starting plan")?;
    ensure!(
        count(serial, is_event("install", "result", "started"))? == 0
            && count(serial, is_page("installing"))? == 0,
        "the interface moved on before the helper answered"
    );
    ctl.save_screenshot(&artifacts.join("next-reattach-starting-review.png"))?;

    // 1. This `Apply` is going to be refused: the disk is changed behind its
    //    back, as a stick or a spare disk plugged in and formatted would be. A
    //    partition table where the plan found none is not "the disk that was
    //    checked", and the helper refuses it once its minute is over (after the
    //    signature, which needs the registry). Nothing of its own is written.
    vm.ssh_ok(&format!(
        "printf 'label: gpt\\n,64M\\n' | sfdisk --quiet {TARGET_DEVICE} && sync"
    ))?;
    let changed_by_the_test = head_hash(target)?;
    let welcomes = count(serial, is_page("welcome"))?;
    let probes = count(serial, is_event("probe", "result", "ok"))?;
    let mut round = 0;
    vm.ssh_ok(&format!("{RESET_START_LIMIT}; {KILL_INSTALLER}"))?;
    round += 1;
    let page = nth_marker(
        vm,
        serial,
        "the installing page after the interface was killed while Apply checked",
        round,
        Duration::from_secs(180),
        |w| is_page("installing")(w) && w.get("reattached").is_some_and(|r| r == "true"),
    )?;
    ensure!(
        field(&page, "stage")? == "starting" && field(&page, "progress")? == "0",
        "the installing page found the helper's Apply checking, with every stage waiting: {page:?}"
    );
    ensure!(
        count(serial, is_event("attach", "result", "installing"))? >= round,
        "no attach marker after the interface was killed during Apply: {page:?}"
    );
    wait_page(vm, &ctl, artifacts, "reattach-starting")?;
    ctl.save_screenshot(&artifacts.join("next-reattach-starting.png"))?;
    // The helper says which plan it refused, and why, and that nothing began.
    let refused = wait_state(vm, "the refusal of Apply", Duration::from_secs(240), |s| {
        s.refusal.is_some()
    })?;
    let refusal = refused.refusal.clone().context("no refusal")?;
    ensure!(
        refusal.plan == refused_plan
            && refusal.error.code == ErrorCode::DiskChanged
            && !refusal.error.message.contains(PASSWORD)
            && !refused.active
            && refused.starting.is_none()
            && refused.applied.is_none()
            && refused.outcome.is_none()
            && !refused.disk_changed,
        "the helper's record of the refused Apply: {refused:?}"
    );
    // The interface that was restarted in the window knows no choices to go
    // back to: the welcome page, with the sentence. Not the problem page, not
    // the lost-helper page, and no outcome of an earlier attempt.
    let again = nth_marker(
        vm,
        serial,
        "the welcome page after the Apply was refused",
        1,
        Duration::from_secs(240),
        |w| is_page("welcome")(w) && w.get("notice").is_some_and(|n| n == "not-started"),
    )?;
    ensure!(
        field(&again, "edition")? == "gnome",
        "the welcome page after the refusal: {again:?}"
    );
    ensure!(
        count(serial, is_event("install", "result", "not-started"))? >= 1
            && count(serial, is_page("problem"))? == 0
            && count(serial, is_event("lost", "", ""))? == 0
            && count(serial, is_event("outcome", "", ""))? == 0
            && count(serial, is_event("stage", "", ""))? == 0,
        "the restarted interface read something else as the outcome: {}",
        console(serial)?
    );
    ensure!(
        head_hash(target)? == changed_by_the_test && !helper_state(vm)?.disk_changed,
        "the refused Apply wrote to the disk"
    );
    // The first look at the computer, the restarted interface's own.
    nth_marker(
        vm,
        serial,
        "the look at the computer after the refusal",
        probes + 1,
        EVENT_TIMEOUT,
        is_event("probe", "result", "ok"),
    )?;
    wait_page(vm, &ctl, artifacts, "reattach-not-started")?;
    ctl.save_screenshot(&artifacts.join("next-reattach-not-started.png"))?;
    let welcomes_now = count(serial, is_page("welcome"))?;
    ensure!(
        welcomes_now == welcomes + 1,
        "the welcome page was announced {} times after the refusal, expected once",
        welcomes_now - welcomes
    );

    // 2. The next walk-through is the person's, and installs: the same session,
    //    the same helper, the disk now carrying the partition table that was
    //    written. The window is open again, and the interface is killed in it
    //    once more.
    from_welcome(vm, &ctl, serial, artifacts, "reattach-again", "0", false)?;
    confirm_erase_with(vm, &ctl, serial, artifacts, "reattach-again", false)?;
    let checking = wait_state(
        vm,
        "the second Apply that is checking",
        Duration::from_secs(60),
        |s| s.starting.is_some(),
    )?;
    let plan = checking.starting.clone().context("no starting plan")?;
    ensure!(
        plan != refused_plan && checking.refusal.is_none() && checking.applied.is_none(),
        "State in the second window is not a new Apply's: {checking:?}"
    );
    vm.ssh_ok(&format!("{RESET_START_LIMIT}; {KILL_INSTALLER}"))?;
    round += 1;
    let page = nth_marker(
        vm,
        serial,
        "the installing page after the interface was killed in the second window",
        round,
        Duration::from_secs(180),
        |w| is_page("installing")(w) && w.get("reattached").is_some_and(|r| r == "true"),
    )?;
    ensure!(
        field(&page, "stage")? == "starting" && field(&page, "progress")? == "0",
        "the installing page found the second Apply checking, with every stage waiting: {page:?}"
    );
    wait_page(vm, &ctl, artifacts, "reattach-starting-again")?;
    ctl.save_screenshot(&artifacts.join("next-reattach-starting-again.png"))?;
    // The installation the helper then began is the one that was confirmed
    // the second time, and the new interface follows it (the old one never saw
    // its answer).
    nth_marker(
        vm,
        serial,
        "the image stage",
        1,
        Duration::from_secs(600),
        |w| {
            is_event("stage", "stage", "image")(w)
                && w.get("status").is_some_and(|s| s == "running")
        },
    )?;
    let first = helper_state(vm)?;
    ensure!(
        first.active && first.starting.is_none() && first.applied.as_deref() == Some(plan.as_str()),
        "the installation is not the one Apply was checking: {first:?}"
    );
    // The compositor, in the image stage.
    vm.ssh_ok(&format!("{RESET_START_LIMIT}; {KILL_COMPOSITOR}"))?;
    round += 1;
    // The first page the new interface shows says it found the installation
    // under way.
    let page = nth_marker(
        vm,
        serial,
        "the installing page after the compositor was killed",
        round,
        Duration::from_secs(180),
        |w| is_page("installing")(w) && w.get("reattached").is_some_and(|r| r == "true"),
    )?;
    ensure!(
        count(serial, is_event("attach", "result", "installing"))? >= round,
        "no attach marker after the compositor was killed: {page:?}"
    );
    std::thread::sleep(Duration::from_secs(5));
    let state = helper_state(vm)?;
    ensure!(
        state.applied.as_deref() == Some(plan.as_str())
            && (state.active || state.outcome.is_some()),
        "the helper's installation changed when the compositor was killed: {state:?}"
    );
    ctl.save_screenshot(&artifacts.join("next-reattach-compositor.png"))?;
    ensure!(
        count(serial, is_page("welcome"))? == welcomes_now,
        "the interface showed the welcome page after it was killed during an installation"
    );
    let outcome = nth_marker(
        vm,
        serial,
        "the end of the installation",
        1,
        INSTALL_TIMEOUT,
        is_event("outcome", "", ""),
    )?;
    ensure!(
        field(&outcome, "result")? == "success",
        "the installation did not succeed after the restarts: {outcome:?}; serial: {}",
        console(serial)?
    );
    let done = nth_marker(vm, serial, "the done page", 1, EVENT_TIMEOUT, |w| {
        is_page("done")(w) && w.get("reattached").is_some_and(|r| r == "true")
    })?;
    ensure!(
        field(&done, "composition")? == "generic.gnome",
        "the done page after a restart: {done:?}"
    );
    wait_page(vm, &ctl, artifacts, "reattach-done")?;
    ctl.save_screenshot(&artifacts.join("next-reattach-done.png"))?;
    let state = helper_state(vm)?;
    ensure!(
        matches!(state.outcome, Some(Outcome::Success))
            && state.applied.as_deref() == Some(plan.as_str())
            && state.starting.is_none()
            && state.refusal.is_none(),
        "the helper's record after the restarts: {state:?}"
    );
    no_password(vm, serial, None)?;
    selinux::check(vm, artifacts, "next-reattach")?;
    Ok(())
}

// --- iso::next_problem ---------------------------------------------------------------------

/// A device the helper is asked to write the log to, and the code of the
/// refusal that must come back.
fn refuses(vm: &Vm, device: &str, code: &str) -> Result<()> {
    let ran = ssh(
        vm,
        &busctl("ilex-installer", "Finish", &[&format!("save-log:{device}")]),
    )?;
    let refused = refusal(&ran).with_context(|| format!("save-log:{device}"))?;
    ensure!(
        format!("{:?}", refused.code).to_lowercase() == code.replace('-', ""),
        "save-log:{device} is refused as {:?}, expected {code}: {}",
        refused.code,
        refused.message
    );
    ensure!(
        !refused.message.contains(PASSWORD),
        "a refusal quotes the password"
    );
    Ok(())
}

/// The first 32 MiB of a file, as a hash: what must not change.
fn head_hash(path: &Path) -> Result<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg("head -c 33554432 \"$0\" | sha256sum")
        .arg(path)
        .output()
        .context("hashing the head of a disk image")?;
    ensure!(
        out.status.success(),
        "sha256sum of {} failed",
        path.display()
    );
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned())
}

fn problem() -> Result<()> {
    let mut rig = boot(&Setup {
        name: "iso-next-problem",
        purpose: "iso-next-prob",
        fault: Some("image"),
        usb_medium: true,
        spares: true,
    })?;
    let artifacts = rig.artifacts.clone();
    let (root, target, sticks) = (rig.root.clone(), rig.target.clone(), rig.sticks.clone());
    let vm = rig.guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let outcome = observe_problem(vm, &serial, &artifacts, &root, &target, &sticks);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, "problem");
    }
    outcome
}

fn observe_problem(
    vm: &mut Vm,
    serial: &Path,
    artifacts: &Path,
    root: &Path,
    target: &Path,
    sticks: &[PathBuf],
) -> Result<()> {
    let ctl = Vmctl::from_info(vm.info.clone());
    // The ISO's stick, the two sticks and the spare disk are too small or in
    // use: one disk is offered and four are not.
    to_review(vm, &ctl, serial, artifacts, "problem", "4", false)?;
    confirm_erase(vm, &ctl, serial, artifacts, "problem")?;

    // The image stage fails; the problem page says which stage and how.
    let failed = nth_marker(
        vm,
        serial,
        "the problem page",
        1,
        Duration::from_secs(900),
        is_page("problem"),
    )?;
    ensure!(
        field(&failed, "kind")? == "failed"
            && field(&failed, "stage")? == "image"
            && field(&failed, "retry")? == "true",
        "the problem page: {failed:?}"
    );
    wait_page(vm, &ctl, artifacts, "problem-problem")?;
    ctl.save_screenshot(&artifacts.join("next-problem-page.png"))?;
    let state = helper_state(vm)?;
    ensure!(
        matches!(&state.outcome, Some(Outcome::Failed { stage, .. }) if stage.name() == "image")
            && state.disk_changed
            && !state.active,
        "the helper's record of the failure: {state:?}"
    );

    // The devices the log must not go to, refused by the helper for their own
    // reason, and nothing on any of them changes.
    let probe = helper_probe(vm)?;
    let by_serial = |serial: &str| {
        probe
            .disks
            .iter()
            .find(|d| d.serial == serial || d.id.contains(serial))
            .map(|d| d.id.clone())
            .with_context(|| format!("no disk with serial {serial} in the probe"))
    };
    let fat_id = by_serial("ILEXSTICK0")?;
    let blank_id = by_serial("ILEXSTICK1")?;
    let spare_id = by_serial("ilex-disk-1")?;
    let live_id = probe
        .disks
        .iter()
        .find(|d| d.live_medium)
        .map(|d| d.id.clone())
        .context("the ISO's stick is not the live medium")?;
    ensure!(
        probe
            .disks
            .iter()
            .find(|d| d.id == fat_id)
            .is_some_and(|d| d.removable)
            && probe
                .disks
                .iter()
                .find(|d| d.id == live_id)
                .is_some_and(|d| d.removable),
        "the sticks are not removable disks in the probe"
    );
    let (target_head, blank_before, spare_before) = (
        head_hash(target)?,
        head_hash(&sticks[1])?,
        head_hash(&root.join("spare.raw"))?,
    );
    refuses(vm, "virtio-ilex-disk-0", "install-target")?;
    refuses(vm, &live_id, "live-medium")?;
    refuses(vm, &spare_id, "not-removable")?;
    refuses(vm, &blank_id, "no-file-system")?;
    refuses(vm, "usb-nothing-0:0", "unknown-disk")?;
    for bad in ["bogus", "save-log:", "save-log:../x", "save-log:a/b", ""] {
        let ran = ssh(vm, &busctl("ilex-installer", "Finish", &[bad]))?;
        let error = refusal(&ran).with_context(|| format!("Finish({bad:?})"))?;
        ensure!(
            format!("{:?}", error.code) == "InvalidField",
            "Finish({bad:?}) is refused as {:?}",
            error.code
        );
    }
    let nobody = ssh(vm, &busctl("nobody", "Finish", &["restart"]))?;
    ensure!(
        crate::installer::apply::denied(&nobody),
        "the bus let another user call Finish: {}",
        nobody.text
    );
    ensure!(
        head_hash(target)? == target_head
            && head_hash(&sticks[1])? == blank_before
            && head_hash(&root.join("spare.raw"))? == spare_before,
        "a refused Finish wrote to a disk"
    );
    ensure!(
        stick_files(&sticks[0])?.is_empty(),
        "a refused Finish wrote a file to the FAT stick"
    );

    // Save the log by keyboard: Try again has the focus, Tab reaches Save the
    // log, Enter opens the dialog, which puts the focus on the stick the log
    // can go to.
    ctl.key("Tab")?;
    ctl.key("Return")?;
    let choosing = nth_marker(
        vm,
        serial,
        "the sticks the dialog lists",
        1,
        EVENT_TIMEOUT,
        is_event("log", "state", "choosing"),
    )?;
    ensure!(
        field(&choosing, "offered")? == "2" && field(&choosing, "usable")? == "1",
        "the sticks offered: {choosing:?} (the FAT stick can take the log, the empty one cannot, \
         the live medium and the target are not offered)"
    );
    std::thread::sleep(Duration::from_secs(2));
    ctl.save_screenshot(&artifacts.join("next-problem-save-log.png"))?;
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the log written",
        1,
        Duration::from_secs(180),
        is_event("finish", "action", "save-log"),
    )
    .and_then(|w| {
        ensure!(field(&w, "result")? == "ok", "the log was not saved: {w:?}");
        Ok(w)
    })?;
    nth_marker(
        vm,
        serial,
        "the saved dialog",
        1,
        EVENT_TIMEOUT,
        is_event("log", "state", "saved"),
    )?;
    std::thread::sleep(Duration::from_secs(2));
    ctl.save_screenshot(&artifacts.join("next-problem-log-saved.png"))?;

    // The file is on the stick, and is what the helper kept.
    let files = stick_files(&sticks[0])?;
    ensure!(
        files.len() == 1
            && files[0].starts_with("ilex-installer-log-")
            && files[0].ends_with(".tar"),
        "the stick holds {files:?}"
    );
    let tarball = root.join("saved-log.tar");
    ensure!(
        Command::new("mcopy")
            .arg("-i")
            .arg(format!("{}@@1M", sticks[0].display()))
            .arg(format!("::{}", files[0]))
            .arg(&tarball)
            .status()?
            .success(),
        "mcopy could not read the log"
    );
    let listing = String::from_utf8_lossy(
        &Command::new("tar")
            .arg("-tf")
            .arg(&tarball)
            .output()?
            .stdout,
    )
    .into_owned();
    for member in [
        "plan.json",
        "state.json",
        "lsblk.txt",
        "MANIFEST.txt",
        "01-partition.log",
        "03-mount.log",
        "04-image.log",
    ] {
        ensure!(
            listing
                .lines()
                .any(|l| l == format!("ilex-installer-log/{member}")),
            "the log has no {member}: {listing}"
        );
    }
    let unpacked = root.join("saved-log");
    fs::create_dir_all(&unpacked)?;
    ensure!(
        Command::new("tar")
            .arg("-xf")
            .arg(&tarball)
            .arg("-C")
            .arg(&unpacked)
            .status()?
            .success(),
        "tar could not unpack the log"
    );
    let grep = Command::new("grep")
        .args(["-r", "-l", "-F", "-e", "Correct horse", "-e", "staple"])
        .arg(&unpacked)
        .output()?;
    ensure!(
        grep.stdout.is_empty(),
        "the saved log holds the password: {}",
        String::from_utf8_lossy(&grep.stdout)
    );
    let saved_state: State =
        serde_json::from_slice(&fs::read(unpacked.join("ilex-installer-log/state.json"))?)?;
    ensure!(
        matches!(saved_state.outcome, Some(Outcome::Failed { .. })),
        "the saved state is not the failure: {saved_state:?}"
    );
    let lsblk = fs::read_to_string(unpacked.join("ilex-installer-log/lsblk.txt"))?;
    ensure!(
        lsblk.contains("ILEXSTICK0") && lsblk.contains("ilex-disk-0"),
        "the saved lsblk does not name the disks: {lsblk}"
    );
    // Close has the focus in the saved dialog: Enter closes it, and writes
    // nothing more.
    ctl.key("Return")?;
    std::thread::sleep(Duration::from_secs(2));
    ensure!(
        stick_files(&sticks[0])?.len() == 1,
        "closing the dialog wrote another file"
    );

    // Try again: the review page again, the same choices checked, and the erase
    // confirmed again: the image stage fails again.
    ctl.key("shift+Tab")?;
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the second check of the choices",
        2,
        Duration::from_secs(300),
        |w| is_event("plan", "result", "ok")(w),
    )?;
    wait_page(vm, &ctl, artifacts, "problem-review-again")?;
    confirm_erase(vm, &ctl, serial, artifacts, "problem-again")?;
    nth_marker(
        vm,
        serial,
        "the problem page again",
        2,
        Duration::from_secs(900),
        is_page("problem"),
    )?;
    wait_page(vm, &ctl, artifacts, "problem-problem-again")?;
    ctl.save_screenshot(&artifacts.join("next-problem-again.png"))?;

    // A third attempt, and the helper is killed as it begins to partition: the
    // problem page comes from what its successor recorded.
    ctl.key("Return")?;
    nth_marker(
        vm,
        serial,
        "the third check of the choices",
        3,
        Duration::from_secs(300),
        |w| is_event("plan", "result", "ok")(w),
    )?;
    wait_page(vm, &ctl, artifacts, "problem-review-third")?;
    // A guest-side watcher kills the helper the moment the partition stage is
    // recorded as running (the stage is over in a second or two, faster than a
    // harness that reads the console can answer).
    vm.ssh_ok(
        "setsid -f sh -c 'for i in $(seq 1 6000); do \
             if grep -q \"\\\"stage\\\": \\\"partition\\\"\" /run/ilex-installer/state.json 2>/dev/null \
                && grep -q \"\\\"active\\\": true\" /run/ilex-installer/state.json 2>/dev/null; then \
                 systemctl kill -s KILL ilex-installerd.service; echo killed > /run/ilex-helper-killed; exit 0; \
             fi; sleep 0.02; done' >/dev/null 2>&1 < /dev/null; echo watcher-started",
    )?;
    confirm_erase(vm, &ctl, serial, artifacts, "problem-third")?;
    let stopped = nth_marker(
        vm,
        serial,
        "the problem page after the helper was killed",
        3,
        Duration::from_secs(300),
        is_page("problem"),
    )?;
    ensure!(
        field(&stopped, "kind")? == "failed"
            && vm
                .ssh_ok("cat /run/ilex-helper-killed 2>/dev/null || echo not-killed")?
                .trim()
                == "killed",
        "the problem page after the helper was killed: {stopped:?} (killed: the watcher's file)"
    );
    wait_page(vm, &ctl, artifacts, "problem-helper-stopped")?;
    ctl.save_screenshot(&artifacts.join("next-problem-helper-stopped.png"))?;
    let recorded = helper_state(vm)?;
    ensure!(
        matches!(&recorded.outcome, Some(Outcome::Failed { reason, .. }) if reason.contains("helper stopped")),
        "the helper's successor did not record the stop: {recorded:?}"
    );
    no_password(vm, serial, None)?;
    selinux::check(vm, artifacts, "next-problem")?;
    Ok(())
}
