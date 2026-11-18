//! Opt-in ISO e2e tier: DMI/override selection through the local chooser,
//! then installation and first boot of the exact produced ISO. The catalogue
//! beside the ISO determines the expected source/target; registry resolution,
//! digest and origin enforcement assertions are mandatory for installation.
//! A local --testing-tag build installs the published signed legacy payload,
//! so it cannot establish that the current branch image passes these checks.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_devkit::{Workspace, cmd, installer};
use ilex_vmctl::{Frame, Vmctl, WaitCond};
use libtest_mimic::{Failed, Trial};

use crate::common;
use crate::disk::vmtest::{self, Report};

/// The chooser/Anaconda page has to come up this soon after the ISO boots.
const WELCOME_TIMEOUT: Duration = Duration::from_secs(300);
/// Partitioning, pulling the image from the registry and
/// installing the bootloader.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(1800);
/// The installer's display mode (`VmConfig::new`).
const INSTALLER_SCREEN: (u32, u32) = (1280, 800);
/// The installer keeps everything it writes in RAM. Importing a payload from
/// the ISO's own container storage unpacked layers into its `/var/tmp` and
/// needed 16 GiB for the ~5 GB image; pulling from the registry should stream
/// the layers into the new root, so this is much smaller (see CLAUDE.md for
/// what was measured).
const INSTALLER_MEMORY: &str = "6G";
/// What a bootc root needs: the system disk of the install (sparse).
const DISK_SIZE: &str = "40G";

/// Printed on the serial console by the kickstart's `%post` once the payload
/// is installed; its absence means the installer gave up.
const INSTALL_DONE: &str = "ILEX-INSTALL-DONE";
/// Fixed artifact names: guest serial records may not choose arbitrary paths.
const INSTALLER_LOGS: &[(&str, &str)] = &[
    ("ilex-apps.log", "/var/log/anaconda/ilex-apps.log"),
    ("program.log", "/var/log/anaconda/program.log"),
    ("anaconda.log", "/var/log/anaconda/anaconda.log"),
    ("installer-apps-state.json", "/var/lib/ilex/apps/state.json"),
    (
        "installer-apps-result.txt",
        "/var/lib/ilex/apps/installer-result",
    ),
    (
        "installer-mounts-before.txt",
        "/var/lib/ilex/apps/installer-mounts-before",
    ),
    (
        "installer-mounts-after.txt",
        "/var/lib/ilex/apps/installer-mounts-after",
    ),
];

/// Test-only answers, delivered on a disk labelled `OEMDRV` (Anaconda reads
/// `ks.cfg` from it without any boot argument). Anaconda only reads the
/// shipped `interactive-defaults.ks` when there is no kickstart (without it,
/// it falls back to an RPM install from a mirror), so this includes it: the
/// `bootc` command in it, and with it the payload and the image the machine
/// tracks, are the shipped ones.
const KICKSTART: &str = r#"# Test-only: unattended answers for what interactive-defaults.ks leaves open.
%include /usr/share/anaconda/interactive-defaults.ks
graphical
lang en_US.UTF-8
keyboard us
# WebUI 90 requires a Region/City entry from Anaconda's timezone catalogue;
# bare UTC is valid for the backend but leaves the web review incomplete.
timezone Etc/UTC --utc
rootpw --lock
zerombr
clearpart --all --initlabel
autopart
poweroff

# Ask Anaconda what product it thinks it is, with its own code, and say so on
# the serial console (the harness reads serial.log).
%pre --erroronfail
python3 - > /dev/ttyS0 2>&1 <<'PY'
from pyanaconda.core.product import get_product_values
print("ILEX-PRODUCT", get_product_values())
PY
%end

%post --nochroot --erroronfail
# Preserve Anaconda-owned mounts; the preload must leave the same mount IDs.
set -e
python3 - > /dev/ttyS0 2>&1 <<'PY'
import json
from pathlib import Path
# Capture before assertions/poweroff, even if the installed system cannot boot.
for name, path in [
    ('ilex-apps.log', 'var/log/anaconda/ilex-apps.log'),
    ('program.log', 'var/log/anaconda/program.log'),
    ('anaconda.log', 'var/log/anaconda/anaconda.log'),
    ('installer-apps-state.json', 'var/lib/ilex/apps/state.json'),
    ('installer-apps-result.txt', 'var/lib/ilex/apps/installer-result'),
    ('installer-mounts-before.txt', 'var/lib/ilex/apps/installer-mounts-before'),
    ('installer-mounts-after.txt', 'var/lib/ilex/apps/installer-mounts-after'),
]:
    try:
        text = (Path('/mnt/sysroot') / path).read_text(errors='replace')
        print('ILEX-INSTALLER-LOG ' + json.dumps({'name': name, 'text': text}), flush=True)
    except OSError as error:
        print(f'ILEX-INSTALLER-LOG-ERROR {name}: {error}', flush=True)
state = Path('/mnt/sysroot/var/lib/ilex/apps')
before = (state / 'installer-mounts-before').read_text()
after = (state / 'installer-mounts-after').read_text()
assert before == after, f'preload changed runtime mounts: before={before!r}, after={after!r}'
print('ILEX-PRELOAD-MOUNTS-CLEAN')
PY
echo ILEX-INSTALL-DONE > /dev/ttyS0
%end
"#;

