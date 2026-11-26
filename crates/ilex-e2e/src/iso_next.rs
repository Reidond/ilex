//! Opt-in tier for the next installer's live ISO (`iso::next…`, built by
//! `cargo ilexctl iso --next` or named by `ILEX_ISO_NEXT`). It is named
//! explicitly: `iso::` alone is the Anaconda release gate and must not boot an
//! ISO of another product.
//!
//! The session prints marker lines on the serial console
//! (`ILEX-INSTALLER-SESSION …` from the session script, `ILEX-INSTALLER page=…`
//! once the window is mapped, `ILEX-INSTALLER-STATS …` after the page has
//! settled), and the harness takes a QMP screenshot of what was actually drawn.
//! Markers are matched as complete lines (one still being written is not a
//! line yet) by their first word and a key, never as substrings.
//!
//! The live image's SSH server starts only once root's `authorized_keys` has
//! been provisioned (here by the harness's `ssh.authorized_keys.root` SMBIOS
//! credential, which every VM gets; bcvk's `tmpfiles.extra` credential does the
//! same for its own VMs); the guest's own diagnostics and the polkit report use
//! it. The serial trials boot without the key and check that nothing answers
//! on the forwarded port.
//!
//! * `iso::next_boot`: UEFI with Secure Boot required on KVM, default boot
//!   entry; also reports what polkit lets the installer's session do.
//! * `iso::next_basic_graphics`: the same, choosing "Install ilex (basic
//!   graphics)" in GRUB with real key input; GTK must report its cairo
//!   renderer.
//! * `iso::next_fallback`: a fixed test credential makes the session fail; the
//!   unit gives up after three failures and the message appears, once, on tty1
//!   (read back from the VT's own text, `/dev/vcs1`).
//! * `iso::next_restart`: a window close request does not end the session; a
//!   compositor that is killed leaves its socket behind and the next attempt
//!   still comes up; so does a killed client.
//! * `iso::next_ssh_tmpfiles`: the key arrives the way bcvk delivers it (a
//!   `tmpfiles.extra` credential, not `ssh.authorized_keys.root`) and sshd
//!   still starts.
//! * `iso::next_serial_backpressure` / `_unavailable` / `_stall`: the serial
//!   mirror of the markers fails on purpose (a fixed test credential: a stopped
//!   pty, no device, a worker that hangs for three seconds); the page still
//!   comes up promptly, each failed write is reported on the second UART within
//!   a second, and no SSH server runs without the key.
//!
//! The boot trials use `SecurityLaunch::QualificationFixture`: provisioning
//! (KVM with SMM, a Secure Boot firmware pair) is required, a passing
//! qualification of the host is not. They measure whatever the host gives them
//! and never promote its qualification record.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::cmd;
use ilex_devkit::core::{Capability, Requirement, SecurityLaunch, VmProvider};
use ilex_devkit::provider::{LinuxProvider, LinuxVm};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::{Frame, Vmctl};
use libtest_mimic::{Failed, Trial};

use crate::common;
use crate::iso::grub_key;

/// From power-on to the page being mapped. Boot is a few tens of seconds; the
/// margin is for a contended runner.
const PAGE_TIMEOUT: Duration = Duration::from_secs(420);
/// The page's memory sample comes this long after the page.
const STATS_TIMEOUT: Duration = Duration::from_secs(90);
/// What the installer VM gets unless `ILEX_ISO_NEXT_MEMORY` says otherwise.
const MEMORY: &str = "4G";
const GRUB_COUNTDOWN: &str = "The highlighted entry will be executed automatically";
/// What a failed mirror write may take the interface: the worker's budget is
/// 200 ms, with margin for a loaded guest.
const SERIAL_BUDGET_SECONDS: f64 = 1.0;
/// The polkit actions the installer's session may do without a password:
/// changing VT, so Ctrl+Alt+F2 reaches tty2's root shell from the page. Each
/// later page that needs another (a restart button, the network page) adds its
/// action here with the rule that allows it.
const POLKIT_ALLOWED: &[&str] = &["org.freedesktop.login1.chvt"];

pub fn trials() -> Vec<Trial> {
    vec![
        trial("iso::next_boot", || boot(Entry::Normal)),
        trial("iso::next_basic_graphics", || boot(Entry::BasicGraphics)),
        trial("iso::next_fallback", fallback),
        trial("iso::next_restart", restart),
        trial("iso::next_serial_backpressure", || serial("backpressure")),
        trial("iso::next_serial_unavailable", || serial("unavailable")),
        trial("iso::next_serial_stall", || serial("stall")),
        trial("iso::next_ssh_tmpfiles", ssh_tmpfiles),
    ]
}

fn trial(name: &'static str, run: fn() -> Result<()>) -> Trial {
    Trial::test(name, move || {
        run().map_err(|error| Failed::from(format!("{error:#}")))
    })
}

#[derive(Clone, Copy, PartialEq)]
enum Entry {
    Normal,
    BasicGraphics,
}

impl Entry {
    fn name(self) -> &'static str {
        match self {
            Entry::Normal => "normal",
            Entry::BasicGraphics => "basic-graphics",
        }
    }
    /// VM purposes stay short: they name Unix socket paths.
    fn purpose(self) -> &'static str {
        match self {
            Entry::Normal => "iso-next",
            Entry::BasicGraphics => "iso-next-basic",
        }
    }
    /// What the session script reports for it.
    fn graphics(self) -> &'static str {
        match self {
            Entry::Normal => "default",
            Entry::BasicGraphics => "basic",
        }
    }
}

fn iso_path() -> Result<PathBuf> {
    let Some(path) = std::env::var_os("ILEX_ISO_NEXT").filter(|p| !p.is_empty()) else {
        bail!(
            "ILEX_ISO_NEXT is not set: run `cargo ilexctl test iso::next`, which builds the ISO first"
        );
    };
    let path = PathBuf::from(path);
    ensure!(
        path.is_file(),
        "ILEX_ISO_NEXT={} is not a file",
        path.display()
    );
    Ok(path)
}

fn blank_disk(path: &Path) -> Result<()> {
    cmd::require("qemu-img")?;
    let _ = fs::remove_file(path);
    cmd::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(path)
            .arg("1G"),
    )
}

