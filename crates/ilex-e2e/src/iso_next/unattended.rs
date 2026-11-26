//! `iso::next_unattended`: an installation from the real ISO with no interface.
//!
//! The ISO boots through UEFI with Secure Boot beside a target disk and a small
//! ext4 volume labelled `ILEX-ANSWERS` that holds `answers.toml`. Nothing is
//! typed and no page is drawn: at boot, `ilex-installer-answers-gate.service`
//! finds the volume (once udev has settled), the interface's session is
//! skipped, and `ilex-installer-answers.service` drives the helper (`Probe`,
//! `Plan`, `SetSecret`, `Apply`, `State`) as root, prints
//! `ILEX-INSTALLER-ANSWERS` lines, and powers the machine off when the
//! installation has succeeded. The harness follows those lines on the serial
//! console, then boots the disk it installed (without the ISO) and asserts what
//! the interactive trials assert of an installed system: the digest the plan
//! pinned, the origin that enforces the container signature policy, the
//! administrator logging in with the password and `sudo`, the answers applied,
//! the system running with nothing failed and GDM up.
//!
//! Refusals come first, each on a VM of its own that must power nothing on and
//! write nothing: an answers file that is not valid (the line and column, never
//! its text), one whose `confirm-erase` is not the disk, one that names a disk
//! by a bus path (a disk with no `/dev/disk/by-id` name), and, for the CD-ROM
//! source, one that names a disk that already holds an ilex installation
//! without `reinstall = true` (the install then runs on that disk, with it).
//! The answers volume's image is hashed before and after: the helper reads it
//! read-only.
//!
//! `iso::next_unattended_loop` runs the same thing with the ISO as a *file* on
//! another disk (GRUB's loopback and dracut's `iso-scan/filename=`, see
//! `loop_file.rs`), so that the live-medium logic is exercised on the install
//! path too: the disk that holds the file is found by its size and the probe
//! calls it the live medium; answers that name it as the target are refused
//! and the disk (the file included) is byte for byte what it was; the answers
//! that name the blank disk install, and the host disk is again unchanged
//! after the installation and the power-off.
//!
//! `iso::next_unattended_fat` reads the answers from a FAT stick labelled
//! `ILEXANSWERS` (the label a FAT volume can carry); `iso::next_unattended_late`
//! plugs a stick in after the interface is up and requires that nothing changes.
//!
//! `iso::next_unattended_repeat` installs with `reinstall = true`, then boots the
//! ISO again with the same sticks (the machine powered off after its
//! installation and the firmware boots USB first): the answers refuse
//! ("already installed"), the whole target disk is byte for byte what the
//! installation left, and answers that differ in one word are not refused for
//! that reason.
//!
//! `iso::next_unattended_handover` makes the machine fail before it has written
//! anything, in two ways (a fixed test credential): the helper cannot start, and
//! the unattended process is killed before it has asked the helper for
//! anything. In both the interface starts, so that tty1 is not left empty, and
//! the disk is untouched.
//!
//! The installation of `iso::next_unattended` goes over a whole-disk LVM
//! physical volume (a signature with no partition table), which is also refused
//! without `reinstall = true`: its `/dev/disk/by-id/lvm-pv-uuid-*` link goes
//! when the disk is wiped, so the disk must be named by its hardware's link.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::cmd;
use ilex_devkit::core::{Capability, Requirement, SecurityLaunch, VmProvider};
use ilex_devkit::installer_model::probe::Probe;
use ilex_devkit::provider::LinuxProvider;
use ilex_devkit::qemu::{self, Vm, VmConfig};

use ilex_vmctl::Vmctl;

use super::{
    Guest, PAGE_TIMEOUT, count_markers, diagnostics, fields, iso_path, loop_file, wait_for_marker,
};
use crate::common;
use crate::installer::apply::{ADMIN, Expected, PASSWORD, check_installed, gdm_screenshot};

/// The target's virtual size (`41G` for qemu-img): how the harness finds it in
/// a probe, since the id a virtio disk gets without a serial is its PCI slot.
const TARGET_BYTES: u64 = 41 << 30;
/// The unattended installation, from the pull to the power-off.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(40 * 60);
/// The ISO's VM: the pull streams into the target and the helper, bootc and
/// the page cache share the rest.
const MEMORY: &str = "6G";
const MARKER: &str = "ILEX-INSTALLER-ANSWERS";

/// Where the ISO comes from.
#[derive(Clone, Copy, PartialEq)]
enum Source {
    /// An optical drive: the VM's own disk is the target.
    Cdrom,
    /// A file on another disk: the VM's own disk is that host disk, and the
    /// target is attached beside it.
    File,
}

/// What a trial boots: its source, its files and the names it gives things.
struct Layout {
    source: Source,
    /// The host disk and the boot loader's FAT disk, built once for the
    /// trial's VMs (the file source only).
    fixture: Option<loop_file::Fixture>,
}

impl Layout {
    fn new(source: Source, root: &Path, iso: &Path) -> Result<Self> {
        let fixture = match source {
            Source::Cdrom => None,
            Source::File => Some(loop_file::build(&root.join("loop"), iso)?),
        };
        Ok(Self { source, fixture })
    }
    /// What the trial's files and artifacts are called.
    fn name(&self) -> &'static str {
        match self.source {
            Source::Cdrom => "unattended",
            Source::File => "unattended-loop",
        }
    }
    /// VM purposes stay short: they name Unix socket paths.
    fn purpose(&self) -> &'static str {
        match self.source {
            Source::Cdrom => "iso-next-unatt",
            Source::File => "iso-next-unatf",
        }
    }
    fn target(&self, root: &Path) -> PathBuf {
        root.join("target.raw")
    }
}

/// `answers.toml` for the target disk: answers that are not the defaults, so
/// that applying them is visible.
fn answers(disk: &str, confirm: &str, reinstall: bool) -> String {
    answers_named(disk, confirm, reinstall, "ilextest")
}