/// Check the whole tier before any trial starts a VM or resolves a registry tag.
pub fn preflight() -> Result<()> {
    for tool in [
        "xorriso",
        "qemu-system-x86_64",
        "qemu-img",
        "mkfs.ext4",
        "skopeo",
        "ssh",
        "ssh-keygen",
    ] {
        cmd::require(tool).context(
            "iso:: host prerequisites: install xorriso, QEMU (including qemu-img), \
             e2fsprogs, skopeo and the OpenSSH client (Ubuntu: openssh-client); \
             UEFI firmware is also required (Ubuntu: ovmf)",
        )?;
    }
    Ok(())
}

pub fn trials() -> Vec<Trial> {
    let mut trials = Vec::new();
    for (name, machine, board_only, kernel) in [
        ("welcome", None, false, false),
        ("chooser", Some("ms-7c35"), false, false),
        ("detect_board", Some("ms-7c35"), true, false),
        ("kernel_argument", Some("ms-7c35"), false, true),
        ("chooser_browser_closed", Some("ms-7c35"), false, false),
        ("chooser_browser_crashed", Some("ms-7c35"), false, false),
        ("chooser_delayed", Some("ms-7c35"), false, false),
        ("chooser_forced", Some("ms-7c35"), false, false),
        ("chooser_serial_unavailable", Some("ms-7c35"), false, false),
        ("chooser_serial_backpressure", Some("ms-7c35"), false, false),
    ] {
        trials.push(Trial::test(format!("iso::{name}"), move || {
            boot_selection(name, machine, board_only, kernel)
                .map_err(|e| Failed::from(format!("{e:#}")))
        }));
    }
    trials.push(Trial::test("iso::install", || {
        run().map_err(|e| Failed::from(format!("{e:#}")))
    }));
    trials
}

fn table(iso: &Path) -> Result<installer::Table> {
    let table: installer::Table =
        serde_json::from_slice(&fs::read(iso.with_file_name("compositions.json"))?)?;
    if table.schema != 1 || table.repository != installer::REPOSITORY {
        bail!("invalid ISO catalogue");
    }
    Ok(table)
}

fn selection(
    table: &installer::Table,
    machine: Option<&str>,
    answer: Option<&str>,
) -> Result<String> {
    if let Some(tag) = answer {
        if !table.compositions.iter().any(|e| e.tag == tag) {
            bail!("ILEX_ISO_EXPECT is not offered: {tag}");
        }
        return Ok(tag.into());
    }
    let default = table
        .compositions
        .iter()
        .find(|e| e.tag == table.default)
        .context("missing default composition")?;
    Ok(table
        .compositions
        .iter()
        .find(|e| Some(e.machine.as_str()) == machine && e.shells == default.shells)
        .unwrap_or(default)
        .tag
        .clone())
}

fn smbios(
    table: &installer::Table,
    machine: Option<&str>,
    board_only: bool,
) -> Result<Vec<String>> {
    let Some(machine) = machine else {
        return Ok(vec![]);
    };
    let rules = &table
        .machines
        .get(machine)
        .with_context(|| format!("machine {machine} is not offered"))?
        .matches;
    if rules.is_empty() {
        bail!("machine {machine} has no DMI fixture");
    }
    let mut structures = Vec::new();
    for (vendor, model, kind) in [
        ("sys_vendor", "product_name", 1),
        ("board_vendor", "board_name", 2),
    ] {
        if kind == 1 && board_only {
            continue;
        }
        if let Some(rule) = rules
            .iter()
            .find(|r| r.contains_key(vendor) && r.contains_key(model))
        {
            structures.push(format!(
                "type={kind},manufacturer={},product={}",
                rule[vendor].replace(',', ",,"),
                rule[model].replace(',', ",,")
            ));
        }
    }
    if structures.is_empty() {
        bail!("no complete SMBIOS fixture for {machine}");
    }
    Ok(structures)
}

