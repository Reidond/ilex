//! Answers mode: an installation with no interface. `answers.toml` on a volume
//! labelled `ILEX-ANSWERS` (or `ILEXANSWERS`, the name a FAT volume can carry)
//! answers what the pages of the interface ask, and this client drives the
//! helper's own methods with it, as root, exactly as an interface would:
//! `Probe`, `Plan`, `SetSecret` (a sealed memfd), `Apply`, then `State` until
//! the installation ends. Nothing here installs anything: every check the
//! helper makes for an interface, it makes for this.
//!
//! **Boot time only.** Whether there is an answers volume is decided once per
//! boot, after udev has settled (`--answers-gate`, run by
//! `ilex-installer-answers-gate.service`, which the interface's session is
//! ordered after). A volume that appears later is ignored with a message
//! (`--answers-late`): nothing stops the interface, so that a stick someone
//! plugs in cannot replace the installer a person is using.
//!
//! The volume is read once. Its file may hold plaintext secrets, so it is
//! mounted read-only without devices, setuid files or execution inside this
//! unit's private mount namespace, read as a bounded regular file through a
//! descriptor that is checked, and unmounted; its text lives in memory that is
//! wiped, and no line this program prints quotes it.

use crate::{
    disks::{self, node_number},
    exec, fault,
    scratch::Scratch,
    secret, target,
    topology::{DevNum, split_dev},
};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::{
    answers::Answers,
    plan::{Plan, PlanError},
    probe::{Disk, Probe},
    state::{Outcome, State},
};
use serde_json::Value;
use std::{
    fs,
    io::Read,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use zbus::{Connection, Proxy};
use zeroize::Zeroizing;

const NAME: &str = "org.ilex.Installer1";
const PATH: &str = "/org/ilex/Installer1";
/// The labels a volume of answers may carry. FAT and exFAT labels are 11
/// characters at most, and `ILEX-ANSWERS` has 12: the second name is the one a
/// FAT volume can have.
pub const VOLUME_LABELS: [&str; 2] = ["ILEX-ANSWERS", "ILEXANSWERS"];
/// The file in it.
const FILE_NAME: &str = "answers.toml";
/// An answers file is a few hundred bytes.
const MAX_BYTES: u64 = 64 * 1024;
/// File systems an answers volume may carry, by the type blkid reports.
/// FAT is what a stick written on any computer has; the rest are what a person
/// who makes one on Linux uses.
pub const FILE_SYSTEMS: [&str; 7] = ["ext4", "ext3", "ext2", "xfs", "vfat", "iso9660", "udf"];
/// Written at boot by the gate when an answers volume is there: the device
/// numbers (`major:minor`, one per line) of the volumes it found. The
/// interface's session does not start while it exists, and a volume that
/// appears later is told from these.
pub const BOOT_MARKER: &str = "/run/ilex-answers-volume";
/// Started when answers mode fails before it has changed any disk.
const SESSION: &str = "ilex-installer-session.service";
/// Present from just before `Apply` is called until `Apply` has refused (or for
/// the rest of the boot once it did not): it says the helper may have begun to
/// write. The systemd-level hand-over (`ilex-installer-answers-handover.service`,
/// the answers unit's `OnFailure=`) does nothing while it exists, and starts
/// the interface when the unit ended without it.
pub const APPLYING_FLAG: &str = "/run/ilex-answers-applying";

/// What to do once the installation has succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Leave the machine as it is.
    Nothing,
    /// Power off: an unattended installation must not boot the medium it just
    /// installed from again.
    PowerOff,
}

pub struct Options {
    pub run_dir: PathBuf,
    /// Read the answers from this file instead of the volume (development
    /// and the `installer::` tier's fixtures).
    pub file: Option<PathBuf>,
    pub then: Then,
}