/// [`answers`] with a host name of its own.
fn answers_named(disk: &str, confirm: &str, reinstall: bool, host: &str) -> String {
    format!(
        r#"schema = 1

[install]
disk = "{disk}"
mode = "whole-disk"
confirm-erase = "{confirm}"
{}
[encryption]
enabled = false

[system]
keyboard = "de-latin1"
language = "de_DE.UTF-8"
time-zone = "Europe/Berlin"
host-name = "{host}"

[administrator]
name = "{ADMIN}"
password = "{}"
"#,
        if reinstall { "reinstall = true\n" } else { "" },
        // A TOML basic string: the quotes of the password are escaped.
        PASSWORD.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// A volume's image and what it was when it was made.
struct Volume {
    path: PathBuf,
    sha256: String,
}

impl Volume {
    fn new(path: PathBuf) -> Result<Self> {
        let sha256 = sha256_of(&path)?;
        Ok(Self { path, sha256 })
    }

    /// The helper reads a volume read-only: it is what it was.
    fn unchanged(&self) -> Result<()> {
        ensure!(
            sha256_of(&self.path)? == self.sha256,
            "the answers volume {} changed: the helper must read it read-only",
            self.path.display()
        );
        Ok(())
    }
}

/// A small ext4 image labelled `ILEX-ANSWERS` with `text` as `answers.toml`:
/// `mke2fs -d` fills it from a directory, no mount and no root needed.
fn answers_volume(root: &Path, text: &str) -> Result<Volume> {
    let content = root.join("answers");
    fs::create_dir_all(&content)?;
    fs::write(content.join("answers.toml"), text)?;
    let image = root.join("answers.img");
    let _ = fs::remove_file(&image);
    cmd::require("mke2fs")?;
    cmd::run(
        Command::new("mke2fs")
            .args(["-q", "-t", "ext4", "-L", "ILEX-ANSWERS", "-d"])
            .arg(&content)
            .arg(&image)
            .arg("16M"),
    )?;
    Volume::new(image)
}

/// A small FAT image labelled `label` (11 characters at most) with `text` as
/// `answers.toml`. `mkfs.fat` makes the file system and `mcopy` (mtools) writes
/// the file into the image: no mount and no root needed.
fn fat_volume(root: &Path, text: &str, label: &str) -> Result<Volume> {
    for tool in ["mkfs.fat", "mcopy"] {
        cmd::require(tool)?;
    }
    let content = root.join("answers");
    fs::create_dir_all(&content)?;
    fs::write(content.join("answers.toml"), text)?;
    let image = root.join("answers-fat.img");
    let _ = fs::remove_file(&image);
    cmd::run(
        Command::new("mkfs.fat")
            .args(["-F", "16", "-n", label, "-C"])
            .arg(&image)
            .arg("16384"),
    )?;
    cmd::run(
        Command::new("mcopy")
            .arg("-i")
            .arg(&image)
            .arg(content.join("answers.toml"))
            .arg("::answers.toml"),
    )?;
    Volume::new(image)
}

/// What a boot has: which disks, and how they are named.
#[derive(Clone, Copy, Default)]
struct Setup<'a> {
    /// The answers volume, if there is one.
    volume: Option<&'a Path>,
    /// Serial numbers on the disks, so that they have `/dev/disk/by-id` names.
    serials: bool,
    /// A target that already holds an ilex installation (a partition labelled
    /// `ilex-root`) and an EFI system partition that cannot be read (no file
    /// system in it).
    existing: bool,
    /// An empty slot a disk can be added to while the guest runs.
    hotplug: bool,
    /// The target is a whole-disk LVM physical volume: no partition table, an
    /// LVM2 label in its second sector (what `pvcreate` writes).
    lvm: bool,
    /// Leave the target as the last boot left it (an installed disk).
    keep_target: bool,
    /// The test fault the guest is given (`ilex.installer.fault`): `helper` or
    /// `answers`.
    fault: Option<&'a str>,
}

/// [`boot_once`], started again (up to twice) when the guest never ran: on a
/// Boat sandbox a VM now and then prints nothing at all on its serial console
/// for as long as it is left (QEMU's vCPU threads at 0 s of CPU: firmware never
/// started). That is the sandbox, not the installer, and a guest that has not
/// printed a byte in two and a half minutes will not: a fresh one is started,
/// and the retry is said out loud.
///
/// **A temporary workaround.** It stays until this stack is rebased onto the
/// change that fixes QEMU's audio-probe hang (PR #90), which is what the
/// silent guests are suspected to be; it is then removed, with the retry. Each
/// failed attempt's `qemu.log` and `serial.log` are kept in the artifacts
/// (never overwritten by the next attempt's, which reuses the VM's directory),
/// so that the cause can still be found.
fn boot(root: &Path, layout: &Layout, setup: Setup<'_>) -> Result<Guest> {
    const ATTEMPTS: u32 = 3;
    const ALIVE_WITHIN: Duration = Duration::from_secs(150);
    for attempt in 1..=ATTEMPTS {
        let mut guest = boot_once(root, layout, setup)?;
        let vm = guest.vm.running_mut()?;
        let serial = vm.info.dir.join("serial.log");
        let started = Instant::now();
        // `ILEX_ISO_NEXT_FORCE_SILENT=1` calls the first attempt of every boot
        // silent after three seconds, whatever it printed: how the retry and the
        // logs it keeps are exercised without waiting for the flake.
        let forced = attempt == 1 && std::env::var_os("ILEX_ISO_NEXT_FORCE_SILENT").is_some();
        loop {
            if !forced && fs::metadata(&serial).is_ok_and(|m| m.len() > 0) {
                return Ok(guest);
            }
            if let Some(status) = vm.exited()? {
                bail!("the VM exited with {status} before it printed anything");
            }
            if started.elapsed()
                > if forced {
                    Duration::from_secs(3)
                } else {
                    ALIVE_WITHIN
                }
            {
                break;
            }
            std::thread::sleep(Duration::from_secs(if forced { 1 } else { 2 }));
        }
        let kept = keep_silent_attempt(vm.info.dir.as_path(), root, layout, attempt);
        eprintln!(
            "iso::next: attempt {attempt} of {ATTEMPTS}: the guest printed nothing on its serial console in {ALIVE_WITHIN:?} (the sandbox's idle-vCPU flake; a temporary workaround); {kept}; starting it again"
        );
        drop(guest);
    }
    bail!("the guest printed nothing on its serial console in {ATTEMPTS} attempts")
}

/// Copy the logs of a silent attempt into the artifacts under names that no
/// later attempt reuses; says where they went.
fn keep_silent_attempt(vm_dir: &Path, root: &Path, layout: &Layout, attempt: u32) -> String {
    let Ok(ws) = common::workspace() else {
        return "its logs were not kept (no workspace)".into();
    };
    let Ok(artifacts) = qemu::artifact_dir(&ws, "iso-next") else {
        return "its logs were not kept (no artifact directory)".into();
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let part = root
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    let mut kept = Vec::new();
    for file in ["qemu.log", "serial.log"] {
        let to = artifacts.join(format!(
            "next-{}-{part}-silent-attempt{attempt}-{stamp}-{file}",
            layout.name()
        ));
        if fs::copy(vm_dir.join(file), &to).is_ok() {
            kept.push(to.display().to_string());
        }
    }
    if kept.is_empty() {
        "its logs could not be copied".into()
    } else {
        format!("its logs are kept in {}", kept.join(" and "))
    }
}

/// The ISO, a target and (usually) an answers volume, booted: UEFI, Secure
/// Boot. The target is fresh every time; with a file as the source, the host
/// disk that holds the file is the VM's own disk and the loader's FAT disk is
/// attached last.
fn boot_once(root: &Path, layout: &Layout, setup: Setup<'_>) -> Result<Guest> {
    let ws = common::workspace()?;
    let target = layout.target(root);
    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    if !setup.keep_target {
        let _ = fs::remove_file(&target);
        fs::File::create(&target)
            .and_then(|file| file.set_len(TARGET_BYTES))
            .with_context(|| format!("creating {}", target.display()))?;
    }
    if setup.lvm {
        write_lvm_pv(&target)?;
    }
    if setup.existing {
        // What an earlier installation left: a GPT with ilex's partition, and
        // an EFI system partition with no file system in it (one that cannot be
        // read counts as another system too).
        cmd::require("sfdisk")?;
        let mut sfdisk = Command::new("sfdisk");
        sfdisk
            .arg("--quiet")
            .arg(&target)
            .stdin(std::process::Stdio::piped());
        let mut child = sfdisk.spawn().context("starting sfdisk")?;
        std::io::Write::write_all(
            child.stdin.as_mut().context("sfdisk's input")?,
            b"label: gpt\nsize=64MiB, name=\"ilex-root\"\nsize=64MiB, type=U, name=\"esp-unreadable\"\n",
        )?;
        ensure!(child.wait()?.success(), "sfdisk could not write the table");
    }
    // A fixed test credential the helper's and the answers' units import: it
    // can only make the installer fail. (A boot from a spec does not read the
    // `VmConfig::smbios` it was made from: the provider carries it.)
    let smbios: Vec<String> = setup
        .fault
        .map(|fault| format!("type=11,value=io.systemd.credential:ilex.installer.fault={fault}"))
        .into_iter()
        .collect();
    let mut provider = LinuxProvider::new(ws.clone())?
        .with_disk_serials(setup.serials)
        .with_hotplug_port(setup.hotplug)
        .with_smbios(smbios);
    let mut config = match &layout.fixture {
        None => {
            let mut config = VmConfig::new(layout.purpose(), &target);
            config.cdrom = Some(iso_path()?);
            config.extra_disks = setup.volume.map(Path::to_path_buf).into_iter().collect();
            config
        }
        Some(fixture) => {
            provider = provider.with_esp_dir(&fixture.esp);
            let mut config = VmConfig::new(layout.purpose(), &fixture.disk);
            config.extra_disks = std::iter::once(target)
                .chain(setup.volume.map(Path::to_path_buf))
                .collect();
            config
        }
    };
    config.snapshot = false;
    config.memory = memory;
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    let caps = provider.probe(&spec)?;
    caps.require_provisioning(Capability::FirmwareEnforcement)?;
    let vm = provider.create(spec)?;
    // What the guard removes with the VM is a directory of its own: the disk
    // it installed to is read after it is gone.
    let scratch = root.join("vm");
    fs::create_dir_all(&scratch)?;
    let mut guest = Guest {
        provider,
        vm,
        root: scratch,
    };
    guest.provider.start(&mut guest.vm)?;
    ensure!(
        guest.vm.running_mut()?.info.secure_boot,
        "the VM was not started with Secure Boot"
    );
    Ok(guest)
}

pub(super) fn run() -> Result<()> {
    run_from(Source::Cdrom)
}

pub(super) fn run_loop() -> Result<()> {
    run_from(Source::File)
}

fn fixtures(name: &str) -> Result<PathBuf> {
    let ws = common::workspace()?;
    let root = ws.state_dir().join("iso-next-fixtures").join(name);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    Ok(root)
}

fn run_from(source: Source) -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let iso = iso_path()?;
    let name = match source {
        Source::Cdrom => "unattended",
        Source::File => "unattended-loop",
    };
    let root = fixtures(name)?;
    let layout = Layout::new(source, &root, &iso)?;
    // The host disk is what it is now, and has to be so at the end: the live
    // system boots from a file on it and must never write to it.
    let host_before = layout
        .fixture
        .as_ref()
        .map(|f| sha256_of(&f.disk))
        .transpose()?;

    // What this VM's helper calls the target with no serial number on it: a
    // bus path, which an unattended installation does not accept.
    let bus_id = refuse_invalid(&root, &artifacts, &layout)?;
    refuse_bus_path(&root, &artifacts, &layout, &bus_id)?;
    // Which also says what the disks are called: the same VM, with the same
    // devices in the same order, names them the same way.
    let found = refuse_confirmation(&root, &artifacts, &layout)?;
    println!(
        "iso::next {name}: the target is {} (live medium: {})",
        found.target,
        found.live.as_deref().unwrap_or("none")
    );
    // The CD-ROM source installs over what was on the disk (the answers must
    // say so): first an ilex installation's partition table, refused without
    // `reinstall = true`; then a whole-disk LVM physical volume, which is
    // refused too and is what the installation goes over. The file source
    // installs on a blank disk.
    let existing = layout.source == Source::Cdrom;
    if existing {
        refuse_existing(&root, &artifacts, &layout, &found.target)?;
        refuse_whole_disk(&root, &artifacts, &layout, &found.target)?;
    }
    if let Some(live) = &found.live {
        // The install path's own view of the medium: answers that name it are
        // refused, and nothing, the ISO file included, is written.
        refuse_live_medium(&root, &artifacts, &layout, live)?;
        ensure!(
            host_before.as_deref()
                == Some(sha256_of(&layout.fixture.as_ref().unwrap().disk)?.as_str()),
            "the host disk changed while its installation was refused"
        );
    }

    // The installation.
    let volume = answers_volume(&root, &answers(&found.target, &found.target, existing))?;
    let mut guest = boot(
        &root,
        &layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            lvm: existing,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let started = Instant::now();
    let outcome = install(vm, &serial, &artifacts, name);
    if let Err(error) = &outcome {
        diagnostics(vm, &artifacts, name);
        return Err(anyhow::anyhow!("{error:#}"));
    }
    let console = outcome?;
    let _ = fs::write(artifacts.join(format!("next-{name}-serial.txt")), &console);
    println!(
        "iso::next {name}: installed and powered off in {:?}",
        started.elapsed()
    );
    let plan = fields(&console, MARKER, "plan")?;
    let digest = plan
        .get("image")
        .context("the plan line has no image=")?
        .clone();
    ensure!(digest.starts_with("sha256:"), "the plan's image: {digest}");
    // The target held an LVM physical volume: the answers' `reinstall = true`
    // was what let it be erased, and the console says what went.
    ensure!(
        console.contains("reinstall=true erases:") == existing,
        "the reinstall line is {} on the console: {console}",
        if existing { "missing" } else { "unexpected" }
    );
    ensure!(
        !existing || console.contains("LVM2_member"),
        "the console does not say that an LVM physical volume was erased: {console}"
    );
    // No interface was ever drawn: the session did not get to start before an
    // answers volume was found, and no page was reported.
    ensure!(
        count_markers(&console, "ILEX-INSTALLER", Some("page")) == 0,
        "an interface drew a page during an unattended installation: {console}"
    );
    // The machine is off; the disk it installed boots on its own.
    drop(guest);
    // The helper read the volume read-only: its image is what it was.
    volume.unchanged()?;
    if let Some(fixture) = &layout.fixture {
        // The disk the ISO file lies on was not touched by the installation.
        ensure!(
            host_before.as_deref() == Some(sha256_of(&fixture.disk)?.as_str()),
            "the host disk changed during the installation"
        );
    }
    let mut config = VmConfig::new(format!("{}-t", layout.purpose()), layout.target(&root));
    config.snapshot = true;
    config.memory = "4G".into();
    let mut installed = Vm::boot(&ws, &config)?;
    installed
        .wait_ssh(PAGE_TIMEOUT)
        .context("the installed disk did not come up")?;
    check_installed(
        &|file, text| {
            let _ = qemu::write_artifact(&artifacts, &format!("next-{name}-{file}"), text);
        },
        &installed,
        &Expected {
            digest: &digest,
            target: "ghcr.io/reidond/ilex:generic.gnome",
            host: "ilextest",
            zone: "Europe/Berlin",
            locale: "de_DE.UTF-8",
            keymap: "de-latin1",
            layout: "de",
            admin: ADMIN,
            password: PASSWORD,
        },
    )?;
    gdm_screenshot(
        &mut installed,
        &artifacts.join(format!("next-{name}-gdm.png")),
    )?;
    Ok(())
}

/// The SHA-256 of a file (`sha256sum`, which streams it).
fn sha256_of(path: &Path) -> Result<String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .output()
        .context("running sha256sum")?;
    ensure!(out.status.success(), "sha256sum {} failed", path.display());
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_owned)
        .context("sha256sum printed nothing")
}