/// Exercise the chooser and actual initramfs/Anaconda %pre path; no registry is needed.
fn boot_selection(name: &str, machine: Option<&str>, board_only: bool, kernel: bool) -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let table = table(&iso)?;
    let work = ws.state_dir().join(format!("iso-{name}"));
    fs::create_dir_all(&work)?;
    let artifacts = qemu::artifact_dir(&ws, "iso")?;
    let detected = selection(&table, machine, None)?;
    let expected = if kernel || name == "chooser" {
        table.default.clone()
    } else {
        detected.clone()
    };
    let disk = work.join("blank.qcow2");
    blank_disk(&disk)?;
    // Trial names keep Rust's underscores. Drop the redundant chooser prefix
    // and use dashes for resource names, keeping Unix socket paths short.
    let purpose = format!(
        "iso-{}",
        name.strip_prefix("chooser_")
            .unwrap_or(name)
            .replace('_', "-")
    );
    let mut config = VmConfig::new(purpose, &disk);
    config.memory = INSTALLER_MEMORY.into();
    config.vcpus = 4;
    config.usb_keyboard = kernel;
    config.cdrom = Some(iso.clone());
    config.smbios = smbios(&table, machine, board_only)?;
    let serial_fault = name.strip_prefix("chooser_serial_");
    config.secondary_serial = serial_fault.is_some();
    if let Some(mode) = serial_fault {
        config.smbios.push(format!(
            "type=11,value=io.systemd.credential:ilex.chooser.serial-test={mode}"
        ));
    }
    if name == "chooser" || serial_fault.is_some() {
        config
            .smbios
            .push("type=11,value=io.systemd.credential:ilex.chooser.self-check=1".into());
    }
    let fault = match name {
        "chooser_browser_crashed" => Some("crash"),
        "chooser_delayed" => Some("delay"),
        "chooser_forced" => Some("hung"),
        _ => None,
    };
    if let Some(fault) = fault {
        config.smbios.push(format!(
            "type=11,value=io.systemd.credential:ilex.chooser.lifecycle={fault}"
        ));
    }
    let mut vm = Vm::boot(&ws, &config)?;
    let ctl = Vmctl::from_info(vm.info.clone());
    let serial = vm.info.dir.join(if serial_fault.is_some() {
        "serial-secondary.log"
    } else {
        "serial.log"
    });
    if kernel {
        // The instructions print before GRUB finishes drawing the menu. Its
        // countdown is the point at which it starts accepting edit keys.
        wait_serial(
            &mut vm,
            &serial,
            "The highlighted entry will be executed automatically",
        )?;
        kernel_argument(
            &iso,
            &work,
            &table.default,
            &ctl,
            &artifacts,
            &mut vm,
            &serial,
        )?;
    } else {
        let wanted = format!("ILEX-CHOOSER default={detected} offered=");
        wait_serial(&mut vm, &serial, &wanted)?;
        if name != "chooser_browser_crashed" {
            wait_welcome(&mut vm, &ctl, &artifacts)?;
        } else {
            wait_serial(&mut vm, &serial, "ILEX-CHOOSER-BROWSER READY")?;
        }
        if name == "detect_board" {
            fs::write(
                artifacts.join("detect_board-serial.txt"),
                fs::read(&serial)?,
            )?;
            return Ok(());
        }
        let shot = match name {
            "chooser" => "03-chooser-detected.png".into(),
            "welcome" => "01-chooser.png".into(),
            _ => format!("{name}-before.png"),
        };
        ctl.save_screenshot(&artifacts.join(shot))?;
        if name == "chooser" || serial_fault.is_some() {
            wait_serial(&mut vm, &serial, "ILEX-CHOOSER-CHECK OK")?;
        }
        if name == "chooser" {
            for key in ["shift+tab", "shift+tab", "up", "tab", "tab"] {
                ctl.key(key)?;
            }
            std::thread::sleep(Duration::from_secs(1));
            ctl.save_screenshot(&artifacts.join("04-chooser-generic.png"))?;
        }
        match name {
            "chooser_browser_closed" => {
                ctl.key("ctrl+shift+w")?;
                // The stock Firefox profile asks before quitting. Confirm its
                // native dialog with Enter, rather than submitting the page.
                std::thread::sleep(Duration::from_secs(1));
                ctl.key("Return")?;
            }
            "chooser_browser_crashed" => (), // The fixed credential kills Firefox.
            _ => ctl.key("Return")?,
        }
    }
    let reason = if kernel {
        "chosen at boot"
    } else if matches!(name, "chooser_browser_closed" | "chooser_browser_crashed") {
        "detected"
    } else {
        "chosen in the installer"
    };
    let wanted = format!("ILEX-COMPOSITION {expected} ({reason})");
    wait_serial(&mut vm, &serial, &wanted)?;
    let console = fs::read_to_string(&serial)?;
    if kernel && console.contains("ILEX-CHOOSER") {
        bail!("kernel override did not skip the chooser: {console}");
    }
    if !kernel {
        assert_handoff(&console, name, &artifacts)?;
    }
    if let Some(mode) = serial_fault {
        // Read the independent UART, not the unavailable/flow-stopped sink.
        // Require actual EAGAIN/ENOENT outcomes from the shipped mirror at
        // server startup, kiosk startup, HTTP self-check, gate and apply.
        for message in [
            "ILEX-CHOOSER",
            "ILEX-CHOOSER-BROWSER",
            "ILEX-CHOOSER-CHECK",
            "ILEX-CHOOSER-HANDOFF",
            "ILEX-COMPOSITION",
        ] {
            wait_serial(&mut vm, &serial, &format!("\"message\": \"{message}\""))?;
        }
        let console = fs::read_to_string(&serial)?;
        let audits = console
            .lines()
            .filter_map(|line| line.strip_prefix("ILEX-CHOOSER-SERIAL "))
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if audits.len() < 5
            || audits.iter().any(|audit| {
                audit["mode"] != mode
                    || audit["status"] != mode
                    || audit["seconds"].as_f64().unwrap_or(f64::INFINITY) >= 1.0
            })
        {
            bail!("serial fault was not exercised without blocking: {audits:?}");
        }
        fs::write(artifacts.join(format!("{name}-serial.txt")), console)?;
        fs::write(
            artifacts.join(format!("{name}-diagnostics.json")),
            serde_json::to_vec_pretty(&audits)?,
        )?;
    }
    fs::write(artifacts.join(format!("{name}-serial.txt")), console)?;
    wait_welcome(&mut vm, &ctl, &artifacts)?;
    let screenshot = if name == "welcome" {
        "02-welcome.png".into()
    } else if name == "chooser" {
        "chooser-welcome.png".into()
    } else {
        format!("{name}-welcome.png")
    };
    ctl.save_screenshot(&artifacts.join(&screenshot))?;
    println!(
        "{wanted}; screenshot {}",
        artifacts.join(screenshot).display()
    );
    Ok(())
}