/// A line a person or a test harness can find on the console. It never holds
/// anything from the answers file. It goes to the journal and the console
/// (standard output) and, once, to the serial port if there is one, without
/// ever waiting for it: a port nobody reads, or one that has stopped, costs
/// this line and nothing else.
pub fn say(text: &str) {
    use std::io::Write;
    let line = format!("ILEX-INSTALLER-ANSWERS {text}");
    println!("{line}");
    if let Ok(mut port) = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open("/dev/ttyS0")
    {
        let _ = port.write(format!("{line}\r\n").as_bytes());
    }
}

/// A volume that carries an answers label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Volume {
    pub path: String,
    pub fs: String,
    pub number: DevNum,
}

/// Whether `label` is one of the answers labels. FAT and exFAT keep a label in
/// capitals whatever was typed, so for them case does not matter.
fn is_answers_label(label: &str, fs: &str) -> bool {
    VOLUME_LABELS.iter().any(|name| {
        if matches!(fs, "vfat" | "exfat") {
            label.eq_ignore_ascii_case(name)
        } else {
            label == *name
        }
    })
}

/// The volumes `lsblk --json` lists that carry an answers label: disks,
/// partitions and optical media with a file system. Every one is returned, so
/// that two sticks with the label are noticed instead of one being chosen.
pub fn parse_volumes(json: &str) -> Result<Vec<Volume>> {
    let root: Value = serde_json::from_str(json).context("reading lsblk")?;
    let devices = root["blockdevices"]
        .as_array()
        .context("lsblk has no blockdevices")?;
    let mut volumes = Vec::new();
    fn walk(devices: &[Value], volumes: &mut Vec<Volume>) -> Result<()> {
        for device in devices {
            let text = |key: &str| device[key].as_str().unwrap_or_default();
            if matches!(text("type"), "disk" | "part" | "rom")
                && !text("fstype").is_empty()
                && is_answers_label(text("label"), text("fstype"))
            {
                let (major, minor) = text("maj:min")
                    .trim()
                    .split_once(':')
                    .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                    .context("lsblk gives no device number for a volume")?;
                volumes.push(Volume {
                    path: text("path").to_owned(),
                    fs: text("fstype").to_owned(),
                    number: (major, minor),
                });
            }
            if let Some(children) = device["children"].as_array() {
                walk(children, volumes)?;
            }
        }
        Ok(())
    }
    walk(devices, &mut volumes)?;
    Ok(volumes)
}

/// The volumes of this system that carry an answers label.
pub fn volumes() -> Result<Vec<Volume>> {
    let out = exec::run(
        "lsblk",
        &[
            "--json",
            "--paths",
            "--output",
            "PATH,LABEL,FSTYPE,TYPE,MAJ:MIN",
        ],
        Duration::from_secs(30),
    )?;
    ensure!(out.success(), "lsblk failed: {}", out.stderr.trim());
    parse_volumes(&out.stdout_text())
}