/// Boot to the end of the installation: the markers say what happened. On the
/// way, once the pull is under way, the screen is saved: what a person at the
/// machine sees of an unattended installation.
fn install(vm: &mut Vm, serial: &Path, artifacts: &Path, name: &str) -> Result<String> {
    let started = Instant::now();
    let mut shown = false;
    let console = loop {
        let console = fs::read(serial)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        if !shown
            && ["0.4", "0.5", "0.6"]
                .iter()
                .any(|p| console.contains(&format!("stage=image progress={p}")))
        {
            shown = true;
            let _ = Vmctl::from_info(vm.info.clone())
                .save_screenshot(&artifacts.join(format!("next-{name}-progress.png")));
        }
        if count_markers(&console, MARKER, Some("result")) >= 1 {
            break console;
        }
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} before the installation ended; serial: {console}");
        }
        ensure!(
            started.elapsed() < INSTALL_TIMEOUT,
            "the unattended installation did not end in {INSTALL_TIMEOUT:?}; serial: {console}"
        );
        std::thread::sleep(Duration::from_secs(2));
    };
    let result = fields(&console, MARKER, "result")?;
    ensure!(
        result.get("result").map(String::as_str) == Some("success"),
        "the unattended installation did not succeed: {}",
        console
            .lines()
            .filter(|l| l.contains(MARKER))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // It powers the machine off by itself.
    let started = Instant::now();
    loop {
        if vm.exited()?.is_some() {
            return Ok(console);
        }
        if started.elapsed() > Duration::from_secs(300) {
            bail!("the machine did not power off after a successful installation");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// An answers file that is not valid: refused with where and not what, and
/// nothing is written. This VM has no serial numbers on its disks, so its
/// helper names the target by a bus path: returned, for the refusal of such a
/// name.
fn refuse_invalid(root: &Path, artifacts: &Path, layout: &Layout) -> Result<String> {
    let name = layout.name();
    let dir = root.join("invalid");
    fs::create_dir_all(&dir)?;
    // A password in a line that is wrong: the error must not quote it.
    let secret = "S3cret-never-to-be-printed";
    let text = format!("schema = 1\n[administrator]\npassword = \"{secret}\" extra\n");
    let volume = answers_volume(&dir, &text)?;
    let mut guest = boot(
        &dir,
        layout,
        Setup {
            volume: Some(&volume.path),
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-invalid")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-invalid-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed") && console.contains("line 3"),
        "an invalid answers file was not refused with its position: {console}"
    );
    ensure!(!console.contains(secret), "the password was printed");
    assert_untouched(vm, &dir, layout)?;
    // Nothing was written and nothing was started, so the interface does: the
    // screen is not left empty by a stick whose answers cannot be used.
    vm.wait_for(
        "the interface after a refusal that wrote nothing",
        "systemctl is-active ilex-installer-session.service",
        PAGE_TIMEOUT,
    )
    .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-invalid")))?;
    ensure!(
        console.contains("handover=interface"),
        "the refusal did not hand over to the interface: {console}"
    );
    let probe = probe_of(vm)?;
    let target = target_of(&probe)?;
    volume.unchanged()?;
    println!(
        "iso::next {name}: an invalid answers file is refused by position, without its text, and the interface starts (the target is {} without serial numbers)",
        target.id
    );
    Ok(target.id.clone())
}

/// An id that is a bus path, not a `/dev/disk/by-id` name, is refused: it is
/// whatever disk sits in that slot, and a disk swapped in could take it.
fn refuse_bus_path(root: &Path, artifacts: &Path, layout: &Layout, id: &str) -> Result<()> {
    let name = layout.name();
    let dir = root.join("bus-path");
    fs::create_dir_all(&dir)?;
    let volume = answers_volume(&dir, &answers(id, id, false))?;
    let mut guest = boot(
        &dir,
        layout,
        Setup {
            volume: Some(&volume.path),
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-bus-path")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-bus-path-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed")
            && console.contains("by-id")
            && !console.contains("apply=started"),
        "answers that name a disk by a bus path were not refused: {console}"
    );
    assert_untouched(vm, &dir, layout)?;
    volume.unchanged()?;
    println!("iso::next {name}: a disk named by its bus path ({id}) is refused");
    Ok(())
}

/// The disks as the helper names them in this VM.
struct Found {
    /// The blank target.
    target: String,
    /// The disk the ISO file lies on (the file source only).
    live: Option<String>,
}

/// The helper's own probe, over SSH as root.
fn probe_of(vm: &mut Vm) -> Result<Probe> {
    vm.wait_ssh(PAGE_TIMEOUT)
        .context("the live system did not accept the harness's SSH key")?;
    let reply = vm.ssh_ok("busctl --system --json=short call org.ilex.Installer1 /org/ilex/Installer1 org.ilex.Installer1 Probe")?;
    let reply: serde_json::Value = serde_json::from_str(&reply).context("busctl's JSON")?;
    serde_json::from_str(reply["data"][0].as_str().context("busctl's data")?)
        .context("the probe's document")
}

/// The disk of the target's size.
fn target_of(probe: &Probe) -> Result<&ilex_devkit::installer_model::probe::Disk> {
    probe
        .disks
        .iter()
        .find(|d| d.size_bytes == TARGET_BYTES)
        .with_context(|| {
            format!(
                "no disk of {TARGET_BYTES} bytes in the probe: {:?}",
                probe
                    .disks
                    .iter()
                    .map(|d| (&d.id, d.size_bytes))
                    .collect::<Vec<_>>()
            )
        })
}

/// Whole-disk mode erases: `confirm-erase` has to repeat the disk's id. Also
/// says how this VM's helper names the target (with serial numbers: by a
/// `/dev/disk/by-id` name) and, for a boot from a file, the medium.
fn refuse_confirmation(root: &Path, artifacts: &Path, layout: &Layout) -> Result<Found> {
    let name = layout.name();
    let dir = root.join("confirmation");
    fs::create_dir_all(&dir)?;
    let volume = answers_volume(&dir, &answers("a-disk", "another-disk", false))?;
    let mut guest = boot(
        &dir,
        layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-confirmation")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-confirmation-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed") && console.contains("confirm-erase"),
        "a wrong confirm-erase was not refused: {console}"
    );
    assert_untouched(vm, &dir, layout)?;
    volume.unchanged()?;
    println!("iso::next {name}: a wrong confirm-erase is refused and the disk is untouched");
    // The helper's own probe: the disk of the target's size.
    let probe = probe_of(vm)?;
    let target = target_of(&probe)?;
    ensure!(
        !target.live_medium,
        "the blank target is called the live medium: {target:?}"
    );
    ensure!(
        target.id.starts_with("virtio-ilex-disk-"),
        "the target has no by-id name from its serial number: {}",
        target.id
    );
    let live: Vec<_> = probe.disks.iter().filter(|d| d.live_medium).collect();
    let live = match &layout.fixture {
        // An optical drive is no disk: nothing is the medium.
        None => {
            ensure!(live.is_empty(), "a disk is the live medium: {live:?}");
            None
        }
        // The disk that holds the file, by its size, not by elimination.
        Some(fixture) => {
            ensure!(
                live.len() == 1 && live[0].size_bytes == fixture.disk_bytes,
                "the disk holding the ISO file is not the one live_medium disk ({} bytes): {live:?}",
                fixture.disk_bytes
            );
            Some(live[0].id.clone())
        }
    };
    Ok(Found {
        target: target.id.clone(),
        live,
    })
}

/// A disk that already holds an ilex installation is not erased unless the
/// answers say `reinstall = true`: refused, naming what is there, with the
/// partition table as it was.
fn refuse_existing(root: &Path, artifacts: &Path, layout: &Layout, target: &str) -> Result<()> {
    let name = layout.name();
    let dir = root.join("existing");
    fs::create_dir_all(&dir)?;
    let volume = answers_volume(&dir, &answers(target, target, false))?;
    let mut guest = boot(
        &dir,
        layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            existing: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-existing")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-existing-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed")
            && console.contains("reinstall = true")
            && console.contains("ilex")
            && console.contains("could not be read")
            && !console.contains("apply=started")
            && !console.contains("plan=ready"),
        "a disk that holds an ilex installation, and an EFI system partition that cannot be read, was not refused: {console}"
    );
    // The disk is as it was: one partition, labelled ilex-root.
    vm.wait_ssh(PAGE_TIMEOUT)
        .context("the live system did not accept the harness's SSH key")?;
    let out = vm.ssh_ok(&format!(
        "d=$(lsblk -b -d -n -o NAME,SIZE | awk -v s={TARGET_BYTES} '$2==s{{print $1}}'); [ -n \"$d\" ] || {{ echo no-target-disk; exit 3; }}; lsblk -n -o TYPE,PARTLABEL /dev/$d"
    ))?;
    let rows: Vec<&str> = out
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("part"))
        .collect();
    ensure!(
        rows.len() == 2 && rows[0].ends_with("ilex-root") && rows[1].ends_with("esp-unreadable"),
        "the refused installation changed the disk: {out}"
    );
    volume.unchanged()?;
    println!(
        "iso::next {name}: a disk that holds an ilex installation is refused without reinstall = true"
    );
    Ok(())
}

