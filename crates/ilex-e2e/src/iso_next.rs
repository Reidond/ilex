//! Opt-in tier for the next installer's live ISO (`iso::next…`, built by
//! `cargo ilexctl iso --next` or named by `ILEX_ISO_NEXT`). It is named
//! explicitly: `iso::` alone is the Anaconda release gate and must not boot an
//! ISO of another product.
//!
//! The live ISO has no SSH server: the session prints marker lines on the
//! serial console (`ILEX-INSTALLER-SESSION …` from the session script,
//! `ILEX-INSTALLER page=…` once the window is mapped, `ILEX-INSTALLER-STATS …`
//! after the page has settled) and the harness takes a QMP screenshot of what
//! was actually drawn.
//!
//! * `iso::next_boot`: UEFI with Secure Boot required on KVM (the secure
//!   fixture path; unqualified hosts fail), default boot entry.
//! * `iso::next_basic_graphics`: the same, choosing "Install ilex (basic
//!   graphics)" in GRUB with real key input; GTK must report its cairo
//!   renderer.
//! * `iso::next_fallback`: a fixed test credential makes the session fail; the
//!   unit gives up after three failures and tells the person on tty1.

use std::collections::BTreeMap;
use std::fs;
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

pub fn trials() -> Vec<Trial> {
    vec![
        Trial::test("iso::next_boot", || {
            boot(Entry::Normal).map_err(|e| Failed::from(format!("{e:#}")))
        }),
        Trial::test("iso::next_basic_graphics", || {
            boot(Entry::BasicGraphics).map_err(|e| Failed::from(format!("{e:#}")))
        }),
        Trial::test("iso::next_fallback", || {
            fallback().map_err(|e| Failed::from(format!("{e:#}")))
        }),
    ]
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
    // Measure whatever size the run asks for; this never promotes the host's
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
        // stops it and selects the second entry.
        wait_for(vm, &serial, GRUB_COUNTDOWN, PAGE_TIMEOUT)?;
        grub_key(&ctl, "down")?;
        ctl.save_screenshot(&artifacts.join("next-basic-graphics-grub.png"))?;
        grub_key(&ctl, "Return")?;
    }

    let console = wait_for(vm, &serial, "ILEX-INSTALLER page=", PAGE_TIMEOUT)?;
    let to_page = started.elapsed();
    let session = fields(&console, "ILEX-INSTALLER-SESSION compositor=")
        .context("the session script printed no ILEX-INSTALLER-SESSION line")?;
    let page = fields(&console, "ILEX-INSTALLER page=")
        .context("the page printed no ILEX-INSTALLER line")?;
    ensure!(
        session["graphics"] == entry.graphics(),
        "booted graphics={} but the {} entry was chosen",
        session["graphics"],
        entry.name()
    );
    ensure!(
        session["secure-boot"] == "1",
        "the guest does not report Secure Boot on: {session:?}"
    );
    ensure!(page["page"] == "placeholder", "unexpected page: {page:?}");
    ensure!(page["edition"] == "gnome", "unexpected edition: {page:?}");
    if entry == Entry::BasicGraphics {
        ensure!(
            page["renderer"] == "GskCairoRenderer",
            "basic graphics must draw with cairo, GTK chose {}",
            page["renderer"]
        );
    }
    ensure!(
        !console.contains("ILEX-INSTALLER-FALLBACK"),
        "the session fell back: {console}"
    );

    // What was actually drawn: a settled frame that is the libadwaita page.
    let frame = wait_page(vm, &ctl, artifacts, entry)?;
    let shot = artifacts.join(format!("next-{}.png", entry.name()));
    ctl.save_screenshot(&shot)?;

    let console = wait_for(vm, &serial, "ILEX-INSTALLER-STATS ", STATS_TIMEOUT)?;
    let stats = fields(&console, "ILEX-INSTALLER-STATS ").context("no memory sample")?;
    let kib = |key: &str| -> Result<u64> {
        stats
            .get(key)
            .and_then(|v| v.parse().ok())
            .with_context(|| format!("memory sample lacks {key}: {stats:?}"))
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
        "renderer": page["renderer"],
        "scale": page["scale"],
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
    Ok(())
}