/// The one volume answers are read from: none is no answers mode; two are
/// refused, because which one a person meant cannot be known.
pub fn find_volume(all: &[Volume]) -> Result<&Volume> {
    match all {
        [] => bail!(
            "no volume is labelled {} (or {})",
            VOLUME_LABELS[0],
            VOLUME_LABELS[1]
        ),
        [one] => Ok(one),
        many => bail!(
            "more than one volume carries an answers label ({}): remove all but one",
            many.iter()
                .map(|v| v.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// At boot, once udev has settled: is there an answers volume? When there is,
/// leave the marker the interface's session waits on and says so; when there
/// is not, there is nothing to write and nothing to say. Always succeeds:
/// whatever goes wrong here, the interface still starts.
pub fn gate() -> Result<()> {
    let _ = exec::run(
        "udevadm",
        &["settle", "--timeout=30"],
        Duration::from_secs(40),
    );
    let found = volumes().unwrap_or_default();
    // A stale marker (a restart of the unit within one boot) is replaced.
    let _ = fs::remove_file(BOOT_MARKER);
    if found.is_empty() {
        return Ok(());
    }
    let numbers: String = found
        .iter()
        .map(|v| format!("{}:{}\n", v.number.0, v.number.1))
        .collect();
    // Written whole or not at all (a marker that is there but empty or cut
    // short would keep the interface from starting and leave nothing to read).
    let partial = format!("{BOOT_MARKER}.partial");
    let written = fs::write(&partial, numbers).and_then(|()| fs::rename(&partial, BOOT_MARKER));
    if let Err(error) = written {
        let _ = fs::remove_file(&partial);
        return Err(error).context("writing the boot marker");
    }
    say(&format!(
        "gate=present volumes={} (unattended: the interface does not start)",
        found.len()
    ));
    Ok(())
}

/// A volume with an answers label appeared (`device` is its node): if it was
/// not there at boot, say that it is ignored. Nothing is stopped or started.
pub fn late(device: &Path) -> Result<()> {
    let number = fs::metadata(device)
        .ok()
        .filter(|m| m.file_type().is_block_device())
        .map(|m| split_dev(m.rdev()));
    let at_boot = fs::read_to_string(BOOT_MARKER).unwrap_or_default();
    let known = number.is_some_and(|(major, minor)| {
        at_boot
            .lines()
            .any(|line| line.trim() == format!("{major}:{minor}"))
    });
    if known {
        return Ok(());
    }
    say(&format!(
        "ignored=late volume={} (an answers volume that appears after the boot is not read: answers are read at boot only, so nothing was started and the installer is as it was)",
        exec::clean_line(device.to_string_lossy().as_bytes())
    ));
    Ok(())
}

/// The systemd-level hand-over has run (`--answers-handed-over`, from
/// `ilex-installer-answers-handover.service`): say so on the console and the
/// serial port, once. The marker was removed and the session started by the
/// unit's own commands, which need nothing of this program.
pub fn handed_over() {
    say(
        "handover=interface by=systemd (the unattended installation ended before it began to write; the installer's interface starts)",
    );
}

/// What the boot recorded in [`BOOT_MARKER`]: the device numbers of the answers
/// volumes the gate found, one `major:minor` per line. A line that is not one
/// is an error, never skipped.
pub fn parse_marker(text: &str) -> Result<Vec<DevNum>> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim()
                .split_once(':')
                .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
                .with_context(|| "the boot's record of the answers volume is not a device number")
        })
        .collect()
}

/// The volume to read: the one the boot recorded, and no other. Exactly one
/// device was recorded; what is listed now must be exactly that device (the
/// time between the gate and this read is long enough to unplug a stick, swap
/// it for another or plug a second one in).
pub fn check_boot_volume(recorded: &[DevNum], found: &[Volume]) -> Result<Volume> {
    ensure!(
        recorded.len() == 1,
        "the boot recorded {} answers volumes; exactly one is read (remove the others and restart)",
        recorded.len()
    );
    let volume = find_volume(found)?;
    ensure!(
        volume.number == recorded[0],
        "the answers volume found now ({} is {}:{}) is not the one the boot recorded ({}:{}): a stick was swapped or added after the boot, so nothing is read",
        exec::clean_line(volume.path.as_bytes()),
        volume.number.0,
        volume.number.1,
        recorded[0].0,
        recorded[0].1
    );
    Ok(volume.clone())
}

/// Read `answers.toml` from `directory`.
pub fn read_file(directory: &Path) -> Result<Zeroizing<Vec<u8>>> {
    read_regular(&directory.join(FILE_NAME))
}

/// A regular file of at most [`MAX_BYTES`], opened without following a link
/// (`O_NOFOLLOW`) and without blocking on a FIFO or a device (`O_NONBLOCK`),
/// and checked on the descriptor it opened: what is read is what was
/// checked, and what was checked is a regular file.
fn read_regular(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    read_bounded(path, FILE_NAME, MAX_BYTES)
}

/// [`read_regular`] for any file of at most `max` bytes; `what` names it in the
/// messages.
fn read_bounded(path: &Path, what: &str, max: u64) -> Result<Zeroizing<Vec<u8>>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("{what} cannot be opened (missing, or a link)"))?;
    let meta = file.metadata().context("looking at the file")?;
    ensure!(meta.file_type().is_file(), "{what} is not a regular file");
    ensure!(meta.len() <= max, "{what} is larger than {max} bytes");
    let mut bytes = Zeroizing::new(Vec::with_capacity(meta.len() as usize));
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "{what} grew while it was read");
    Ok(bytes)
}