/// CRC-32 as LVM computes it over a label: the reflected polynomial
/// 0xedb88320, an initial value of 0xf597a6cf and no final inversion.
fn lvm_crc(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xf597_a6cf;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// An LVM2 physical volume on the whole of `target`: the label `pvcreate`
/// writes into the disk's second sector (`LABELONE`, its CRC, `LVM2 001`, a
/// 32-character PV UUID, the device size and the data and metadata areas), with
/// no partition table. udev's blkid probe then calls the disk `LVM2_member` and
/// makes `/dev/disk/by-id/lvm-pv-uuid-<uuid>` for it: a name that sorts ahead
/// of `virtio-...` and goes when the signature is wiped. Written by hand so
/// that no root, loop device or LVM tool is needed; blkid, run on the result,
/// says whether it is one.
fn write_lvm_pv(target: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut sector = [0u8; 512];
    sector[0..8].copy_from_slice(b"LABELONE");
    sector[8..16].copy_from_slice(&1u64.to_le_bytes());
    sector[20..24].copy_from_slice(&32u32.to_le_bytes());
    sector[24..32].copy_from_slice(b"LVM2 001");
    sector[32..64].copy_from_slice(b"IlexTrialPhysicalVolumeUuid00001");
    sector[64..72].copy_from_slice(&TARGET_BYTES.to_le_bytes());
    // The data area starts at 1 MiB and runs to the end (size 0); the list is
    // ended by an empty descriptor. One metadata area follows, ended likewise.
    sector[72..80].copy_from_slice(&(1u64 << 20).to_le_bytes());
    sector[104..112].copy_from_slice(&4096u64.to_le_bytes());
    sector[112..120].copy_from_slice(&1_044_480u64.to_le_bytes());
    let crc = lvm_crc(&sector[20..]);
    sector[16..20].copy_from_slice(&crc.to_le_bytes());
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(target)
        .with_context(|| format!("opening {}", target.display()))?;
    file.seek(SeekFrom::Start(512))?;
    file.write_all(&sector)?;
    file.sync_all()?;
    // Not vacuous: what the guest's udev will call it.
    if let Ok(out) = Command::new("blkid")
        .args(["--probe", "--output", "value", "--match-tag", "TYPE"])
        .arg(target)
        .output()
    {
        let kind = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        ensure!(
            kind == "LVM2_member",
            "blkid does not take the hand-made label for an LVM physical volume (it says {kind:?})"
        );
    }
    Ok(())
}

/// A disk that is, whole, an LVM physical volume (a signature with no
/// partition table, so no partition says what is there) is data a person would
/// want to keep: refused without `reinstall = true`, naming what is there, and
/// left as it was. It also has a `/dev/disk/by-id/lvm-pv-uuid-*` link that is
/// no identity: the probe names the disk by its hardware's link.
fn refuse_whole_disk(root: &Path, artifacts: &Path, layout: &Layout, target: &str) -> Result<()> {
    let name = layout.name();
    let dir = root.join("whole-disk");
    fs::create_dir_all(&dir)?;
    let volume = answers_volume(&dir, &answers(target, target, false))?;
    let mut guest = boot(
        &dir,
        layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            lvm: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-whole-disk")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-whole-disk-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed")
            && console.contains("reinstall = true")
            && console.contains("LVM2_member")
            && !console.contains("apply=started")
            && !console.contains("plan=ready"),
        "a whole-disk LVM physical volume was not refused without reinstall = true: {console}"
    );
    // The disk is as it was (the signature, no table), and the LVM link that
    // exists for it is not the name the probe gives it.
    vm.wait_ssh(PAGE_TIMEOUT)
        .context("the live system did not accept the harness's SSH key")?;
    let out = vm.ssh_ok(&format!(
        "d=$(lsblk -b -d -n -o NAME,SIZE | awk -v s={TARGET_BYTES} '$2==s{{print $1}}'); [ -n \"$d\" ] || {{ echo no-target-disk; exit 3; }}; echo type=$(blkid -p -o value -s TYPE /dev/$d); echo pttype=$(lsblk -n -d -o PTTYPE /dev/$d); echo parts=$(lsblk -n -o TYPE /dev/$d | grep -c part); ls /dev/disk/by-id | sed 's/^/link=/'"
    ))?;
    ensure!(
        out.contains("type=LVM2_member")
            && out.lines().any(|l| l == "pttype=")
            && out.contains("parts=0")
            && out.contains("link=lvm-pv-uuid-")
            && out.contains("link=virtio-ilex-disk-"),
        "the refused installation changed the disk, or it has no LVM link to test with: {out}"
    );
    let probe = probe_of(vm)?;
    let disk = target_of(&probe)?;
    ensure!(
        disk.id == target && disk.id.starts_with("virtio-ilex-disk-"),
        "the probe names the disk {} (answers use {target}), not by its hardware's link",
        disk.id
    );
    ensure!(
        disk.other_systems
            .iter()
            .any(|s| s.description.contains("LVM2_member") && s.partition.is_none()),
        "the probe does not list the whole-disk physical volume: {:?}",
        disk.other_systems
    );
    volume.unchanged()?;
    println!(
        "iso::next {name}: a whole-disk LVM physical volume is refused without reinstall = true (named {}, not by its lvm-pv-uuid link)",
        disk.id
    );
    Ok(())
}