/// A secure guest and its private fixture; both go away with it.
struct Guest {
    provider: LinuxProvider,
    vm: LinuxVm,
    root: PathBuf,
}

impl Drop for Guest {
    fn drop(&mut self) {
        match self.provider.destroy_owned(&mut self.vm) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&self.root);
            }
            Err(e) => eprintln!(
                "iso::next fixture cleanup: {e:#}; retained {}",
                self.root.display()
            ),
        }
    }
}

fn boot(entry: Entry) -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let root = ws
        .state_dir()
        .join("iso-next-fixtures")
        .join(entry.purpose());
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let disk = root.join("blank.qcow2");
    blank_disk(&disk)?;

    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    let mut config = VmConfig::new(entry.purpose(), &disk);
    config.snapshot = false;
    config.memory = memory.clone();
    config.cdrom = Some(iso.clone());
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    // Provisioning is required, a qualified host is not: this tier measures
    // whatever size the run asks for and never promotes the host's
    // qualification (see CLAUDE.md, "Linux security qualification").
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    // GRUB drops navigation keys that virtio-keyboard sends on some OVMF
    // builds; the basic graphics entry is chosen with real key input.
    let provider = LinuxProvider::new(ws.clone())?.with_usb_keyboard(entry == Entry::BasicGraphics);
    let caps = provider.probe(&spec)?;
    fs::write(
        artifacts.join(format!("next-{}-backend.json", entry.name())),
        serde_json::to_vec_pretty(&caps)?,
    )?;
    caps.require_provisioning(Capability::FirmwareEnforcement)?;
    let vm = provider.create(spec)?;
    let mut guest = Guest { provider, vm, root };

    let started = Instant::now();
    guest.provider.start(&mut guest.vm)?;
    let vm = guest.vm.running_mut()?;
    ensure!(
        vm.info.secure_boot,
        "the VM was not started with Secure Boot"
    );
    let outcome = observe(vm, entry, &iso, &memory, started, &artifacts);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, entry.name());
    }
    outcome
}

/// Everything the boot trials assert once the guest runs.
fn observe(
    vm: &mut Vm,
    entry: Entry,
    iso: &Path,
    memory: &str,
    started: Instant,
    artifacts: &Path,
) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());

    if entry == Entry::BasicGraphics {
        // The countdown is the point at which GRUB accepts keys. Moving down
        // stops it and selects the second entry. GRUB redraws its menu with
        // cursor movements, not lines: this one is a substring match.
        wait_for_text(vm, &serial, GRUB_COUNTDOWN, PAGE_TIMEOUT)?;
        grub_key(&ctl, "down")?;
        ctl.save_screenshot(&artifacts.join("next-basic-graphics-grub.png"))?;
        grub_key(&ctl, "Return")?;
    }

    let console = wait_for_marker(vm, &serial, "ILEX-INSTALLER", "page", 1, PAGE_TIMEOUT)?;
    let to_page = started.elapsed();
    let session = fields(&console, "ILEX-INSTALLER-SESSION", "compositor")?;
    let page = fields(&console, "ILEX-INSTALLER", "page")?;
    let graphics = field(&session, "graphics")?;
    ensure!(
        graphics == entry.graphics(),
        "booted graphics={graphics} but the {} entry was chosen",
        entry.name()
    );
    ensure!(
        field(&session, "secure-boot")? == "1",
        "the guest does not report Secure Boot on: {session:?}"
    );
    ensure!(
        field(&page, "page")? == "placeholder",
        "unexpected page: {page:?}"
    );
    ensure!(
        field(&page, "edition")? == "gnome",
        "unexpected edition: {page:?}"
    );
    let renderer = field(&page, "renderer")?;
    if entry == Entry::BasicGraphics {
        ensure!(
            renderer == "GskCairoRenderer",
            "basic graphics must draw with cairo, GTK chose {renderer}"
        );
    }
    ensure!(
        !has_marker(&console, "ILEX-INSTALLER-FALLBACK", None),
        "the session fell back: {console}"
    );

    // What was actually drawn: a settled frame that is the libadwaita page.
    let frame = wait_page(vm, &ctl, artifacts, entry.name())?;
    let shot = artifacts.join(format!("next-{}.png", entry.name()));
    ctl.save_screenshot(&shot)?;

    let console = wait_for_marker(
        vm,
        &serial,
        "ILEX-INSTALLER-STATS",
        "mem_total_kib",
        1,
        STATS_TIMEOUT,
    )?;
    let stats = fields(&console, "ILEX-INSTALLER-STATS", "mem_total_kib")?;
    let kib = |key: &str| -> Result<u64> {
        let value = field(&stats, key)?;
        value
            .parse()
            .with_context(|| format!("memory sample {key}={value} is not a number: {stats:?}"))
    };
    let (total, available) = (kib("mem_total_kib")?, kib("mem_available_kib")?);
    let report = serde_json::json!({
        "entry": entry.name(),
        "secure_boot": true,
        "vcpus": 2,
        "memory": memory,
        "iso_bytes": fs::metadata(iso)?.len(),
        "seconds_power_on_to_page": to_page.as_secs_f64(),
        "guest_uptime_at_page": page.get("uptime"),
        "renderer": renderer,
        "scale": page.get("scale"),
        "screen": format!("{}x{}", frame.width, frame.height),
        "guest_mem_total_kib": total,
        "guest_mem_available_kib": available,
        "guest_mem_used_kib": total.saturating_sub(available),
        "guest_shmem_kib": kib("shmem_kib")?,
        "qemu_rss_kib": process_rss_kib(vm.info.pid),
        "screenshot": shot,
    });
    fs::write(
        artifacts.join(format!("next-{}.json", entry.name())),
        serde_json::to_vec_pretty(&report)?,
    )?;
    fs::write(
        artifacts.join(format!("next-{}-serial.txt", entry.name())),
        fs::read(&serial)?,
    )?;
    println!("iso::next {}: {report}", entry.name());

    if entry == Entry::Normal {
        vm.wait_ssh(Duration::from_secs(180))
            .context("the harness's SSH key did not give a login to the live ISO")?;
        // What a person does: Ctrl+Alt+F2 from the page reaches tty2's root
        // shell and Ctrl+Alt+F1 returns. The compositor asks logind to change
        // VT, which the polkit rule allows this session (and nothing else).
        keyboard_round_trip(vm, &ctl, artifacts)?;
        // The session still owns tty1 after root looks at the other VT: the
        // compositor gives the VT up and takes it back, with no getty between.
        vt_round_trip(vm, &Switch::Chvt)?;
        wait_page(vm, &ctl, artifacts, "after-vt-switch")?;
        ctl.save_screenshot(&artifacts.join("next-normal-after-vt-switch.png"))?;
        let session = vm.ssh_ok("systemctl is-active ilex-installer-session.service")?;
        ensure!(
            session == "active",
            "the session is {session} after a VT round trip"
        );
        surface(vm, artifacts)?;
    }
    Ok(())
}