/// The device number a mount point's mount is made of, from a mountinfo table.
pub fn mounted_number(mountinfo: &str, mountpoint: &str) -> Option<DevNum> {
    mountinfo.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let device = fields.nth(2)?;
        let point = fields.nth(1)?;
        (point == mountpoint).then_some(())?;
        let (major, minor) = device.split_once(':')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    })
}

/// What the volume was and what it said.
struct Answered {
    text: Zeroizing<Vec<u8>>,
    volume: Volume,
}

/// Mount the volume read-only in a private directory, read the file, unmount.
fn read_volume(run_dir: &Path) -> Result<Answered> {
    // Which volume the gate found at boot, and no other: the listing below is
    // taken a long time after it (this unit waits for the network), and what
    // was plugged in, swapped or removed in between is not to be read.
    let recorded =
        parse_marker(&fs::read_to_string(BOOT_MARKER).context("reading what the boot recorded")?)?;
    let all = volumes()?;
    let volume = check_boot_volume(&recorded, &all)?;
    ensure!(
        FILE_SYSTEMS.contains(&volume.fs.as_str()),
        "the volume {} holds a {} file system; an answers volume holds one of {}",
        volume.path,
        exec::clean_line(volume.fs.as_bytes()),
        FILE_SYSTEMS.join(", ")
    );
    // The device is whatever the numbers say, not whatever a name says later.
    ensure!(
        node_number(&volume.path) == Ok(volume.number) && volume.number == recorded[0],
        "{} is not the device the boot recorded (it was replaced)",
        volume.path
    );
    say(&format!(
        "volume={} fs={}",
        volume.path,
        exec::clean_line(volume.fs.as_bytes())
    ));
    let mut scratch = Scratch::new(run_dir, "answers")?;
    let mountpoint = scratch.subdir("volume")?;
    let mount_point = mountpoint.to_string_lossy().into_owned();
    let mounted = exec::run(
        "mount",
        &[
            "-t",
            &volume.fs,
            "-o",
            "ro,nodev,nosuid,noexec",
            &volume.path,
            &mount_point,
        ],
        Duration::from_secs(60),
    )?;
    ensure!(
        mounted.success(),
        "the volume {} could not be mounted read-only as {} ({})",
        volume.path,
        exec::clean_line(volume.fs.as_bytes()),
        exec::clean_line(
            mounted
                .stderr
                .lines()
                .last()
                .unwrap_or("no message")
                .as_bytes()
        )
    );
    // What is mounted is the device that was chosen (a disk swapped between
    // the listing and the mount would answer to the same name).
    // Checked twice: against the mount table and against the mounted file
    // system's own device number.
    let table = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let same = mounted_number(&table, &mount_point) == Some(volume.number)
        && fs::metadata(&mountpoint).is_ok_and(|m| split_dev(m.dev()) == recorded[0]);
    let read = if same {
        read_file(&mountpoint)
    } else {
        Err(anyhow::anyhow!(
            "what was mounted is not the volume that was chosen ({})",
            volume.path
        ))
    };
    let unmounted = exec::run("umount", &[&mount_point], Duration::from_secs(60));
    let text = read?;
    ensure!(
        unmounted.is_ok_and(|o| o.success()),
        "the answers volume could not be unmounted"
    );
    Ok(Answered { text, volume })
}