/// Answers that name the disk the ISO file lies on are refused as the live
/// medium, before anything is written: the target stays blank, and the
/// caller compares the host disk's checksum.
fn refuse_live_medium(root: &Path, artifacts: &Path, layout: &Layout, live: &str) -> Result<()> {
    let name = layout.name();
    let dir = root.join("medium");
    fs::create_dir_all(&dir)?;
    // `reinstall = true`: the disk holds a file system of its own (the host
    // partition), which answers mode would otherwise refuse first; the refusal
    // wanted here is the helper's, for the medium.
    let volume = answers_volume(&dir, &answers(live, live, true))?;
    let mut guest = boot(
        &dir,
        layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, artifacts, &format!("{name}-medium")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-medium-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed") && console.contains("medium"),
        "answers naming the live medium were not refused as such: {console}"
    );
    ensure!(
        !console.contains("apply=started"),
        "an installation started on the live medium: {console}"
    );
    assert_untouched(vm, &dir, layout)?;
    volume.unchanged()?;
    println!("iso::next {name}: answers that name the disk holding the ISO file are refused");
    Ok(())
}

/// The target has no partition table: nothing was written to it. It is found
/// by its size (with the ISO as a file the VM's first disk is the host).
fn assert_untouched(vm: &mut Vm, dir: &Path, layout: &Layout) -> Result<()> {
    vm.wait_ssh(PAGE_TIMEOUT)
        .context("the live system did not accept the harness's SSH key")?;
    let out = vm.ssh_ok(&format!(
        "d=$(lsblk -b -d -n -o NAME,SIZE | awk -v s={TARGET_BYTES} '$2==s{{print $1}}'); [ -n \"$d\" ] || {{ echo no-target-disk; exit 3; }}; lsblk -n -o NAME,TYPE,PTTYPE /dev/$d"
    ))?;
    ensure!(
        !out.contains("part") && !out.contains("gpt"),
        "the refused installation wrote to the disk: {out}"
    );
    ensure!(
        layout.target(dir).is_file(),
        "the target disk image is gone"
    );
    Ok(())
}