/// What the installer's session can reach, from the running guest: the polkit
/// actions authorized for it without a password (each one is checked with
/// `pkcheck` against the session's own process), the names on the system bus,
/// the listening sockets. The authorized set must be exactly [`POLKIT_ALLOWED`].
fn surface(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    const SCRIPT: &str = r#"set -u
pid=$(systemctl show ilex-installer-session.service -p MainPID --value)
rule=/usr/share/polkit-1/rules.d/00-ilex-installer.rules
echo "=== session (main pid $pid)"
loginctl list-sessions --no-pager --no-legend
# The user manager has a session of its own (class manager-early, no seat):
# the installer's is the one on seat0.
sid=$(loginctl --no-legend list-sessions | awk '$3 == "ilex-installer" && $4 == "seat0" {print $1; exit}')
echo "SESSION-ID ${sid:-none}"
loginctl show-session "${sid:-none}" -p Id -p Seat -p VTNr -p Active -p Remote -p Class -p Type -p State
echo "=== masked units"
for unit in geoclue.service autovt@tty1.service; do
    echo "UNIT $unit $(systemctl is-enabled "$unit" 2>&1)"
done
echo "=== polkit: authorized for the session without a password"
for action in $(pkaction); do
    pkcheck --action-id "$action" --process "$pid" >/dev/null 2>&1 && echo "AUTHORIZED $action"
done
echo "=== polkit: end"
# Controls: the empty list above must be the rule's doing, not a broken check.
# Two actions the polkit defaults allow to any active local session.
control() {
    for action in org.freedesktop.login1.set-self-linger org.freedesktop.login1.inhibit-delay-sleep; do
        if pkcheck --action-id "$action" --process "$pid" >/dev/null 2>&1; then echo "CONTROL $1 AUTHORIZED $action"; else echo "CONTROL $1 DENIED $action"; fi
    done
}
control with-rule
mv "$rule" /run/00-ilex-installer.rules.off; sleep 4
control without-rule
mv /run/00-ilex-installer.rules.off "$rule"; sleep 4
control rule-restored
echo "=== system bus: names (running and activatable)"
busctl --system list --no-legend --no-pager | awk '{print $1, $NF}'
echo "=== listening sockets"
ss -tulnH
echo "=== units running"
systemctl list-units --type=service --state=running --no-legend --no-pager | awk '{print $1}'
"#;
    vm.wait_ssh(Duration::from_secs(180))
        .context("the harness's SSH key did not give a login to the live ISO")?;
    let report = vm.ssh_ok(SCRIPT).context("collecting the polkit surface")?;
    fs::write(artifacts.join("next-surface.txt"), &report)?;
    // The report is about the installer's own session, not the user manager's.
    for expected in [
        "Seat=seat0",
        "VTNr=1",
        "Active=yes",
        "Remote=no",
        "Class=user",
    ] {
        ensure!(
            report.lines().any(|line| line == expected),
            "the seat0 session of ilex-installer lacks {expected} (full report: next-surface.txt)"
        );
    }
    // Nothing needs location services or a getty on tty1 in the live image.
    for unit in ["geoclue.service", "autovt@tty1.service"] {
        ensure!(
            report
                .lines()
                .any(|line| line == format!("UNIT {unit} masked")),
            "{unit} is not masked (full report: next-surface.txt)"
        );
    }
    ensure!(
        !report
            .split("=== units running\n")
            .nth(1)
            .is_some_and(|running| running.contains("geoclue")),
        "geoclue is running (full report: next-surface.txt)"
    );
    let mut authorized: Vec<&str> = report
        .lines()
        .filter_map(|line| line.strip_prefix("AUTHORIZED "))
        .collect();
    authorized.sort_unstable();
    let mut allowed = POLKIT_ALLOWED.to_vec();
    allowed.sort_unstable();
    ensure!(
        authorized == allowed,
        "polkit authorizes {authorized:?} for the installer's session without a password, expected {allowed:?} (full report: next-surface.txt)"
    );
    // The rule is what denies: without it the defaults authorize the controls.
    let controls = |phase: &str| -> Vec<&str> {
        report
            .lines()
            .filter_map(|line| {
                line.strip_prefix("CONTROL ")?
                    .strip_prefix(phase)?
                    .strip_prefix(' ')
            })
            .map(|rest| rest.split_whitespace().next().unwrap_or(""))
            .collect()
    };
    for (phase, expected) in [
        ("with-rule", "DENIED"),
        ("without-rule", "AUTHORIZED"),
        ("rule-restored", "DENIED"),
    ] {
        let seen = controls(phase);
        ensure!(
            seen.len() == 2 && seen.iter().all(|result| *result == expected),
            "polkit control {phase}: expected both actions {expected}, saw {seen:?} (full report: next-surface.txt)"
        );
    }
    // Nothing but the harness's own SSH server listens beyond loopback.
    let sockets = report
        .split("=== listening sockets\n")
        .nth(1)
        .and_then(|rest| rest.split("\n=== ").next())
        .context("the surface report has no listening sockets section")?;
    let exposed: Vec<&str> = sockets
        .lines()
        .filter_map(|line| line.split_whitespace().nth(4))
        .filter(|local| {
            let address = local
                .rsplit_once(':')
                .map_or(*local, |(address, _)| address);
            let address = address.split('%').next().unwrap_or(address);
            !(address.starts_with("127.") || address == "[::1]" || address == "::1")
                && !local.ends_with(":22")
        })
        .collect();
    ensure!(
        exposed.is_empty(),
        "the live image listens beyond loopback on {exposed:?} (full report: next-surface.txt)"
    );
    Ok(())
}