/// Whether the device `volume` is the disk the answers would erase or one of
/// its partitions: by device number, never by name. `parent` is the whole
/// disk of `volume` when it is a partition.
pub fn volume_on_disk(
    volume: DevNum,
    parent: Option<DevNum>,
    disk: DevNum,
    partitions: &[DevNum],
) -> bool {
    volume == disk || parent == Some(disk) || partitions.contains(&volume)
}

/// The whole disk of a partition, from sysfs.
fn parent_disk(sys: &Path, number: DevNum) -> Result<Option<DevNum>> {
    let node = fs::canonicalize(
        sys.join("dev/block")
            .join(format!("{}:{}", number.0, number.1)),
    )
    .context("sysfs does not know the volume's device")?;
    if !node.join("partition").exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(
        node.parent()
            .context("a partition with no disk above it")?
            .join("dev"),
    )
    .context("reading the volume's disk")?;
    let (major, minor) = text.trim().split_once(':').context("the disk's number")?;
    Ok(Some((major.parse()?, minor.parse()?)))
}

/// The volume must not be on the disk the installation would erase. The
/// device that was read (its number, resolved when it was read) is compared
/// with the disk and its partitions by their device numbers; a node that is
/// gone, or a number that cannot be found, is a refusal.
fn ensure_not_on(volume: &Volume, disk: &Disk) -> Result<()> {
    let disk_number = node_number(&disk.path).map_err(anyhow::Error::msg)?;
    let partitions = disk
        .partitions
        .iter()
        .map(|p| node_number(&p.path))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::msg)?;
    let parent = parent_disk(Path::new("/sys"), volume.number)?;
    ensure!(
        !volume_on_disk(volume.number, parent, disk_number, &partitions),
        "the answers volume {} is on {}, the disk the answers would erase",
        volume.path,
        disk.id
    );
    Ok(())
}

