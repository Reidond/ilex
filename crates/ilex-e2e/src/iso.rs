//! Opt-in ISO e2e tier: actual DMI/override selection in Anaconda's web interface,
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

/// Anaconda's welcome screen has to come up this soon after the ISO boots.
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

%post --nochroot
echo ILEX-INSTALL-DONE > /dev/ttyS0
%end
"#;

pub fn trials() -> Vec<Trial> {
    let mut trials = Vec::new();
    for (name, machine, board_only, kernel) in [
        ("welcome", None, false, false),
        ("detect", Some("ms-7c35"), false, false),
        ("detect_board", Some("ms-7c35"), true, false),
        ("kernel_argument", Some("ms-7c35"), false, true),
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

/// Exercise the actual initramfs/Anaconda %pre path; no registry is needed.
fn boot_selection(name: &str, machine: Option<&str>, board_only: bool, kernel: bool) -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let table = table(&iso)?;
    let work = ws.state_dir().join(format!("iso-{name}"));
    fs::create_dir_all(&work)?;
    let artifacts = qemu::artifact_dir(&ws, "iso")?;
    let expected = selection(&table, machine, kernel.then_some(table.default.as_str()))?;
    let boot_iso = if kernel {
        kernel_iso(&iso, &work, &table.default)?
    } else {
        iso.clone()
    };
    let disk = work.join("blank.qcow2");
    blank_disk(&disk)?;
    let mut config = VmConfig::new(format!("iso-{name}"), &disk);
    config.memory = INSTALLER_MEMORY.into();
    config.vcpus = 4;
    config.cdrom = Some(boot_iso);
    config.smbios = smbios(&table, machine, board_only)?;
    let mut vm = Vm::boot(&ws, &config)?;
    let ctl = Vmctl::from_info(vm.info.clone());
    let serial = vm.info.dir.join("serial.log");
    let reason = if kernel {
        "chosen at boot"
    } else if machine.is_some() {
        "detected"
    } else {
        "no machine configuration matches this computer"
    };
    let wanted = format!("ILEX-COMPOSITION {expected} ({reason})");
    let started = Instant::now();
    loop {
        let console = fs::read_to_string(&serial).unwrap_or_default();
        if console.contains(&wanted) {
            fs::write(artifacts.join(format!("{name}-serial.txt")), &console)?;
            break;
        }
        if started.elapsed() > WELCOME_TIMEOUT || vm.exited()?.is_some() {
            bail!("missing {wanted}; serial: {console}");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    wait_welcome(&mut vm, &ctl, &artifacts)?;
    let screenshot = match name {
        "welcome" => "01-welcome.png",
        "detect" => "02-welcome.png",
        "detect_board" => "03-board-detected.png",
        _ => "05-kernel-argument.png",
    };
    ctl.save_screenshot(&artifacts.join(screenshot))?;
    println!(
        "{wanted}; screenshot {}",
        artifacts.join(screenshot).display()
    );
    Ok(())
}

/// Test-only ISO copy with a kernel argument in GRUB. The installation trial
/// always boots the untouched produced ISO. This needs no GUI input backend.
fn kernel_iso(iso: &Path, work: &Path, tag: &str) -> Result<PathBuf> {
    installer::validate_tag(tag)?;
    let grub = work.join("grub.cfg");
    cmd::run(
        Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(iso)
            .args(["-extract", "/EFI/BOOT/grub.cfg"])
            .arg(&grub),
    )?;
    let original = fs::read_to_string(&grub)?;
    let mut changed = false;
    let edited = original
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("linux ") || line.trim_start().starts_with("linuxefi ")
            {
                changed = true;
                format!("{line} ilex.composition=invalid-first-choice ilex.composition={tag}\n")
            } else {
                format!("{line}\n")
            }
        })
        .collect::<String>();
    if !changed {
        bail!("no linux line in ISO's GRUB configuration");
    }
    fs::write(&grub, edited)?;
    let output = work.join("kernel-argument.iso");
    if output.exists() {
        fs::remove_file(&output)?;
    }
    cmd::run(
        Command::new("xorriso")
            .arg("-indev")
            .arg(iso)
            .arg("-outdev")
            .arg(&output)
            .args(["-boot_image", "any", "replay", "-map"])
            .arg(&grub)
            .arg("/EFI/BOOT/grub.cfg"),
    )?;
    Ok(output)
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

/// Boot the ISO interactively and save Anaconda's welcome screen.
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
            bail!("Anaconda's welcome screen did not appear within {WELCOME_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_secs(3));
    };
    println!(
        "Anaconda web interface settled ({}x{})",
        frame.width, frame.height
    );
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
                // Keep the last drawn web page, not a startup console or the
                // shutdown screen (the machine goes black as it powers off).
                if let Ok(png) = ctl.screenshot_png()
                    && Frame::decode(&png).is_ok_and(|f| is_webui(&f))
                {
                    let _ = fs::write(artifacts.join("06-installing.png"), png);
                }
                last_shot = Instant::now();
            }
            if started.elapsed() > INSTALL_TIMEOUT {
                let _ = ctl.save_screenshot(&artifacts.join("02-install-timeout.png"));
                bail!(
                    "the install did not finish within {INSTALL_TIMEOUT:?} (serial log: {})",
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
        let reason = if std::env::var_os("ILEX_ISO_EXPECT").is_some() {
            "chosen in the installer"
        } else if machine.is_some() {
            "detected"
        } else {
            "no machine configuration matches this computer"
        };
        if !console.contains(&format!("ILEX-COMPOSITION {chosen} ({reason})")) {
            bail!("installer selected an unexpected composition: {console}");
        }
        let _ = fs::write(artifacts.join("installer-serial.txt"), &console);
        if !status.success() {
            bail!("the installer VM exited with {status}");
        }
        // A failed install also powers the machine off (`poweroff`); only the
        // `%post` of a finished one prints this.
        if !console.contains(INSTALL_DONE) {
            bail!(
                "the installer powered off without finishing the install (no {INSTALL_DONE} on the serial console; see 06-installing.png)"
            );
        }
        // The kickstart's `%pre` printed Anaconda's own idea of the product.
        let product = console
            .lines()
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

    report.check(
        "bootc tracks the target image",
        bootc_status(&vm, artifacts, target, digest),
    );
    report.check("system reaches `running`", system_running(&vm));
    report.check("no failed units", no_failed_units(&vm));
    report.check("GDM is up", gdm(&mut vm, &ctl, artifacts));
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