/// After a failure: what the live system says about itself. The harness'
/// test key is in the guest as a credential and the image keeps sshd, so this
/// usually works; when it does not, the serial log and screenshot remain.
fn diagnostics(vm: &mut Vm, artifacts: &Path, name: &str) {
    if let Ok(console) = fs::read_to_string(vm.info.dir.join("serial.log")) {
        let _ = fs::write(
            artifacts.join(format!("next-{name}-failure-serial.txt")),
            console,
        );
    }
    let _ = Vmctl::from_info(vm.info.clone())
        .save_screenshot(&artifacts.join(format!("next-{name}-failure.png")));
    // A deadline of its own: a guest without sshd must not hang the failure path.
    let script = "systemctl --no-pager --failed; \
         systemctl --no-pager status ilex-installer-session ilex-installer-fallback; \
         loginctl list-sessions --no-pager; getenforce; \
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

/// Wait until the serial log holds `wanted`; fails fast when the guest exited
/// or the installer gave up. Returns the whole log.
fn wait_for(vm: &mut Vm, serial: &Path, wanted: &str, timeout: Duration) -> Result<String> {
    let started = Instant::now();
    loop {
        let console = fs::read_to_string(serial).unwrap_or_default();
        if console.contains(wanted) {
            return Ok(console);
        }
        if console.contains("ILEX-INSTALLER-FALLBACK") && wanted != "ILEX-INSTALLER-FALLBACK" {
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

/// `key=value` words of the last line starting with `prefix`.
fn fields(console: &str, prefix: &str) -> Option<BTreeMap<String, String>> {
    let line = console.lines().rev().find(|l| l.starts_with(prefix))?;
    Some(
        line.split_whitespace()
            .filter_map(|word| {
                let (key, value) = word.split_once('=')?;
                Some((key.to_owned(), value.to_owned()))
            })
            .collect(),
    )
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
fn wait_page(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path, entry: Entry) -> Result<Frame> {
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
            let _ =
                ctl.save_screenshot(&artifacts.join(format!("next-{}-failure.png", entry.name())));
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

/// The session unit gives up after three failures and the fallback unit says
/// so on tty1. A fixed test credential makes the session script fail.
fn fallback() -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let work = ws.state_dir().join("iso-next-fallback");
    fs::create_dir_all(&work)?;
    let disk = work.join("blank.qcow2");
    blank_disk(&disk)?;
    let mut config = VmConfig::new("iso-next-fallback", &disk);
    config.memory = "2G".into();
    config.cdrom = Some(iso);
    config
        .smbios
        .push("type=11,value=io.systemd.credential:ilex.installer.fault=session".into());
    let mut vm = Vm::boot(&ws, &config)?;
    let outcome = observe_fallback(&mut vm, &artifacts);
    if outcome.is_err() {
        diagnostics(&mut vm, &artifacts, "fallback");
    }
    outcome
}

fn observe_fallback(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    let ctl = Vmctl::from_info(vm.info.clone());
    let console = wait_for(vm, &serial, "ILEX-INSTALLER-FALLBACK", PAGE_TIMEOUT)?;
    let failures = console
        .matches("ILEX-INSTALLER-SESSION fault=session")
        .count();
    ensure!(
        failures == 3,
        "expected three session failures before the fallback, saw {failures}: {console}"
    );
    ensure!(
        !console.contains("ILEX-INSTALLER page="),
        "a page was shown although the session was made to fail: {console}"
    );
    // The message is text on tty1: the frame is not blank and not the page.
    std::thread::sleep(Duration::from_secs(3));
    let frame = ctl.save_screenshot(&artifacts.join("next-fallback.png"))?;
    ensure!(
        !page_drawn(&frame),
        "the screen shows a drawn page, not the fallback message"
    );
    fs::write(artifacts.join("next-fallback-serial.txt"), console)?;
    println!("iso::next fallback: three failures, then the message on tty1");
    Ok(())
}