/// What the disk's `/boot` says it was installed from: the digest in
/// [`target::REQUEST_STAMP`], when the disk has a partition labelled
/// `ilex-boot` with an ext4 file system that has one. `None` is no such
/// partition, or no stamp on it (an installation that did not finish, or one
/// made by another program). A file system that cannot be mounted, or a stamp
/// that is not a digest, is an error: it may be exactly the installation this
/// check exists to protect, and what cannot be read is not "no stamp". The
/// partition is mounted read-only, without journal replay (`noload`: not one
/// byte is written), in this unit's private mount namespace, and unmounted
/// before anything else is asked of the disk.
fn read_stamp(run_dir: &Path, disk: &Disk) -> Result<Option<String>> {
    let Some(boot) = disk
        .partitions
        .iter()
        .find(|p| p.label == target::BOOT_LABEL && p.filesystem == "ext4")
    else {
        return Ok(None);
    };
    let number = node_number(&boot.path).map_err(anyhow::Error::msg)?;
    let mut scratch = Scratch::new(run_dir, "stamp")?;
    let mountpoint = scratch.subdir("boot")?;
    let mount_point = mountpoint.to_string_lossy().into_owned();
    let mounted = exec::run(
        "mount",
        &[
            "-t",
            "ext4",
            "-o",
            "ro,noload,nodev,nosuid,noexec",
            &boot.path,
            &mount_point,
        ],
        Duration::from_secs(60),
    )?;
    ensure!(
        mounted.success(),
        "{} ({}) could not be mounted read-only to look for the stamp of an earlier installation ({}); erasing what might be an installation is not done blind",
        boot.path,
        target::BOOT_LABEL,
        exec::clean_line(
            mounted
                .stderr
                .lines()
                .last()
                .unwrap_or("no message")
                .as_bytes()
        )
    );
    let table = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let same = mounted_number(&table, &mount_point) == Some(number);
    let read = if same {
        match fs::symlink_metadata(mountpoint.join(target::REQUEST_STAMP)) {
            Ok(_) => {
                read_bounded(&mountpoint.join(target::REQUEST_STAMP), "the stamp", 4096).map(Some)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!("looking for the stamp: {e}")),
        }
    } else {
        Err(anyhow::anyhow!(
            "what was mounted is not {} (the partition was replaced)",
            boot.path
        ))
    };
    let unmounted = exec::run("umount", &[&mount_point], Duration::from_secs(60));
    let read = read?;
    ensure!(
        unmounted.is_ok_and(|o| o.success()),
        "the partition {} could not be unmounted after the stamp was read",
        boot.path
    );
    let Some(bytes) = read else {
        return Ok(None);
    };
    let text = std::str::from_utf8(&bytes)
        .ok()
        .map(str::trim)
        .filter(|t| t.len() == 64 && t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .with_context(|| format!("the stamp on {} is not a digest", boot.path))?;
    Ok(Some(text.to_owned()))
}

/// Whether the stamp of a disk is the digest of the request now being made:
/// these answers already installed this disk.
pub fn stamp_is_these_answers(stamp: Option<&str>, digest: &str) -> bool {
    stamp == Some(digest)
}

/// What the disk already holds that a person would want to keep: ilex's own
/// partitions, another operating system, a boot loader.
pub fn existing_systems(disk: &Disk) -> Vec<String> {
    let mut found: Vec<String> = disk
        .partitions
        .iter()
        .filter(|p| p.label.starts_with("ilex-"))
        .map(|p| {
            format!(
                "an ilex partition ({})",
                exec::clean_line(p.label.as_bytes())
            )
        })
        .collect();
    found.extend(
        disk.other_systems
            .iter()
            .map(|s| exec::clean_line(s.description.as_bytes())),
    );
    found
}

/// The tag the plan is for. Named by the answers, it is theirs. Left out, it is
/// the installer's own selection (the boot's choice, else detection); when that
/// selection failed, the boot asked for something the installer does not offer,
/// and an unattended installation does not guess what was meant: the answers must
/// name a composition.
pub fn choose_composition(
    named: Option<&str>,
    selection: Option<&str>,
    selection_error: Option<&str>,
) -> Result<String> {
    match (named, selection, selection_error) {
        (Some(_), _, _) => Ok(String::new()),
        (None, Some(tag), _) => Ok(tag.to_owned()),
        (None, None, Some(error)) => bail!(
            "the answers name no composition and the boot's choice is not valid ({}): name one in [install] composition",
            exec::clean_line(error.as_bytes())
        ),
        (None, None, None) => {
            bail!("the answers name no composition and the installer has made no selection")
        }
    }
}

/// What a refused call says: the helper's JSON refusal when the error carries
/// one, else the error itself.
fn describe(error: &zbus::Error) -> String {
    if let zbus::Error::MethodError(_, Some(message), _) = error
        && let Ok(refusal) = serde_json::from_str::<PlanError>(message)
    {
        return format!(
            "{:?}{}: {}",
            refusal.code,
            refusal
                .field
                .map_or_else(String::new, |f| format!(" ({f})")),
            refusal.message
        );
    }
    error.to_string()
}

async fn call<R>(
    proxy: &Proxy<'_>,
    method: &'static str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType + Sync),
) -> Result<R>
where
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    proxy
        .call(method, body)
        .await
        .map_err(|e| anyhow::anyhow!("{method}: {}", describe(&e)))
}

fn text_of(state: &State) -> String {
    match &state.outcome {
        Some(Outcome::Failed { stage, reason }) => format!("failed in {}: {reason}", stage.name()),
        Some(Outcome::Success) => "succeeded".into(),
        None => state.message.clone(),
    }
}

/// How far an unattended installation got.
#[derive(Default)]
struct Reached {
    /// `Apply` was accepted: a disk may have been written.
    applied: bool,
}