// --- iso::next_unattended_fat -------------------------------------------------

/// The answers come from a FAT stick labelled `ILEXANSWERS`, the label a FAT
/// volume can carry (11 characters; `ILEX-ANSWERS` has 12). Its answers name
/// the right disk with a wrong confirmation: the file is mounted read-only and
/// read through the vfat driver, parsed and refused at its first rule, and
/// nothing is written (the stick's image is what it was, the disk is blank).
pub(super) fn run_fat() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let iso = iso_path()?;
    let root = fixtures("unattended-fat")?;
    let layout = Layout::new(Source::Cdrom, &root, &iso)?;
    let volume = fat_volume(
        &root,
        &answers("a-disk", "another-disk", false),
        "ILEXANSWERS",
    )?;
    let mut guest = boot(
        &root,
        &layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, &artifacts, "unattended-fat"))?;
    let _ = fs::write(artifacts.join("next-unattended-fat-serial.txt"), &console);
    let volume_line = fields(&console, MARKER, "volume")?;
    ensure!(
        volume_line.get("fs").map(String::as_str) == Some("vfat"),
        "the volume was not read as vfat: {console}"
    );
    ensure!(
        console.contains("result=failed") && console.contains("confirm-erase"),
        "the answers on the FAT stick were not read and refused at their first rule: {console}"
    );
    // The label is the FAT-sized one, and nothing on the stick was written.
    vm.wait_ssh(PAGE_TIMEOUT)
        .context("the live system did not accept the harness's SSH key")?;
    let labels = vm.ssh_ok("ls /dev/disk/by-label")?;
    ensure!(
        labels.lines().any(|l| l == "ILEXANSWERS") && !labels.lines().any(|l| l == "ILEX-ANSWERS"),
        "the stick's label: {labels}"
    );
    assert_untouched(vm, &root, &layout)?;
    drop(guest);
    volume.unchanged()?;
    println!(
        "iso::next unattended-fat: a FAT stick labelled ILEXANSWERS is read (vfat, read-only) and refused at its first rule"
    );
    Ok(())
}

// --- iso::next_unattended_late ------------------------------------------------