/// ExecStartPre rechecks the actual old PID identities and logind sessions at
/// Anaconda's startup boundary. A stale success marker alone cannot pass.
fn assert_handoff(console: &str, name: &str, artifacts: &Path) -> Result<()> {
    let marker = "ILEX-CHOOSER-HANDOFF ";
    let start = console
        .find(marker)
        .context("Anaconda started without the session teardown gate")?;
    let composition = console
        .find("ILEX-COMPOSITION ")
        .context("no composition line")?;
    if start >= composition {
        bail!("session teardown was not verified before Anaconda's %pre");
    }
    let line = console[start + marker.len()..]
        .lines()
        .next()
        .unwrap()
        .trim();
    let audit: serde_json::Value = serde_json::from_str(line)?;
    fs::write(artifacts.join(format!("{name}-handoff.json")), line)?;
    if audit["live"] != 0
        || audit["old_sessions"] != 0
        || audit["pids"].as_array().is_none_or(Vec::is_empty)
        || audit["sessions"].as_array().is_none_or(Vec::is_empty)
    {
        bail!("old session/processes not proven gone: {audit}");
    }
    if name == "chooser_delayed"
        && (audit["test"] != "delay"
            || !console.contains("ILEX-CHOOSER-FAULT delay=4 both-live=1")
            || audit["cleanup_seconds"].as_f64().unwrap_or(0.0) < 4.0
            || audit["forced"] != false)
    {
        bail!("the launcher did not wait for delayed cleanup: {audit}");
    }
    if name == "chooser_forced" && (audit["test"] != "hung" || audit["forced"] != true) {
        bail!("hung session did not exercise bounded escalation: {audit}");
    }
    if name == "chooser_browser_crashed"
        && (!console.contains("ILEX-CHOOSER-FAULT browser-crash")
            || audit["exit"] != "browser"
            || audit["browser_status"] != -9)
    {
        bail!("the trial did not crash the running browser: {audit}");
    }
    if name == "chooser_browser_closed"
        && (audit["exit"] != "browser" || audit["browser_status"] != 0)
    {
        bail!("the real quit shortcut did not close Firefox: {audit}");
    }
    Ok(())
}

fn wait_serial(vm: &mut Vm, serial: &Path, wanted: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let console = fs::read_to_string(serial).unwrap_or_default();
        if console.contains(wanted) {
            return Ok(());
        }
        if console.contains("ILEX-CHOOSER-CHECK FAILED:")
            || console.contains("ILEX-CHOOSER-LIFECYCLE FAILED:")
        {
            bail!("chooser HTTP self-check failed; serial: {console}");
        }
        if started.elapsed() > WELCOME_TIMEOUT || vm.exited()?.is_some() {
            bail!("missing {wanted}; serial: {console}");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Edit the produced ISO's GRUB entry with actual keyboard input. Read its
/// generated config to find the linux line; never modify or repack the ISO.
fn kernel_argument(
    iso: &Path,
    work: &Path,
    tag: &str,
    ctl: &Vmctl,
    artifacts: &Path,
    vm: &mut Vm,
    serial: &Path,
) -> Result<()> {
    installer::validate_tag(tag)?;
    // The caller waited for the menu countdown. Stop it before reading the
    // ISO, so even slow storage cannot let the default entry boot mid-edit.
    grub_key(ctl, "up")?;
    let grub = work.join("grub.cfg");
    cmd::run(
        Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(iso)
            .args(["-extract", "/EFI/BOOT/grub.cfg"])
            .arg(&grub),
    )?;
    let original = fs::read_to_string(&grub)?;
    let entry = original
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("menuentry "))
        .skip(1)
        .take_while(|l| l.trim() != "}")
        .collect::<Vec<_>>();
    let line = entry
        .iter()
        .position(|l| {
            l.trim_start().starts_with("linux ") || l.trim_start().starts_with("linuxefi ")
        })
        .context("no linux line in ISO's first GRUB entry")?;
    let last_line = entry
        .iter()
        .rfind(|l| !l.trim().is_empty())
        .context("empty GRUB entry")?;
    grub_key(ctl, "e")?;
    // Firmware can spend seconds drawing the editor. Keys sent before its
    // final command is visible are lost, including the cursor movement.
    wait_serial(vm, serial, last_line.trim())?;
    grub_key(ctl, "ctrl+home")?;
    // GRUB inserts setparams and a blank line before the entry's commands.
    for _ in 0..line + 2 {
        grub_key(ctl, "down")?;
    }
    grub_key(ctl, "end")?;
    for character in
        format!(" ilex.composition=invalid-first-choice ilex.composition={tag}").chars()
    {
        let key = if character == ' ' {
            "space".into()
        } else {
            character.to_string()
        };
        grub_key(ctl, &key)?;
    }
    std::thread::sleep(Duration::from_millis(500));
    ctl.save_screenshot(&artifacts.join("05-grub-edit.png"))?;
    grub_key(ctl, "f10")?; // GRUB's single-key equivalent of Ctrl+X.
    Ok(())
}

/// GRUB polls firmware input and drops the desktop typing path's 10 ms key
/// presses. Use vmctl's QMP transport with 100 ms physical key holds and a gap.
fn grub_key(ctl: &Vmctl, combo: &str) -> Result<()> {
    let keys = ilex_vmctl::keys::parse_combo(combo)?
        .into_iter()
        .map(|key| serde_json::json!({"type": "qcode", "data": key}))
        .collect::<Vec<_>>();
    ilex_vmctl::qmp::Qmp::connect(&ctl.info.qmp_sock)?.execute(
        "send-key",
        serde_json::json!({"keys": keys, "hold-time": 100}),
    )?;
    std::thread::sleep(Duration::from_millis(250));
    Ok(())
}

fn run() -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let table = table(&iso)?;
    let machine = std::env::var("ILEX_ISO_MACHINE").ok();
    let answer = std::env::var("ILEX_ISO_EXPECT").ok();
    let chosen = selection(&table, machine.as_deref(), answer.as_deref())?;
    let target = format!(
        "{}:{}",
        installer::REPOSITORY,
        table.testing_tag.as_deref().unwrap_or(&chosen)
    );
    if let Ok(asserted) = std::env::var("ILEX_ISO_TARGET")
        && asserted != target
    {
        bail!("ILEX_ISO_TARGET={asserted} disagrees with the ISO catalogue's {target}");
    }
    let digest = published_digest(&target)?;
    if let Ok(verified) = std::env::var("ILEX_ISO_DIGEST") {
        require_digest(&verified)?;
        if verified != digest {
            bail!(
                "installer target {target} is {digest}, but signature verification recorded {verified}"
            );
        }
    }
    println!("installer target: {target} at {digest}");
    let artifacts = qemu::artifact_dir(&ws, "iso")?;
    let work = ws.state_dir().join("iso-test");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let mut report = Report::default();

    let disk = work.join("ilex.qcow2");
    let vars = work.join("OVMF_VARS.fd");
    let installed = unattended_install(
        &ws,
        &iso,
        (&disk, &vars),
        &work,
        &artifacts,
        (&table, machine.as_deref(), &chosen),
        &mut report,
    );
    if installed {
        installed_system(
            &ws,
            &disk,
            &vars,
            &artifacts,
            (&target, &digest, &chosen),
            &mut report,
        )?;
    }
    println!("artifacts: {}", artifacts.display());
    report.finish()
}