/// Run an unattended installation. Ok only when it succeeded; whatever else
/// happened, the last `ILEX-INSTALLER-ANSWERS` line says `result=` and why
/// (a refusal of the helper, a file that is not valid by its position, never
/// by its text). A failure before `Apply` leaves every disk as it was and
/// hands the machine to the interface, so that the screen is not left empty.
pub fn run(options: &Options) -> Result<()> {
    let mut reached = Reached::default();
    // A flag left by an earlier attempt of this boot says nothing about this one.
    let _ = fs::remove_file(APPLYING_FLAG);
    // The answers file may hold a password: this process never dumps core and
    // cannot be traced, as the helper that receives the password. It is done
    // here, before the volume is read, so that its failure is a failure like
    // any other before `Apply`: the machine is handed to the interface.
    let outcome = ilex_enroll::harden()
        .context("hardening this process")
        .and_then(|()| zbus::block_on(drive(options, &mut reached)));
    if let Err(error) = &outcome {
        say(&format!(
            "result=failed {}",
            exec::clean_line(format!("{error:#}").as_bytes())
        ));
        if !reached.applied && options.file.is_none() {
            hand_over();
        }
    }
    outcome
}

/// Nothing was written: let the interface start after all.
fn hand_over() {
    let _ = fs::remove_file(BOOT_MARKER);
    let started = exec::run(
        "systemctl",
        &["--no-block", "start", SESSION],
        Duration::from_secs(30),
    );
    if started.is_ok_and(|o| o.success()) {
        say("handover=interface (nothing was written; the installer's interface starts)");
    }
}