/// After a failure: what the live system says about itself, over the
/// harness's SSH key when the guest has one (and a login to give it), and the
/// serial log and a screenshot regardless.
fn diagnostics(vm: &mut Vm, artifacts: &Path, name: &str) {
    if let Ok(console) = fs::read(vm.info.dir.join("serial.log")) {
        let _ = fs::write(
            artifacts.join(format!("next-{name}-failure-serial.txt")),
            console,
        );
    }
    let _ = Vmctl::from_info(vm.info.clone())
        .save_screenshot(&artifacts.join(format!("next-{name}-failure.png")));
    // A deadline of its own: a guest without sshd must not hang the failure path.
    // The units that decide what the session may do come first: the last 400
    // lines of everything are mostly the harness's own SSH logins.
    let script = "systemctl --no-pager --failed; \
         systemctl --no-pager status ilex-installer-session ilex-installer-fallback sshd; \
         loginctl list-sessions --no-pager; getenforce; \
         pid=$(systemctl show ilex-installer-session.service -p MainPID --value); \
         pkcheck --action-id org.freedesktop.login1.chvt --process $pid; echo pkcheck-chvt=$?; \
         journalctl -b --no-pager -o short-monotonic -n 200 \
             -u polkit.service -u systemd-logind.service -u ilex-installer-session.service; \
         journalctl -b --no-pager -o short-monotonic -n 400";
    let report = Command::new("timeout")
        .args(["90", "sh", "-c"])
        .arg(format!("{} -- '{script}'", vm.info.ssh_command_line()))
        .output();
    if let Ok(out) = report {
        let mut text = out.stdout;
        text.extend_from_slice(&out.stderr);
        let _ = fs::write(artifacts.join(format!("next-{name}-diagnostics.txt")), text);
    }
}

/// The serial log as text (invalid bytes replaced: GRUB and the kernel print
/// escape sequences). A log that does not exist yet is empty.
fn console(serial: &Path) -> Result<String> {
    match fs::read(serial) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes).into_owned()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", serial.display())),
    }
}

/// The complete lines of a console: a line still being written has no newline
/// yet and is not a line.
fn complete_lines(console: &str) -> impl DoubleEndedIterator<Item = &str> {
    let complete = console.rfind('\n').map_or("", |end| &console[..end]);
    complete.lines().map(|line| line.trim_end_matches('\r'))
}

/// A marker line: its first word is `name` and, when given, one of its words
/// is `key=…`.
fn is_marker(line: &str, name: &str, key: Option<&str>) -> bool {
    let mut words = line.split_whitespace();
    words.next() == Some(name)
        && key.is_none_or(|key| {
            words.any(|word| word.split_once('=').is_some_and(|(found, _)| found == key))
        })
}

fn has_marker(console: &str, name: &str, key: Option<&str>) -> bool {
    complete_lines(console).any(|line| is_marker(line, name, key))
}

fn count_markers(console: &str, name: &str, key: Option<&str>) -> usize {
    complete_lines(console)
        .filter(|line| is_marker(line, name, key))
        .count()
}

/// The `key=value` words of the last complete marker line.
fn fields(console: &str, name: &str, key: &str) -> Result<BTreeMap<String, String>> {
    let line = complete_lines(console)
        .rev()
        .find(|line| is_marker(line, name, Some(key)))
        .with_context(|| {
            format!("no complete {name} line with {key}= on the serial console: {console}")
        })?;
    Ok(words(line))
}