/// A stick plugged in after the boot changes nothing. The ISO boots with no
/// answers volume, the interface comes up, and a volume labelled
/// `ILEX-ANSWERS` whose answers would install onto the blank target is added
/// to the running guest. The helper says it is ignored, once, and nothing else
/// happens: the interface's session is the same process, no unattended unit
/// ran, the helper is idle and the target is blank.
pub(super) fn run_late() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let iso = iso_path()?;
    let root = fixtures("unattended-late")?;
    let layout = Layout::new(Source::Cdrom, &root, &iso)?;
    let mut guest = boot(
        &root,
        &layout,
        Setup {
            serials: true,
            hotplug: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    wait_for_marker(vm, &serial, "ILEX-INSTALLER", "page", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, &artifacts, "unattended-late"))?;
    let probe = probe_of(vm)?;
    let target = target_of(&probe)?.id.clone();
    let session = |vm: &Vm| -> Result<String> {
        vm.ssh_ok("printf '%s %s %s' \"$(systemctl is-active ilex-installer-session.service)\" \"$(systemctl show -p MainPID --value ilex-installer-session.service)\" \"$(systemctl show -p NRestarts --value ilex-installer-session.service)\"")
    };
    let before = session(vm)?;
    ensure!(
        before.starts_with("active "),
        "the interface is not up before the stick: {before}"
    );
    // The stick: answers that would install onto the target, if they were read.
    let volume = answers_volume(&root, &answers(&target, &target, false))?;
    let qmp = &vm.info.qmp_sock;
    ilex_vmctl::qmp::Qmp::connect(qmp)?.execute(
        "blockdev-add",
        serde_json::json!({
            "driver": "raw",
            "node-name": "latestick",
            "file": {"driver": "file", "filename": std::path::absolute(&volume.path)?.to_string_lossy()},
        }),
    )?;
    ilex_vmctl::qmp::Qmp::connect(qmp)?.execute(
        "device_add",
        serde_json::json!({
            "driver": "virtio-blk-pci",
            "id": "latestick0",
            "drive": "latestick",
            "bus": "ilexhp",
            "serial": "ilex-late",
        }),
    )?;
    let console = wait_for_marker(vm, &serial, MARKER, "ignored", 1, Duration::from_secs(120))
        .inspect_err(|_| diagnostics(vm, &artifacts, "unattended-late"))?;
    // Time for anything that was going to happen to happen.
    std::thread::sleep(Duration::from_secs(20));
    let console_after = fs::read(&serial)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let _ = fs::write(
        artifacts.join("next-unattended-late-serial.txt"),
        &console_after,
    );
    ensure!(
        console.contains("ignored=late"),
        "the late stick was not announced as ignored: {console}"
    );
    // Nothing but that one line: no gate, volume, plan, apply, stage, result or
    // hand-over line of unattended mode.
    let others: Vec<&str> = console_after
        .lines()
        .filter(|line| {
            line.split_whitespace().next() == Some(MARKER) && !line.contains("ignored=late")
        })
        .collect();
    ensure!(
        others.is_empty(),
        "unattended mode did something because of a stick plugged in later: {others:?}"
    );
    // The stick is there (the test is not vacuous), no unattended unit ran, and
    // the helper is idle.
    let facts = vm.ssh_ok(
        "test -e /dev/disk/by-label/ILEX-ANSWERS && echo stick=present; echo answers_unit=$(systemctl is-active ilex-installer-answers.service); echo entered=$(systemctl show -p ActiveEnterTimestampMonotonic --value ilex-installer-answers.service)",
    )?;
    ensure!(
        facts.contains("stick=present"),
        "the stick never appeared in the guest: {facts}"
    );
    ensure!(
        facts.contains("answers_unit=inactive") && facts.contains("entered=0"),
        "the unattended unit ran: {facts}"
    );
    let reply = vm.ssh_ok("busctl --system --json=short call org.ilex.Installer1 /org/ilex/Installer1 org.ilex.Installer1 State")?;
    let reply: serde_json::Value = serde_json::from_str(&reply).context("busctl's JSON")?;
    let state: ilex_devkit::installer_model::state::State =
        serde_json::from_str(reply["data"][0].as_str().context("busctl's data")?)?;
    ensure!(
        !state.active
            && state.outcome.is_none()
            && state.stage == ilex_devkit::installer_model::state::Stage::Idle,
        "the helper is not idle: {state:?}"
    );
    let after = session(vm)?;
    ensure!(
        after == before,
        "the interface's session changed: {before} then {after}"
    );
    assert_untouched(vm, &root, &layout)?;
    drop(guest);
    volume.unchanged()?;
    println!(
        "iso::next unattended-late: a stick plugged in after the boot is ignored (once, with a message) and changes nothing"
    );
    Ok(())
}

// --- iso::next_unattended_repeat ----------------------------------------------

/// What an installed disk is made of, from the host: every byte of the target.
fn target_digest(root: &Path, layout: &Layout) -> Result<String> {
    sha256_of(&layout.target(root))
}

/// The machine that powered off after an unattended installation boots the ISO
/// again with the same sticks in it (the firmware tries USB first). The answers
/// say `reinstall = true`, so nothing but the stamp the first installation left
/// on `/boot` keeps them from erasing the fresh installation, every boot.
///
/// 1. install over a blank disk and power off;
/// 2. boot the ISO again with the same answers volume: refused (the console
///    says the answers already installed the disk), the interface starts, and
///    the whole target disk is byte for byte what the installation left (its
///    SHA-256, taken from the host before and after), and the stamp, read
///    in the guest read-only, is the 64-digit digest;
/// 3. boot with answers that differ in a word (the host name): they are not
///    refused as an installation of the same answers (here `reinstall` is
///    off, so they are refused for the ilex installation the disk holds, and
///    nothing is written either).
pub(super) fn run_repeat() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let iso = iso_path()?;
    let name = "unattended-repeat";
    let root = fixtures(name)?;
    let layout = Layout::new(Source::Cdrom, &root, &iso)?;
    // The target is the VM's main disk: `ilex-disk-0`, so its by-id name is
    // known before any boot (the plan line says so, and is checked below).
    let target = "virtio-ilex-disk-0";

    // 1. The installation.
    let volume = answers_volume(&root, &answers(target, target, true))?;
    let mut guest = boot(
        &root,
        &layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let started = Instant::now();
    let console = install(vm, &serial, &artifacts, name).inspect_err(|_| {
        diagnostics(vm, &artifacts, name);
    })?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-install-serial.txt")),
        &console,
    );
    ensure!(
        fields(&console, MARKER, "plan")?
            .get("disk")
            .map(String::as_str)
            == Some(target),
        "the plan is not for {target}: {console}"
    );
    println!(
        "iso::next {name}: installed and powered off in {:?}",
        started.elapsed()
    );
    drop(guest);
    volume.unchanged()?;
    let installed = target_digest(&root, &layout)?;

    // 2. The same sticks again.
    let mut guest = boot(
        &root,
        &layout,
        Setup {
            volume: Some(&volume.path),
            serials: true,
            keep_target: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, &artifacts, &format!("{name}-again")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-again-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed")
            && console.contains("already installed")
            && !console.contains("plan=ready")
            && !console.contains("apply=started")
            && !console.contains("reinstall=true erases"),
        "the answers that installed this disk were not refused: {console}"
    );
    ensure!(
        !console.contains(PASSWORD),
        "the password was printed: {console}"
    );
    vm.wait_for(
        "the interface after a refusal that wrote nothing",
        "systemctl is-active ilex-installer-session.service",
        PAGE_TIMEOUT,
    )
    .inspect_err(|_| diagnostics(vm, &artifacts, &format!("{name}-again")))?;
    ensure!(
        console.contains("handover=interface"),
        "the refusal did not hand over to the interface: {console}"
    );
    // The stamp, read as the installer reads it (read-only, without replaying a
    // journal): the digest of the request, 64 hexadecimal digits.
    let stamp = vm.ssh_ok(
        "set -e; d=/dev/disk/by-partlabel/ilex-boot; mkdir -p /run/stamp-look; mount -t ext4 -o ro,noload,nodev,nosuid,noexec $d /run/stamp-look; cat /run/stamp-look/ilex-install-request.sha256; stat -c 'mode=%a owner=%U' /run/stamp-look/ilex-install-request.sha256; ls -Z /run/stamp-look/ilex-install-request.sha256; umount /run/stamp-look",
    )?;
    let _ = fs::write(artifacts.join(format!("next-{name}-stamp.txt")), &stamp);
    let digest = stamp.lines().next().unwrap_or_default();
    ensure!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && stamp.contains("mode=600 owner=root"),
        "the stamp on /boot is not a digest: {stamp}"
    );
    drop(guest);
    volume.unchanged()?;
    ensure!(
        target_digest(&root, &layout)? == installed,
        "the refused answers changed the installed disk"
    );
    println!(
        "iso::next {name}: the same answers boot again and are refused ({digest}); the {} MiB disk is byte for byte what the installation left",
        TARGET_BYTES >> 20
    );

    // 3. Answers that differ in one word are another request: they get past the
    // stamp (here to the next rule: the disk holds an installation and
    // `reinstall` is off), and write nothing either.
    let edited = answers_volume(
        &root.join("edited"),
        &answers_named(target, target, false, "ilextest-two"),
    )?;
    let mut guest = boot(
        &root,
        &layout,
        Setup {
            volume: Some(&edited.path),
            serials: true,
            keep_target: true,
            ..Setup::default()
        },
    )?;
    let vm = guest.vm.running_mut()?;
    let serial = vm.info.dir.join("serial.log");
    let console = wait_for_marker(vm, &serial, MARKER, "result", 1, PAGE_TIMEOUT)
        .inspect_err(|_| diagnostics(vm, &artifacts, &format!("{name}-edited")))?;
    let _ = fs::write(
        artifacts.join(format!("next-{name}-edited-serial.txt")),
        &console,
    );
    ensure!(
        console.contains("result=failed")
            && console.contains("already holds a system")
            && console.contains("reinstall = true")
            && !console.contains("already installed")
            && !console.contains("apply=started"),
        "answers that differ in a word were refused for the stamp, or not at the reinstall rule: {console}"
    );
    drop(guest);
    edited.unchanged()?;
    ensure!(
        target_digest(&root, &layout)? == installed,
        "the edited answers changed the installed disk"
    );
    println!(
        "iso::next {name}: answers that differ in a word are not taken for the same installation (and write nothing)"
    );
    Ok(())
}