async fn drive(options: &Options, reached: &mut Reached) -> Result<()> {
    // The answers.
    let (text, volume) = match &options.file {
        Some(file) => (read_regular(file)?, None),
        None => {
            let read = read_volume(&options.run_dir)?;
            (read.text, Some(read.volume))
        }
    };
    let answers = Answers::parse(std::str::from_utf8(&text).context("answers.toml is not UTF-8")?)?;
    drop(text);
    // The `installer::` and `iso::next_unattended_handover` tiers' fault (the
    // fixed credential ilex.installer.fault=answers; it can only make this
    // program fail): this process ends as a crash would, before it has asked
    // the helper for anything, so that only systemd can start the interface.
    if fault::requested("answers") {
        say(
            "fault=answers (a test credential): this process is killed before it asks the helper for anything",
        );
        // SAFETY: ends this process.
        unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
    }

    let connection = Connection::system()
        .await
        .context("connecting to the system bus")?;
    let proxy = Proxy::new(&connection, NAME, PATH, NAME).await?;
    let probe: String = call(&proxy, "Probe", &()).await?;
    let probe: Probe = serde_json::from_str(&probe).context("reading the probe")?;
    // The composition: the answers' own, else the installer's selection. A
    // selection that failed (an `ilex.composition=` that names nothing the
    // installer offers) is not replaced by what detection would have chosen.
    let selected = choose_composition(
        answers.install.composition.as_deref(),
        probe.selection.as_ref().map(|s| s.tag.as_str()),
        probe.selection_error.as_deref(),
    )?;
    let request = answers
        .to_request(&selected)
        .map_err(|e| anyhow::anyhow!("answers.toml: {}", e.message))?;
    let disk = probe
        .disks
        .iter()
        .find(|d| d.id == request.disk)
        .with_context(|| {
            format!(
                "no disk of this computer has the id {}",
                exec::clean_line(request.disk.as_bytes())
            )
        })?;
    // The disk is named by what stays its own: a /dev/disk/by-id name. A bus
    // path or a kernel name is whatever is in that slot at the moment.
    let (_, named) = disks::by_id_node(&disk.id).map_err(|why| {
        anyhow::anyhow!(
            "the disk {} has no /dev/disk/by-id name; an unattended installation names its disk by one ({why})",
            exec::clean_line(disk.id.as_bytes())
        )
    })?;
    ensure!(
        node_number(&disk.path) == Ok(named),
        "/dev/disk/by-id/{} leads to another device than {}",
        disk.id,
        disk.path
    );
    // The volume must not be on the disk the installation would erase.
    if let Some(volume) = &volume {
        ensure_not_on(volume, disk)?;
    }
    // The answers that installed this disk once do not install it again: the
    // machine powered off after its installation, and with the stick still in
    // it the firmware booted the installer once more.
    let digest = target::request_digest(&request);
    if stamp_is_these_answers(read_stamp(&options.run_dir, disk)?.as_deref(), &digest) {
        bail!(
            "these answers already installed {} (the installation recorded their digest in /boot/{}): remove the stick, or edit the answers (anything but the password makes them new answers) to install again",
            disk.id,
            target::REQUEST_STAMP
        );
    }
    // A disk that already holds a system is erased only when the answers say so.
    let found = existing_systems(disk);
    if !found.is_empty() && !answers.install.reinstall {
        // What a person must do comes first: the console line is cut at 300
        // characters, and what a disk holds can be a long list.
        bail!(
            "{} already holds a system: an unattended installation erases such a disk only when answers.toml says reinstall = true under [install]; it holds {}",
            disk.id,
            found.join(", ")
        );
    }
    if !found.is_empty() {
        say(&format!("reinstall=true erases: {}", found.join("; ")));
    }
    let plan: String = call(&proxy, "Plan", &(serde_json::to_string(&request)?,)).await?;
    let plan: Plan = serde_json::from_str(&plan).context("reading the plan")?;
    say(&format!(
        "plan=ready disk={} composition={} image={} partitions={}",
        plan.disk.id,
        plan.composition.tag,
        plan.image.digest,
        plan.partitions.len()
    ));
    for warning in &plan.warnings {
        say(&format!("warning={}", exec::clean_line(warning.as_bytes())));
    }

    // The password crosses as a sealed memfd, never as a string.
    let memfd = secret::sealed_memfd(answers.administrator.password.expose().as_bytes())?;
    let _: () = call(
        &proxy,
        "SetSecret",
        &(plan.id.as_str(), "admin-password", zbus::Fd::from(&memfd)),
    )
    .await?;
    drop(memfd);
    let confirm = answers.install.confirm_erase.clone().unwrap_or_default();
    drop(answers);
    // From here the helper may begin to write: the flag tells the systemd-level
    // hand-over to stay out of it. A refusal (nothing was written) lowers it.
    fs::write(APPLYING_FLAG, b"").context("writing the flag that says Apply was called")?;
    let state: String = match call(&proxy, "Apply", &(plan.id.as_str(), confirm.as_str())).await {
        Ok(state) => state,
        Err(error) => {
            let _ = fs::remove_file(APPLYING_FLAG);
            return Err(error);
        }
    };
    // Apply was accepted: whatever happens next (even a state this program
    // cannot read), the disk may be changing and the interface stays out.
    reached.applied = true;
    let _: State = serde_json::from_str(&state).context("reading the state")?;
    say("apply=started");

    // Follow it. State is the contract: it is what an interface that restarts
    // re-attaches with, so it is also what this follows.
    let mut last = (String::new(), -1.0f64);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let state: String = call(&proxy, "State", &()).await?;
        let state: State = serde_json::from_str(&state).context("reading the state")?;
        let stage = state.stage.name().to_owned();
        if stage != last.0 || (state.progress - last.1).abs() >= 0.1 {
            say(&format!(
                "stage={stage} progress={:.2} message={}",
                state.progress, state.message
            ));
            last = (stage, state.progress);
        }
        match &state.outcome {
            Some(Outcome::Success) if !state.active => {
                say("result=success");
                if options.then == Then::PowerOff {
                    say("power=off");
                    let _ = exec::run("systemctl", &["poweroff"], Duration::from_secs(30));
                }
                return Ok(());
            }
            Some(Outcome::Failed { .. }) if !state.active => {
                bail!("the installation {}", text_of(&state));
            }
            _ => {}
        }
    }
}