/// The `key=value` words of one marker line (its first word is the name).
fn words(line: &str) -> BTreeMap<String, String> {
    line.split_whitespace()
        .skip(1)
        .filter_map(|word| {
            let (key, value) = word.split_once('=')?;
            Some((key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Did a complete `ILEX-INSTALLER-SESSION ended` line report `child` (with
/// `status`, when given)?
fn session_ended(console: &str, child: &str, status: Option<&str>) -> bool {
    complete_lines(console)
        .filter(|line| is_marker(line, "ILEX-INSTALLER-SESSION", Some("child")))
        .map(words)
        .any(|w| {
            w.get("child").map(String::as_str) == Some(child)
                && status.is_none_or(|status| w.get("status").map(String::as_str) == Some(status))
        })
}

fn field<'a>(fields: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    fields
        .get(key)
        .map(String::as_str)
        .with_context(|| format!("the serial marker has no {key}=: {fields:?}"))
}

/// Wait until the serial log holds `count` complete `name … key=…` lines;
/// fails fast when the guest exited or the installer gave up. Returns the whole
/// log.
fn wait_for_marker(
    vm: &mut Vm,
    serial: &Path,
    name: &str,
    key: &str,
    count: usize,
    timeout: Duration,
) -> Result<String> {
    let started = Instant::now();
    let wanted = format!("{count} x {name} {key}=");
    loop {
        let console = console(serial)?;
        if count_markers(&console, name, Some(key)) >= count {
            return Ok(console);
        }
        if name != "ILEX-INSTALLER-FALLBACK"
            && has_marker(&console, "ILEX-INSTALLER-FALLBACK", None)
        {
            bail!("the session gave up while waiting for {wanted:?}; serial: {console}");
        }
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} while waiting for {wanted:?}; serial: {console}");
        }
        if started.elapsed() > timeout {
            bail!("no {wanted:?} within {timeout:?}; serial: {console}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Wait until the serial log contains `wanted` as text (GRUB's menu is drawn
/// with cursor movements, not lines).
fn wait_for_text(vm: &mut Vm, serial: &Path, wanted: &str, timeout: Duration) -> Result<()> {
    let started = Instant::now();
    loop {
        let console = console(serial)?;
        if console.contains(wanted) {
            return Ok(());
        }
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} while waiting for {wanted:?}; serial: {console}");
        }
        if started.elapsed() > timeout {
            bail!("no {wanted:?} within {timeout:?}; serial: {console}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn process_rss_kib(pid: u32) -> Option<u64> {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The page is up once two frames three seconds apart are the same and are
/// libadwaita's light window with content on it.
fn wait_page(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path, name: &str) -> Result<Frame> {
    let started = Instant::now();
    let mut previous = None;
    loop {
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} before the page was drawn");
        }
        if let Ok(frame) = ctl.screenshot() {
            let fingerprint = frame.fingerprint();
            if page_drawn(&frame) && previous == Some(fingerprint) {
                return Ok(frame);
            }
            previous = Some(fingerprint);
        }
        if started.elapsed() > Duration::from_secs(120) {
            let _ = ctl.save_screenshot(&artifacts.join(format!("next-{name}-failure.png")));
            bail!("the page was not drawn within two minutes of its marker");
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// libadwaita's window is near white; the status page puts an icon, a title
/// and rows on it, so a blank white or black screen is not the page.
fn page_drawn(frame: &Frame) -> bool {
    use std::collections::HashMap;
    let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
    let mut total = 0u32;
    for pixel in frame.rgb.as_chunks::<3>().0.iter().step_by(13) {
        *counts.entry(*pixel).or_default() += 1;
        total += 1;
    }
    let Some((colour, n)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
        return false;
    };
    let luma = (u32::from(colour[0]) * 3 + u32::from(colour[1]) * 6 + u32::from(colour[2])) / 10;
    luma > 235 && n * 100 > total * 40 && (total - n) * 1000 > total * 5
}

/// Light text on a dark screen: most pixels dark, some clearly light. A black
/// screen has none and a drawn page is mostly light.
fn text_drawn(frame: &Frame) -> bool {
    let (mut dark, mut light, mut total) = (0u32, 0u32, 0u32);
    for pixel in frame.rgb.as_chunks::<3>().0.iter().step_by(7) {
        let luma = (u32::from(pixel[0]) * 3 + u32::from(pixel[1]) * 6 + u32::from(pixel[2])) / 10;
        total += 1;
        if luma < 60 {
            dark += 1;
        } else if luma > 150 {
            light += 1;
        }
    }
    total > 0 && dark * 100 > total * 60 && light * 2000 > total
}

/// The VT the kernel has in front, read from the guest: a key chord that did
/// nothing cannot pass for a VT switch.
fn active_vt(vm: &Vm) -> Result<String> {
    vm.ssh_ok("cat /sys/class/tty/tty0/active")
}

fn wait_vt(vm: &Vm, wanted: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let active = active_vt(vm)?;
        if active == wanted {
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "the active VT is {active}, expected {wanted} after the key chord"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// How the trial changes VT: the key chord a person presses, or root's
/// `chvt` over SSH. On the fallback screen the chord reaches the kernel; on the
/// page the compositor holds the keyboard and asks logind
/// (`org.freedesktop.login1.chvt`), which the installer's polkit rule allows
/// for the session itself.
enum Switch<'a> {
    Keys(&'a Vmctl),
    Chvt,
}

fn switch_vt(vm: &Vm, how: &Switch, vt: u8) -> Result<()> {
    match how {
        Switch::Keys(ctl) => ctl.key(&format!("ctrl+alt+F{vt}")),
        Switch::Chvt => vm.ssh_ok(&format!("chvt {vt}")).map(|_| ()),
    }
}

/// tty1 to tty2 and back, the way a person looks at the shell and returns.
/// Nothing may take tty1 over meanwhile: no getty (`getty@tty1`, or
/// `autovt@tty1`, which logind starts on a switch to a VT without a session
/// when it thinks the VT is free) may be running afterwards.
fn vt_round_trip(vm: &Vm, how: &Switch) -> Result<()> {
    ensure!(
        active_vt(vm)? == "tty1",
        "tty1 is not in front before the round trip"
    );
    switch_vt(vm, how, 2)?;
    wait_vt(vm, "tty2")?;
    std::thread::sleep(Duration::from_secs(2));
    switch_vt(vm, how, 1)?;
    wait_vt(vm, "tty1")?;
    // Past the time logind needs to start autovt@tty1, were it to.
    std::thread::sleep(Duration::from_secs(4));
    let states = vm.ssh_ok("systemctl is-active getty@tty1.service autovt@tty1.service || true")?;
    ensure!(
        states.lines().all(|state| state == "inactive"),
        "a getty took tty1 over after the VT round trip (getty@tty1, autovt@tty1): {states:?}"
    );
    Ok(())
}

/// A person at the page presses Ctrl+Alt+F2 and gets tty2's root shell, types
/// into it and presses Ctrl+Alt+F1 to return to the page. The switch is read
/// from the kernel's active VT and the shell's answer from tty2's own text
/// (`/dev/vcs2`), so neither a dead chord nor a blank VT can pass.
fn keyboard_round_trip(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    ensure!(
        active_vt(vm)? == "tty1",
        "tty1 is not in front before Ctrl+Alt+F2"
    );
    // Control: with the rule the image had before (everything denied) the
    // compositor's login1.chvt request is refused and the chord does nothing,
    // so it is the rule that lets the chord through, not a dead key path.
    let rule = "/usr/share/polkit-1/rules.d/00-ilex-installer.rules";
    vm.ssh_ok(&format!(
        "set -e; cp {rule} /run/00-ilex-installer.rules.allow; \
         cat > {rule} <<'EOF'\n\
         polkit.addRule(function(action, subject) {{\n\
             if (subject.user == \"ilex-installer\") {{ return polkit.Result.NO; }}\n\
         }});\n\
         EOF\n\
         systemctl restart polkit.service"
    ))?;
    ctl.key("ctrl+alt+F2")?;
    std::thread::sleep(Duration::from_secs(4));
    let denied = active_vt(vm)?;
    vm.ssh_ok(&format!(
        "set -e; cp /run/00-ilex-installer.rules.allow {rule}; \
         rm -f /run/00-ilex-installer.rules.allow; systemctl restart polkit.service"
    ))?;
    ensure!(
        denied == "tty1",
        "Ctrl+Alt+F2 left the installer's page although the rule denied login1.chvt (active VT {denied})"
    );
    ctl.key("ctrl+alt+F2")?;
    wait_vt(vm, "tty2").context(
        "Ctrl+Alt+F2 did not leave the installer's page: gnome-kiosk needs --enable-vt-switch and polkit the login1.chvt rule",
    )?;
    // The shell is agetty's autologin on tty2: root, started when the VT was
    // first visited. Ask it a question only it can answer.
    let answer = "ILEX-TTY2-ANSWER-42-root";
    let started = Instant::now();
    std::thread::sleep(Duration::from_secs(2));
    ctl.type_text("echo ILEX-TTY2-ANSWER-$((6*7))-$(id -un)\n")?;
    let text = loop {
        let text = vm.ssh_ok("cat /dev/vcs2")?;
        // /dev/vcs2 is the screen's rows padded to the width, with no newlines.
        if text.split_whitespace().any(|word| word == answer) {
            break text;
        }
        ensure!(
            started.elapsed() < Duration::from_secs(45),
            "tty2's root shell did not answer; /dev/vcs2 reads: {text:?}"
        );
        std::thread::sleep(Duration::from_secs(1));
    };
    fs::write(artifacts.join("next-normal-tty2-vcs2.txt"), &text)?;
    let frame = ctl.save_screenshot(&artifacts.join("next-normal-tty2.png"))?;
    ensure!(
        text_drawn(&frame),
        "tty2 shows no light text on a dark screen (next-normal-tty2.png)"
    );
    ctl.key("ctrl+alt+F1")?;
    wait_vt(vm, "tty1").context("Ctrl+Alt+F1 did not return from tty2")?;
    wait_page(vm, ctl, artifacts, "after-keys-vt-switch")?;
    ctl.save_screenshot(&artifacts.join("next-normal-after-keys-vt-switch.png"))?;
    // Past the time logind needs to start autovt@tty1, were it to.
    std::thread::sleep(Duration::from_secs(4));
    let states = vm.ssh_ok("systemctl is-active getty@tty1.service autovt@tty1.service || true")?;
    ensure!(
        states.lines().all(|state| state == "inactive"),
        "a getty took tty1 over after the keyboard round trip: {states:?}"
    );
    let session = vm.ssh_ok("systemctl is-active ilex-installer-session.service")?;
    ensure!(
        session == "active",
        "the session is {session} after the keyboard round trip"
    );
    Ok(())
}

/// A plain (not Secure Boot) guest booted from the ISO on a blank disk.
fn plain_vm(
    ws: &ilex_devkit::Workspace,
    purpose: &str,
    configure: impl FnOnce(&mut VmConfig),
) -> Result<(Vm, PathBuf)> {
    let iso = iso_path()?;
    let work = ws.state_dir().join(format!("{purpose}-fixture"));
    fs::create_dir_all(&work)?;
    let disk = work.join("blank.qcow2");
    blank_disk(&disk)?;
    let mut config = VmConfig::new(purpose, &disk);
    config.memory = "2G".into();
    config.cdrom = Some(iso);
    configure(&mut config);
    let vm = Vm::boot(ws, &config)?;
    let serial = vm.info.dir.join("serial.log");
    Ok((vm, serial))
}

/// The session unit gives up after three failures and the fallback unit says
/// so on tty1, once. A fixed test credential makes the session script fail.
fn fallback() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let (mut vm, _) = plain_vm(&ws, "iso-next-fallback", |config| {
        config
            .smbios
            .push("type=11,value=io.systemd.credential:ilex.installer.fault=session".into());
    })?;
    let outcome = observe_fallback(&mut vm, &artifacts);
    if outcome.is_err() {
        diagnostics(&mut vm, &artifacts, "fallback");
    }
    outcome
}

fn observe_fallback(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    let console = wait_for_marker(
        vm,
        &serial,
        "ILEX-INSTALLER-FALLBACK",
        "result",
        1,
        PAGE_TIMEOUT,
    )?;
    let failures = count_markers(&console, "ILEX-INSTALLER-SESSION", Some("fault"));
    ensure!(
        failures == 3,
        "expected three session failures before the fallback, saw {failures}: {console}"
    );
    ensure!(
        !has_marker(&console, "ILEX-INSTALLER", Some("page")),
        "a page was shown although the session was made to fail: {console}"
    );

    // Say once, and leave it there: no second message while the guest idles.
    std::thread::sleep(Duration::from_secs(8));
    let console = self::console(&serial)?;
    let said = count_markers(&console, "ILEX-INSTALLER-FALLBACK", None);
    ensure!(
        said == 1,
        "the fallback fired {said} times, expected exactly once: {console}"
    );

    // What the screen holds, from the VT's own text (not from pixels): a black
    // tty1 would pass a "no page drawn" test.
    vm.wait_ssh(Duration::from_secs(180))
        .context("the harness's SSH key did not give a login to the live ISO")?;
    let vcs = vm.ssh_ok("cat /dev/vcs1")?;
    fs::write(artifacts.join("next-fallback-vcs1.txt"), &vcs)?;
    let squeezed = vcs.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        squeezed.contains("ilex installer")
            && squeezed.contains("could not start its graphical session")
            && squeezed.contains("journalctl -b -u ilex-installer-session"),
        "tty1 does not show the fallback message: {vcs:?}"
    );
    let frame = ctl.save_screenshot(&artifacts.join("next-fallback.png"))?;
    ensure!(
        !page_drawn(&frame),
        "the screen shows a drawn page, not the fallback message"
    );
    ensure!(
        text_drawn(&frame),
        "the screen is not light text on a dark background"
    );

    // The message must outlast a look at the tty2 shell: no getty may be
    // started on tty1 when the person comes back.
    vt_round_trip(vm, &Switch::Keys(&ctl))?;
    let after = vm.ssh_ok("cat /dev/vcs1")?;
    fs::write(
        artifacts.join("next-fallback-vcs1-after-vt-switch.txt"),
        &after,
    )?;
    let squeezed = after.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        squeezed.contains("could not start its graphical session") && !squeezed.contains("login:"),
        "tty1 lost the fallback message after a VT round trip: {after:?}"
    );
    let frame = ctl.save_screenshot(&artifacts.join("next-fallback-after-vt-switch.png"))?;
    ensure!(
        text_drawn(&frame) && !page_drawn(&frame),
        "the screen after the VT round trip is not the fallback message"
    );
    let console = self::console(&serial)?;
    ensure!(
        count_markers(&console, "ILEX-INSTALLER-FALLBACK", None) == 1,
        "the fallback fired again after a VT round trip: {console}"
    );
    fs::write(artifacts.join("next-fallback-serial.txt"), console)?;

    println!(
        "iso::next fallback: three failures, then the message on tty1, once, kept across a VT round trip"
    );
    Ok(())
}

/// Nothing may end the session but a failure the unit counts: a close request
/// is refused by the window, and a compositor or a client that dies is replaced
/// by a session that comes up (the compositor's socket survives a hard kill and
/// must not be mistaken for the next compositor's).
fn restart() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let (mut vm, _) = plain_vm(&ws, "iso-next-restart", |config| {
        config.memory = "3G".into();
    })?;
    let outcome = observe_restart(&mut vm, &artifacts);
    if outcome.is_err() {
        diagnostics(&mut vm, &artifacts, "restart");
    }
    outcome
}

fn observe_restart(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    wait_for_marker(vm, &serial, "ILEX-INSTALLER", "page", 1, PAGE_TIMEOUT)?;
    wait_page(vm, &ctl, artifacts, "restart")?;
    vm.wait_ssh(Duration::from_secs(180))
        .context("the harness's SSH key did not give a login to the live ISO")?;
    let restarts = |vm: &Vm| -> Result<u32> {
        let value =
            vm.ssh_ok("systemctl show ilex-installer-session.service -p NRestarts --value")?;
        value
            .parse()
            .with_context(|| format!("NRestarts={value:?} is not a number"))
    };
    ensure!(
        restarts(vm)? == 0,
        "the session restarted before anything was done to it"
    );

    // 1. The compositor asks the window to close (Alt+F4): the installer stays.
    ctl.key("alt+F4")?;
    std::thread::sleep(Duration::from_secs(6));
    let console = console(&serial)?;
    ensure!(
        count_markers(&console, "ILEX-INSTALLER", Some("page")) == 1
            && !has_marker(&console, "ILEX-INSTALLER-SESSION", Some("child")),
        "a close request ended the session: {console}"
    );
    ensure!(
        page_drawn(&ctl.save_screenshot(&artifacts.join("next-restart-after-close.png"))?),
        "the page is gone after a close request"
    );
    ensure!(restarts(vm)? == 0, "a close request restarted the session");

    // 2. The compositor is killed hard: its socket stays in the kept runtime
    //    directory, and the next attempt must come up on the first try.
    vm.ssh_ok(KILL_COMPOSITOR)?;
    let console = wait_for_marker(vm, &serial, "ILEX-INSTALLER", "page", 2, PAGE_TIMEOUT)?;
    ensure!(
        session_ended(&console, "compositor", Some("137")),
        "the session did not report the compositor's death: {console}"
    );
    ensure!(
        !complete_lines(&console).any(|line| is_marker(line, "ILEX-INSTALLER-SESSION", None)
            && (line.contains("compositor exited before listening")
                || line.contains("compositor is not listening"))),
        "the second attempt was spent on a stale socket: {console}"
    );
    wait_page(vm, &ctl, artifacts, "restart")?;
    ensure!(
        restarts(vm)? == 1,
        "expected one restart after the compositor died"
    );

    // 3. The client dies: the compositor is stopped with it and both return.
    vm.ssh_ok(KILL_INSTALLER)?;
    let console = wait_for_marker(vm, &serial, "ILEX-INSTALLER", "page", 3, PAGE_TIMEOUT)?;
    ensure!(
        session_ended(&console, "installer", None),
        "the session did not report the installer's death: {console}"
    );
    wait_page(vm, &ctl, artifacts, "restart")?;
    ensure!(
        restarts(vm)? == 2,
        "expected two restarts after the client died"
    );
    ensure!(
        !has_marker(&console, "ILEX-INSTALLER-FALLBACK", None),
        "the fallback fired after two recoverable failures: {console}"
    );
    fs::write(artifacts.join("next-restart-serial.txt"), console)?;
    println!("iso::next restart: close refused, compositor and client deaths recovered");
    Ok(())
}

/// A hard kill of the compositor, by process name (procps need not be in the image).
const KILL_COMPOSITOR: &str = "for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = gnome-kiosk ] && kill -KILL ${p#/proc/}; done; true";
const KILL_INSTALLER: &str = "for p in /proc/[0-9]*; do [ \"$(cat $p/comm 2>/dev/null)\" = ilex-installer ] && kill -KILL ${p#/proc/}; done; true";

/// The serial mirror of the markers fails on purpose; the interface must not
/// notice, and every failed write must be reported on the second UART. The
/// `backpressure` and `unavailable` variants boot without the harness's key:
/// the live image's SSH server must stay off. `stall` has the key, to start a
/// second marker while the first worker is stuck.
fn serial(mode: &'static str) -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let purpose = format!("iso-next-ser-{}", &mode[..2]);
    ensure!(
        ["backpressure", "unavailable", "stall"].contains(&mode),
        "unknown serial fault {mode}"
    );
    let (mut vm, _) = plain_vm(&ws, &purpose, |config| {
        config.memory = "3G".into();
        config.secondary_serial = true;
        // The stall variant needs a shell to start a second marker while the
        // first worker is stuck; the other two prove there is no SSH server.
        config.ssh_credential = mode == "stall";
        config.smbios.push(format!(
            "type=11,value=io.systemd.credential:ilex.installer.serial-test={mode}"
        ));
    })?;
    let outcome = observe_serial(&mut vm, &artifacts, mode);
    if outcome.is_err() {
        diagnostics(&mut vm, &artifacts, &format!("serial-{mode}"));
    }
    outcome
}

/// `mode` is the injected fault; `backpressure` and `unavailable` are also the
/// status their failed writes report.
fn observe_serial(vm: &mut Vm, artifacts: &Path, mode: &str) -> Result<()> {
    let primary = vm.info.dir.join("serial.log");
    let secondary = vm.info.dir.join("serial-secondary.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    // The faulted sink never sees a marker: readiness comes from the other UART.
    wait_for_marker(vm, &secondary, "ILEX-INSTALLER", "page", 1, PAGE_TIMEOUT)?;
    wait_page(vm, &ctl, artifacts, &format!("serial-{mode}"))?;
    if mode == "stall" {
        stall_burst(vm, &secondary)?;
    }
    let console = self::console(&secondary)?;

    let mut audited = 0;
    let mut budgets = 0;
    let mut busies = 0;
    let mut messages = BTreeSet::new();
    for line in
        complete_lines(&console).filter(|l| is_marker(l, "ILEX-INSTALLER-SERIAL", Some("mode")))
    {
        let words = words(line);
        messages.insert(field(&words, "message")?.to_owned());
        let status = field(&words, "status")?;
        // `stall` hangs its worker: the write that meets the deadline reports
        // `budget`, and the ones that come while it is still stuck `busy`.
        let expected: &[&str] = if mode == "stall" {
            &["budget", "busy"]
        } else {
            &[mode]
        };
        ensure!(
            field(&words, "mode")? == mode && expected.contains(&status),
            "a marker write did not fail as {mode} ({expected:?}): {line}"
        );
        budgets += usize::from(status == "budget");
        busies += usize::from(status == "busy");
        let seconds: f64 = field(&words, "seconds")?
            .parse()
            .with_context(|| format!("unparsable duration in {line}"))?;
        ensure!(
            seconds < SERIAL_BUDGET_SECONDS,
            "a faulted marker write took {seconds}s (limit {SERIAL_BUDGET_SECONDS}s): {line}"
        );
        audited += 1;
    }
    ensure!(
        audited >= 2,
        "expected an audit line for the session's and the page's markers, saw {audited}: {console}"
    );
    // Complete audit lines for the session's marker and for the page's own.
    for message in ["ILEX-INSTALLER-SESSION", "ILEX-INSTALLER"] {
        ensure!(
            messages.contains(message),
            "no complete audit line with message={message} (saw {messages:?}): {console}"
        );
    }
    if mode == "stall" {
        // The first write hit the deadline (200 ms), and the page was shown
        // while its worker was still stuck for seconds; a second marker of the
        // same process, while that worker was stuck, was skipped as busy.
        ensure!(budgets >= 1, "no write met the 200 ms deadline: {console}");
        ensure!(
            busies >= 1,
            "no marker was skipped while the stuck worker held the port: {console}"
        );
    } else {
        ensure!(
            !has_marker(&self::console(&primary)?, "ILEX-INSTALLER", Some("page")),
            "the page marker reached the faulted console"
        );
    }
    // No key, no SSH server (the stall variant booted with the key).
    if mode != "stall" {
        let banner = ssh_banner(vm.info.ssh_port)?;
        ensure!(
            !banner.starts_with("SSH-"),
            "an SSH server answers on the live ISO without the harness's key: {banner:?}"
        );
    }
    fs::write(
        artifacts.join(format!("next-serial-{mode}-secondary.txt")),
        console,
    )?;
    println!(
        "iso::next serial {mode}: {audited} faulted writes ({budgets} met the deadline, {busies} skipped as busy), each reported within {SERIAL_BUDGET_SECONDS}s"
    );
    Ok(())
}

/// Two markers from one process while its first worker is stuck: the first
/// meets the 200 ms deadline, the second is skipped (`busy`) rather than queued
/// behind it. The fault credential reaches the process the way it reaches the
/// session's: through `ImportCredential`.
fn stall_burst(vm: &mut Vm, secondary: &Path) -> Result<()> {
    vm.wait_ssh(Duration::from_secs(180))
        .context("the harness's SSH key did not give a login to the live ISO")?;
    vm.ssh_ok(
        "systemd-run --quiet --wait --collect --pipe -p ImportCredential=ilex.installer.serial-test \
         ilex-installer --marker 'ILEX-INSTALLER-BURST one' 'ILEX-INSTALLER-BURST two' >/dev/null",
    )?;
    let started = Instant::now();
    loop {
        let console = console(secondary)?;
        let statuses: Vec<String> = complete_lines(&console)
            .filter(|l| is_marker(l, "ILEX-INSTALLER-SERIAL", Some("message")))
            .map(words)
            .filter(|w| w.get("message").map(String::as_str) == Some("ILEX-INSTALLER-BURST"))
            .filter_map(|w| w.get("status").cloned())
            .collect();
        if statuses.len() >= 2 {
            ensure!(
                statuses == ["budget", "busy"],
                "the two markers of one process reported {statuses:?}, expected budget then busy: {console}"
            );
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "the burst's audit lines did not arrive (saw {statuses:?}): {console}"
        );
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Whatever the forwarded SSH port sends first: QEMU accepts the connection
/// and relays it to the guest, which resets it when nothing listens.
fn ssh_banner(port: u16) -> Result<String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .with_context(|| format!("connecting to the forwarded SSH port {port}"))?;
    stream.set_read_timeout(Some(Duration::from_secs(8)))?;
    let mut banner = [0u8; 64];
    match stream.read(&mut banner) {
        Ok(n) => Ok(String::from_utf8_lossy(&banner[..n]).into_owned()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(String::new())
        }
        Err(error) => Err(error).context("reading from the forwarded SSH port"),
    }
}

/// bcvk delivers its SSH key as a `tmpfiles.extra` credential that writes
/// `/root/.ssh/authorized_keys` (`f+~`), not as `ssh.authorized_keys.root`. The
/// live image starts sshd once that file exists, whichever way it got there.
fn ssh_tmpfiles() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let key = qemu::test_key(&ws)?;
    let tmpfiles = format!(
        "d- /root/.ssh 0700 - - -\nf+~ /root/.ssh/authorized_keys 0700 - - - {}\n",
        base64(key.public.as_bytes())
    );
    let (mut vm, _) = plain_vm(&ws, "iso-next-tmpfiles", |config| {
        config.ssh_credential = false;
        config.smbios.push(format!(
            "type=11,value=io.systemd.credential.binary:tmpfiles.extra={}",
            base64(tmpfiles.as_bytes())
        ));
    })?;
    let outcome = vm
        .wait_ssh(Duration::from_secs(300))
        .context("sshd did not start from a key delivered as tmpfiles.extra")
        .and_then(|()| {
            let state = vm.ssh_ok("systemctl is-active sshd")?;
            ensure!(state == "active", "sshd is {state}");
            Ok(())
        });
    if outcome.is_err() {
        diagnostics(&mut vm, &artifacts, "ssh-tmpfiles");
    }
    outcome?;
    println!("iso::next ssh_tmpfiles: sshd starts on a key delivered as tmpfiles.extra");
    Ok(())
}

/// Standard base64 (the credential's wire encoding).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}