// --- iso::next_unattended_handover --------------------------------------------

/// Unattended mode fails before it has written anything, in the two ways that
/// the program's own hand-over cannot cover alone, and the interface starts
/// both times (tty1 is never left empty), the answers volume and the target
/// untouched. A fixed test credential (`ilex.installer.fault`) makes each:
///
/// * `helper`: the helper exits as soon as it starts and never takes its bus
///   name. The answers unit does not `Require=` it, so it still starts, fails
///   at its first call and hands over.
/// * `answers`: the unattended process kills itself (SIGKILL) after reading the
///   answers and before it asks the helper for anything: no code of its own
///   runs again, so only systemd can start the interface (`OnFailure=` of the
///   unit, through `ilex-installer-answers-handover.service`).
pub(super) fn run_handover() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let iso = iso_path()?;
    let root = fixtures("unattended-handover")?;
    let layout = Layout::new(Source::Cdrom, &root, &iso)?;
    let target = "virtio-ilex-disk-0";
    for fault in ["helper", "answers"] {
        let dir = root.join(fault);
        fs::create_dir_all(&dir)?;
        let volume = answers_volume(&dir, &answers(target, target, false))?;
        let mut guest = boot(
            &dir,
            &layout,
            Setup {
                volume: Some(&volume.path),
                serials: true,
                fault: Some(fault),
                ..Setup::default()
            },
        )?;
        let vm = guest.vm.running_mut()?;
        let serial = vm.info.dir.join("serial.log");
        let tag = format!("unattended-handover-{fault}");
        // The interface starts: the session unit is active (it is skipped for
        // as long as the gate's file exists, and nothing starts it but the
        // hand-over).
        vm.wait_for(
            "the interface after a failure that wrote nothing",
            "systemctl is-active ilex-installer-session.service",
            PAGE_TIMEOUT,
        )
        .inspect_err(|_| diagnostics(vm, &artifacts, &tag))?;
        // What the console says of it (systemd's own hand-over may print its
        // line a moment after the session is up).
        let wanted = if fault == "answers" {
            "handover=interface by=systemd"
        } else {
            "handover=interface"
        };
        let console = wait_for_text(&serial, wanted, Duration::from_secs(60))
            .inspect_err(|_| diagnostics(vm, &artifacts, &tag))?;
        let _ = fs::write(artifacts.join(format!("next-{tag}-serial.txt")), &console);
        ensure!(
            !console.contains("apply=started") && !console.contains("plan=ready"),
            "something was written or planned although unattended mode was made to fail: {console}"
        );
        ensure!(
            !console.contains(PASSWORD),
            "the password was printed: {console}"
        );
        // What systemd says of the units, and that nothing is left in the way
        // of the interface: the gate's file is gone, Apply's flag never was.
        let facts = vm.ssh_ok(
            "echo marker=$(test -e /run/ilex-answers-volume && echo present || echo gone); echo applying=$(test -e /run/ilex-answers-applying && echo present || echo absent); echo answers=$(systemctl show -p ActiveState,Result --value ilex-installer-answers.service | tr '\n' ' '); echo helper=$(systemctl show -p ActiveState,Result --value ilex-installerd.service | tr '\n' ' '); echo handover=$(systemctl show -p ActiveState,Result --value ilex-installer-answers-handover.service | tr '\n' ' ')",
        )?;
        let _ = fs::write(artifacts.join(format!("next-{tag}-units.txt")), &facts);
        ensure!(
            facts.contains("marker=gone") && facts.contains("applying=absent"),
            "the hand-over left the gate's file, or Apply's flag was raised: {facts}"
        );
        if fault == "answers" {
            ensure!(
                facts.contains("answers=failed signal"),
                "the unattended unit did not end by the signal: {facts}"
            );
            ensure!(
                console.contains("handover=interface by=systemd"),
                "systemd's hand-over did not run: {console}"
            );
            ensure!(
                console.contains("fault=answers") && !console.contains("result=failed"),
                "a killed program printed a verdict, or never said it was made to fail: {console}"
            );
        } else {
            ensure!(
                facts.contains("answers=failed"),
                "the unattended unit did not fail: {facts}"
            );
            ensure!(
                console.contains("handover=interface"),
                "the failure did not hand over: {console}"
            );
        }
        assert_untouched(vm, &dir, &layout)?;
        drop(guest);
        volume.unchanged()?;
        println!(
            "iso::next unattended-handover: with fault={fault} the interface starts, nothing was written and the disk is untouched"
        );
    }
    Ok(())
}

/// The serial console once it holds `text` (a complete line's worth of it).
fn wait_for_text(serial: &Path, text: &str, timeout: Duration) -> Result<String> {
    let started = Instant::now();
    loop {
        let console = fs::read(serial)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        if console.contains(text) {
            return Ok(console);
        }
        ensure!(
            started.elapsed() < timeout,
            "no {text:?} on the serial console in {timeout:?}: {console}"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}