fn iso_path() -> Result<PathBuf> {
    let Some(path) = std::env::var_os("ILEX_ISO").filter(|p| !p.is_empty()) else {
        bail!("ILEX_ISO is not set: run `cargo xtask test iso::`, which builds the ISO first");
    };
    let path = PathBuf::from(path);
    if !path.is_file() {
        bail!("ILEX_ISO={} is not a file", path.display());
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// 1. Anaconda welcome screen
// ---------------------------------------------------------------------------

/// Wait for a settled chooser or Anaconda page (both use WebUI's stylesheet).
fn wait_welcome(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    let started = Instant::now();
    let mut previous = None;
    let frame = loop {
        if let Some(status) = vm.exited()? {
            bail!("the installer VM exited with {status} before Anaconda came up");
        }
        if let Ok(frame) = ctl.screenshot() {
            let fp = frame.fingerprint();
            // Require the web interface's blue masthead and light content,
            // rather than Firefox's blank startup window or the boot splash.
            // Two identical frames three seconds apart: the page has settled.
            if (frame.width, frame.height) == INSTALLER_SCREEN
                && is_webui(&frame)
                && previous == Some(fp)
            {
                break frame;
            }
            previous = Some(fp);
        }
        if started.elapsed() > WELCOME_TIMEOUT {
            let _ = ctl.save_screenshot(&artifacts.join("01-anaconda-welcome-failure.png"));
            bail!("the installer page did not appear within {WELCOME_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_secs(3));
    };
    println!("Installer page settled ({}x{})", frame.width, frame.height);
    Ok(())
}

/// The stock web interface has a blue masthead above a light page. Sample a
/// whole row so translated white title text and the logo do not matter.
/// This only times screenshots; selection assertions use the serial console.
fn is_webui(frame: &Frame) -> bool {
    if frame.width == 0 || frame.height == 0 || !is_light(frame) {
        return false;
    }
    let row = frame.height as usize / 24;
    let width = frame.width as usize;
    let pixels = &frame.rgb[row * width * 3..(row + 1) * width * 3];
    let blue = pixels
        .as_chunks::<3>()
        .0
        .iter()
        .filter(|[r, g, b]| {
            u16::from(*b) > u16::from(*r) + 30 && u16::from(*b) > u16::from(*g) + 15
        })
        .count();
    blue * 4 > width * 3
}

/// Is the most common colour of the picture light (Anaconda's window)?
fn is_light(frame: &Frame) -> bool {
    use std::collections::HashMap;
    let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
    for px in frame.rgb.as_chunks::<3>().0.iter().step_by(97) {
        *counts.entry(*px).or_default() += 1;
    }
    let Some((colour, n)) = counts.into_iter().max_by_key(|(_, n)| *n) else {
        return false;
    };
    let total = (frame.rgb.len() / 3).div_ceil(97) as u32;
    let luma = (u32::from(colour[0]) * 3 + u32::from(colour[1]) * 6 + u32::from(colour[2])) / 10;
    luma > 200 && n * 5 > total
}

// ---------------------------------------------------------------------------
// 2. Unattended install
// ---------------------------------------------------------------------------

/// Run the installer to completion. Returns whether the disk is installed.
fn unattended_install(
    ws: &Workspace,
    iso: &Path,
    storage: (&Path, &Path),
    work: &Path,
    artifacts: &Path,
    choice: (&installer::Table, Option<&str>, &str),
    report: &mut Report,
) -> bool {
    let (disk, vars) = storage;
    let (table, machine, chosen) = choice;
    let install = || -> Result<()> {
        let timeout = std::env::var("ILEX_ISO_INSTALL_TIMEOUT")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()
            .context("ILEX_ISO_INSTALL_TIMEOUT must be a positive number of seconds")?
            .map_or(INSTALL_TIMEOUT, Duration::from_secs);
        if timeout.is_zero() {
            bail!("ILEX_ISO_INSTALL_TIMEOUT must be positive");
        }
        println!("     installer time limit: {timeout:?}");
        blank_disk(disk)?;
        let oemdrv = kickstart_disk(work)?;
        let mut config = VmConfig::new("iso-install", disk);
        config.snapshot = false;
        config.memory = INSTALLER_MEMORY.into();
        config.vcpus = 4;
        config.cdrom = Some(iso.to_path_buf());
        config.extra_disks = vec![oemdrv];
        config.efi_vars = Some(vars.to_path_buf());
        config.smbios = smbios(table, machine, false)?;
        let mut vm = Vm::boot(ws, &config)?;
        let ctl = Vmctl::from_info(vm.info.clone());
        let serial = vm.info.dir.join("serial.log");

        let started = Instant::now();
        let mut last_shot = started;
        let status = loop {
            if let Some(status) = vm.exited()? {
                break status;
            }
            if last_shot.elapsed() > Duration::from_secs(20) {
                // The progress page hides the blue masthead used by welcome.
                // Keep its light, nonblank content rather than the startup
                // console or shutdown screen.
                if let Ok(png) = ctl.screenshot_png()
                    && Frame::decode(&png).is_ok_and(|f| is_light(&f) && !f.is_blank())
                {
                    let _ = fs::write(artifacts.join("06-installing.png"), png);
                }
                last_shot = Instant::now();
            }
            if started.elapsed() > timeout {
                let _ = ctl.save_screenshot(&artifacts.join("02-install-timeout.png"));
                let console = fs::read_to_string(&serial).unwrap_or_default();
                let _ = save_installer_serial(artifacts, &console);
                bail!(
                    "the install did not finish within {timeout:?} (serial log: {})",
                    serial.display()
                );
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        println!(
            "     installer finished after {:?} with {status}",
            started.elapsed()
        );
        let console = fs::read_to_string(&serial).unwrap_or_default();
        let preload_log = save_installer_serial(artifacts, &console)?;
        // Diagnostic JSON can quote kickstart text, including these markers.
        // Only actual control lines may establish installation completion.
        let mut control = console
            .lines()
            .filter(|line| !line.starts_with("ILEX-INSTALLER-LOG"));
        if control.clone().any(|line| line.contains("ILEX-CHOOSER")) {
            bail!("OEMDRV kickstart did not skip the chooser: {console}");
        }
        let reason = if std::env::var_os("ILEX_ISO_EXPECT").is_some() {
            "chosen in the installer"
        } else if machine.is_some() {
            "detected"
        } else {
            "no machine configuration matches this computer"
        };
        if !control
            .clone()
            .any(|line| line.contains(&format!("ILEX-COMPOSITION {chosen} ({reason})")))
        {
            bail!("installer selected an unexpected composition: {console}");
        }
        if !status.success() {
            bail!("the installer VM exited with {status}");
        }
        // A failed install also powers the machine off (`poweroff`); only the
        // `%post` of a finished one prints this.
        if !control.clone().any(|line| line.trim() == INSTALL_DONE) {
            bail!(
                "the installer powered off without finishing the install (no {INSTALL_DONE} on the serial console; see 06-installing.png)"
            );
        }
        if !preload_log {
            bail!("installer did not mirror its preload log to serial");
        }
        if !control
            .clone()
            .any(|line| line.trim() == "ILEX-PRELOAD-MOUNTS-CLEAN")
        {
            bail!("installer did not verify cleanup of the preload's runtime mounts");
        }
        // The kickstart's `%pre` printed Anaconda's own idea of the product.
        let product = control
            .find_map(|l| {
                l.split_once("ILEX-PRODUCT ")
                    .map(|(_, rest)| rest.trim().to_string())
            })
            .context("the installer printed no ILEX-PRODUCT line on the serial console")?;
        println!("     installer product: {product}");
        if !(product.contains("is_final_release=True") && product.contains("name='ilex'")) {
            bail!("the installer does not identify as a final ilex release: {product}");
        }
        Ok(())
    };
    let result = install();
    let ok = result.is_ok();
    report.check("unattended install from the ISO", result);
    ok
}

/// A sparse blank qcow2 to install onto.
fn blank_disk(path: &Path) -> Result<()> {
    cmd::require("qemu-img")?;
    let _ = fs::remove_file(path);
    cmd::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(path)
            .arg(DISK_SIZE),
    )
}

/// A small disk labelled `OEMDRV` holding `ks.cfg`.
fn kickstart_disk(work: &Path) -> Result<PathBuf> {
    let dir = work.join("oemdrv");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let mut answers = String::new();
    if let Ok(tag) = std::env::var("ILEX_ISO_EXPECT") {
        installer::validate_tag(&tag)?;
        answers = format!(
            "%pre --erroronfail\nmkdir -p /run/ilex-installer\nprintf '%s\\n' '{tag}' > /run/ilex-installer/choice\n%end\n"
        );
    }
    fs::write(dir.join("ks.cfg"), answers + KICKSTART)?;
    let image = work.join("oemdrv.img");
    let _ = fs::remove_file(&image);
    fs::File::create(&image)?.set_len(16 << 20)?;
    cmd::require("mkfs.ext4")?;
    cmd::run(
        Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "OEMDRV", "-d"])
            .arg(&dir)
            .arg(&image),
    )?;
    Ok(image)
}

// ---------------------------------------------------------------------------
// 3. The installed machine
// ---------------------------------------------------------------------------

fn installed_system(
    ws: &Workspace,
    disk: &Path,
    vars: &Path,
    artifacts: &Path,
    expected: (&str, &str, &str),
    report: &mut Report,
) -> Result<()> {
    let (target, digest, chosen) = expected;
    // Same firmware variables as the installer: the boot entry it wrote is
    // there. A throwaway overlay keeps the installed disk pristine.
    let mut config = VmConfig::new("iso", disk);
    config.efi_vars = Some(vars.to_path_buf());
    let mut vm = match vmtest::boot(ws, config) {
        Ok(vm) => vm,
        Err(e) => {
            report.check("installed system boots and accepts SSH", Err(e));
            return Ok(());
        }
    };
    let ctl = Vmctl::from_info(vm.info.clone());
    report.check("installed system boots and accepts SSH", Ok(()));
    // Collect before assertions: a failed preload must retain its own output,
    // not just the first-boot journal (the apps timer is suppressed here).
    report.check("installer logs are saved", installer_logs(&vm, artifacts));

    report.check(
        "bootc tracks the target image",
        bootc_status(&vm, artifacts, target, digest),
    );
    report.check("system reaches `running`", system_running(&vm));
    report.check("no failed units", no_failed_units(&vm));
    report.check("GDM is up", gdm(&mut vm, &ctl, artifacts));
    report.check("installer preloaded the composition's system apps", (|| {
        let evidence = vm.ssh_ok(r#"python3 - <<'PY'
import json,pathlib,subprocess
image=json.load(open('/usr/share/ilex/image.json'))
machine=json.load(open('/usr/share/ilex/machine.json'))
wanted=set()
for entry in [*image['shells'],machine]:wanted.update(entry.get('apps',{}).get('flatpak',[]))
if wanted:
    result=pathlib.Path('/var/lib/ilex/apps/installer-result').read_text().strip()
    assert result=='0',f'installer preload exited {result}; see ilex-apps.log'
    state=json.load(open('/var/lib/ilex/apps/state.json'))
    assert state['result']=='complete',state
    for app in wanted:
        assert subprocess.check_output(['flatpak','info','--system','--show-origin',app],text=True).strip()=='flathub',app
    print(json.dumps(state,indent=2))
else:
    print('Published payload declares no system Flatpaks; preload is not applicable to this payload.')
PY"#)?;
        qemu::write_artifact(artifacts, "preloaded-apps.txt", &evidence)?;
        Ok(())
    })());
    report.check(
        "nonsecret versioned target setup handoff",
        (|| {
            let record = vm.ssh_ok("cat /etc/ilex/setup.json")?;
            qemu::write_artifact(artifacts, "setup.json", &record)?;
            let value: serde_json::Value = serde_json::from_str(&record)?;
            if value["schema"] != 1
                || value["composition"] != chosen
                || value["image"] != target
                || !value["profile"]["id"].is_null()
                || !value["profile"]["schema"].is_null()
                || value["security"]["tpm"]["requested"] != "unspecified"
                || value["security"]["tpm"]["capability"] != "unknown"
                || value["security"]["tpm"]["status"] != "not-configured"
            {
                bail!("unexpected setup record: {record}");
            }
            Ok(())
        })(),
    );
    report.check(
        "origin was already verified on first boot",
        (|| {
            let log = vm.ssh_ok("journalctl -b -u ilex-sigpolicy.service --no-pager")?;
            qemu::write_artifact(artifacts, "sigpolicy-journal.txt", &log)?;
            if !log.contains("already") || !log.contains("verified") {
                bail!("sigpolicy did not report an already verified origin: {log}");
            }
            Ok(())
        })(),
    );
    report.check(
        "verified bootc upgrade check",
        vm.ssh_ok("timeout 300 bootc upgrade --check")
            .map(|out| println!("{out}")),
    );
    if report.has_failures() {
        let _ = ctl.save_screenshot(&artifacts.join("failure.png"));
        if let Ok(journal) = vm.ssh("journalctl -b --no-pager -n 500 -o short-monotonic") {
            let _ = qemu::write_artifact(
                artifacts,
                "failure-journal.txt",
                &String::from_utf8_lossy(&journal.stdout),
            );
        }
    }
    Ok(())
}

/// The installer sends these before shutdown; SSH collection later refreshes
/// them, but cannot be the only source when installation or first boot fails.
fn save_installer_serial(artifacts: &Path, console: &str) -> Result<bool> {
    qemu::write_artifact(artifacts, "installer-serial.txt", console)?;
    let mut preload_log = false;
    for record in console.lines().filter_map(|line| {
        serde_json::from_str::<serde_json::Value>(line.strip_prefix("ILEX-INSTALLER-LOG ")?)
            .ok()
    }) {
        if let (Some(name), Some(text)) = (record["name"].as_str(), record["text"].as_str())
            && INSTALLER_LOGS.iter().any(|(allowed, _)| *allowed == name)
        {
            qemu::write_artifact(artifacts, name, text)?;
            preload_log |= name == "ilex-apps.log";
        }
    }
    Ok(preload_log)
}

fn installer_logs(vm: &Vm, artifacts: &Path) -> Result<()> {
    let mut failures = Vec::new();
    for &(name, path) in INSTALLER_LOGS {
        match vm.ssh_ok(&format!("cat {path}")) {
            Ok(log) => qemu::write_artifact(artifacts, name, &log)?,
            Err(error) => {
                qemu::write_artifact(
                    artifacts,
                    &format!("{name}.error.txt"),
                    &format!("{error:#}"),
                )?;
                // Older payloads can have no apps helper or state. Anaconda's
                // logs, including the shipped %post's log, must still exist.
                if path.starts_with("/var/log/anaconda/") {
                    failures.push(format!("{path}: {error:#}"));
                }
            }
        }
    }
    if !failures.is_empty() {
        bail!("could not collect installer logs: {}", failures.join("\n"));
    }
    Ok(())
}

/// Both the booted deployment and its tracked origin must match the verified
/// composition; the booted digest must be exactly the one resolved before install.
fn bootc_status(vm: &Vm, artifacts: &Path, wanted: &str, expected_digest: &str) -> Result<()> {
    let json = vm.ssh_ok("bootc status --json")?;
    let _ = qemu::write_artifact(artifacts, "bootc-status.json", &json);
    let _ = vm.ssh("bootc status").map(|o| {
        qemu::write_artifact(
            artifacts,
            "bootc-status.txt",
            &String::from_utf8_lossy(&o.stdout),
        )
    });
    let status: serde_json::Value = serde_json::from_str(&json).context("parsing bootc status")?;
    let booted = &status["status"]["booted"];
    if booted.is_null() {
        bail!("bootc status shows no booted deployment: {json}");
    }
    let image = &booted["image"];
    let name = image["image"]["image"].as_str().unwrap_or("?");
    let transport = image["image"]["transport"].as_str().unwrap_or("?");
    let digest = image["imageDigest"].as_str().unwrap_or("?");
    println!("     booted image: {transport}:{name} ({digest})");
    println!("     spec.image: {}", status["spec"]["image"]);
    if name != wanted || transport != "registry" {
        bail!("the machine tracks {name}, expected {wanted}");
    }
    let origin = &status["spec"]["image"];
    if origin["image"].as_str() != Some(wanted) || origin["transport"].as_str() != Some("registry")
    {
        bail!("the tracked origin is {origin}, expected registry:{wanted}");
    }
    if digest != expected_digest {
        bail!("the machine booted {digest}, but verified {wanted} was {expected_digest}");
    }
    if origin["signature"] != "containerPolicy" {
        bail!("installed origin is not enforcing: {origin}");
    }
    println!("     {wanted} is the verified published digest {expected_digest}");
    Ok(())
}

fn require_digest(digest: &str) -> Result<()> {
    let hash = digest.strip_prefix("sha256:").unwrap_or_default();
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!("invalid installer digest {digest:?}: expected sha256 and 64 lowercase hex digits");
    }
    Ok(())
}

/// Resolve before booting any VM. Missing/denied/unreadable targets cannot
/// provide installation evidence, and must never silently disable the assertion.
fn published_digest(reference: &str) -> Result<String> {
    let digest = cmd::output(Command::new("skopeo").args([
        "inspect",
        "--no-tags",
        "--format",
        "{{.Digest}}",
        &format!("docker://{reference}"),
    ]))
    .with_context(|| format!("resolving installer target {reference} before installation"))?;
    require_digest(&digest)?;
    Ok(digest)
}

fn system_running(vm: &Vm) -> Result<()> {
    let out = vm.ssh("timeout 300 systemctl is-system-running --wait")?;
    let state = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if state == "running" {
        Ok(())
    } else {
        bail!("system state is `{state}`")
    }
}

fn no_failed_units(vm: &Vm) -> Result<()> {
    let failed = vm.ssh_ok("systemctl --failed --no-legend --plain")?;
    if failed.is_empty() {
        Ok(())
    } else {
        bail!("failed units:\n{failed}")
    }
}

/// GDM is active with a greeter session, and its login screen is drawn.
fn gdm(vm: &mut Vm, ctl: &Vmctl, artifacts: &Path) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(300);
    vm.wait_for(
        "gdm.service to be active",
        "systemctl is-active gdm",
        TIMEOUT,
    )?;
    vm.wait_for(
        "a GDM greeter session",
        "loginctl list-sessions --no-legend | grep -w greeter",
        TIMEOUT,
    )?;
    let cond = WaitCond {
        nonblank: true,
        size: Some(INSTALLER_SCREEN),
        stable: Some(Duration::from_secs(5)),
        ..WaitCond::default()
    };
    ctl.wait_screen(&cond, TIMEOUT)
        .context("waiting for GDM's login screen")?;
    let path = artifacts.join("07-gdm.png");
    let frame = ctl.save_screenshot(&path)?;
    println!(
        "     screenshot {} ({}x{})",
        path.display(),
        frame.width,
        frame.height
    );
    Ok(())
}
