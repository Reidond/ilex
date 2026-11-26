//! The disks of this computer: what is on them, where the free space is, which
//! one the live system came from and which the running system is using. Reading
//! only: the one thing this module mounts is an EFI system partition,
//! read-only, in a mount namespace of its own that ends with the scan.

use crate::{
    exec,
    scratch::Scratch,
    topology::{self, DevNum, Roots, System, Tracer},
};
use anyhow::{Context, Result};
use ilex_installer_model::probe::{Disk, Extent, OtherSystem, Partition};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    path::Path,
    thread,
    time::Duration,
};

const MIB: u64 = 1024 * 1024;
const TOOL_TIMEOUT: Duration = Duration::from_secs(60);
/// GPT type GUIDs.
const ESP_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
const MICROSOFT_RESERVED: &str = "e3c9e316-0b5c-4db8-817d-f92df00215ae";

const LSBLK_COLUMNS: &str = "NAME,KNAME,TYPE,SIZE,MODEL,SERIAL,TRAN,RM,RO,LOG-SEC,PTTYPE,PARTTYPE,PARTLABEL,FSTYPE,LABEL,PKNAME";

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().trim().to_owned()
}

fn number(value: &Value, key: &str) -> u64 {
    match &value[key] {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn flag(value: &Value, key: &str) -> bool {
    match &value[key] {
        Value::Bool(b) => *b,
        Value::String(s) => s == "1" || s == "true",
        Value::Number(n) => n.as_u64() == Some(1),
        _ => false,
    }
}

/// The kernel name of a path under `/dev` (`/dev/vda1` becomes `vda1`).
fn kernel_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// Devices that are not somewhere to install by their kernel name: compressed
/// RAM, network block devices, floppy and optical drives, loop devices.
fn is_installable(kname: &str) -> bool {
    !["zram", "ram", "nbd", "fd", "loop", "sr"]
        .iter()
        .any(|prefix| {
            kname
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()))
        })
}

/// Transports of disks that live on a network or a SAN: iSCSI, Fibre Channel
/// (also over Ethernet) and SCSI over RDMA, and the NVMe fabrics, as lsblk
/// names them. lsblk calls every NVMe disk `nvme`, so those are told from sysfs.
const NETWORK_TRANSPORTS: &[&str] = &["iscsi", "fc", "fcoe", "srp", "tcp", "rdma"];
/// The one NVMe transport that is a local disk.
const NVME_LOCAL: &str = "pcie";

/// Reached over a network, whatever its kernel name says. An NVMe disk is local
/// only when sysfs says every controller behind it is PCIe: a fabric, a
/// transport that cannot be read and a namespace with no controller found are
/// all treated as the network.
fn is_network(kname: &str, tran: &str, nvme_transport: Option<&str>) -> bool {
    NETWORK_TRANSPORTS.contains(&tran)
        || (kname.starts_with("nvme") && nvme_transport != Some(NVME_LOCAL))
}

/// Whether a directory entry is an NVMe controller (`nvme0`, `nvme12`): not a
/// namespace (`nvme0n1`), a path (`nvme0c0n1`) or the subsystem (`nvme-subsys0`).
fn is_nvme_controller(entry: &str) -> bool {
    entry
        .strip_prefix("nvme")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// The transport of the controllers behind an NVMe namespace, from sysfs:
/// `pcie` only when at least one controller was found and every one of them
/// reads as PCIe; the first other transport when a controller reads as one;
/// `None` when any controller's transport cannot be read or reads empty, or no
/// controller is found. Without native multipath `device` is the controller;
/// with it (Fedora's default) it is the NVMe subsystem, which has no transport
/// of its own (a missing file there is expected, any other error is not) and
/// whose `nvme<digits>` entries are its controllers; newer kernels also list
/// each path under `multipath/`.
pub fn nvme_transport(sys: &Path, name: &str) -> Option<String> {
    let block = fs::canonicalize(sys.join("class/block").join(name)).ok()?;
    let subsystem = block.join("device");
    let mut found: Vec<String> = Vec::new();
    match fs::read_to_string(subsystem.join("transport")) {
        Ok(text) => found.push(text.trim().to_owned()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    for entry in directory_names(&subsystem).ok()? {
        if is_nvme_controller(&entry) {
            let text = fs::read_to_string(subsystem.join(&entry).join("transport")).ok()?;
            found.push(text.trim().to_owned());
        }
    }
    for path in directory_names(&block.join("multipath")).ok()? {
        let text =
            fs::read_to_string(block.join("multipath").join(path).join("device/transport")).ok()?;
        found.push(text.trim().to_owned());
    }
    if found.is_empty() || found.iter().any(String::is_empty) {
        return None;
    }
    found
        .iter()
        .find(|t| t.as_str() != NVME_LOCAL)
        .or_else(|| found.first())
        .cloned()
}

/// The names in a directory: none if it is not there, an error for any other
/// failure of the directory or one of its entries (a directory that cannot be
/// read is not an empty one).
fn directory_names(dir: &Path) -> io::Result<Vec<String>> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for entry in read {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    Ok(names)
}

/// How a disk is attached, from lsblk's transport and the device's name.
pub fn bus_of(tran: &str, kname: &str) -> String {
    match tran {
        "" => {
            if kname.starts_with("nvme") {
                "nvme"
            } else if kname.starts_with("mmcblk") {
                "mmc"
            } else if kname.starts_with("vd") {
                "virtio"
            } else {
                "unknown"
            }
        }
        "ata" | "sata" => "sata",
        other => other,
    }
    .into()
}

/// A disk before its stable id and the sysfs-only facts are added.
pub struct Raw {
    pub kname: String,
    pub disk: Disk,
}

/// Parse `lsblk --json --bytes --paths` into disks with their partitions.
/// `start_of` gives a partition's first sector in 512-byte units (sysfs);
/// `nvme_transport_of` an NVMe disk's controller transport.
pub fn parse_lsblk(
    json: &str,
    start_of: &dyn Fn(&str) -> Option<u64>,
    nvme_transport_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Raw>> {
    let root: Value = serde_json::from_str(json).context("reading lsblk")?;
    let devices = root["blockdevices"]
        .as_array()
        .context("lsblk has no blockdevices")?;
    let mut disks = Vec::new();
    for device in devices {
        let path = text(device, "name");
        let kname = kernel_name(&text(device, "kname"));
        let nvme_transport = kname
            .starts_with("nvme")
            .then(|| nvme_transport_of(&kname))
            .flatten();
        if text(device, "type") != "disk"
            || !is_installable(&kname)
            || is_network(&kname, &text(device, "tran"), nvme_transport.as_deref())
            || flag(device, "ro")
            || number(device, "size") == 0
        {
            continue;
        }
        let mut partitions: Vec<Partition> = device["children"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter(|child| text(child, "type") == "part")
            .map(|child| {
                let part_kname = kernel_name(&text(child, "kname"));
                let number = part_kname
                    .strip_prefix(&kname)
                    .map(|rest| rest.trim_start_matches('p'))
                    .and_then(|rest| rest.parse().ok())
                    .unwrap_or(0);
                Partition {
                    number,
                    path: text(child, "name"),
                    start_bytes: start_of(&part_kname).unwrap_or(0) * 512,
                    size_bytes: number_of(child),
                    type_id: text(child, "parttype").to_ascii_lowercase(),
                    label: text(child, "partlabel"),
                    filesystem: text(child, "fstype"),
                    filesystem_label: text(child, "label"),
                }
            })
            .collect();
        partitions.sort_by_key(|p| p.start_bytes);
        let table = text(device, "pttype");
        disks.push(Raw {
            disk: Disk {
                id: String::new(),
                path,
                model: text(device, "model"),
                serial: text(device, "serial"),
                size_bytes: number(device, "size"),
                sector_size: number(device, "log-sec").max(512) as u32,
                bus: bus_of(&text(device, "tran"), &kname),
                removable: flag(device, "rm"),
                live_medium: false,
                in_use: vec![],
                notes: vec![],
                partition_table: (!table.is_empty()).then_some(table),
                partitions,
                free_extents: vec![],
                other_systems: vec![],
            },
            kname,
        });
    }
    Ok(disks)
}

fn number_of(value: &Value) -> u64 {
    number(value, "size")
}

/// The file system signature of every block device lsblk lists (disks,
/// partitions, loop and stacked devices), by kernel name: what the topology
/// asks when a file system was detached and no mount says what it was.
pub fn device_types(json: &str) -> Result<BTreeMap<String, String>> {
    fn walk(devices: &[Value], types: &mut BTreeMap<String, String>) {
        for device in devices {
            let kname = kernel_name(&text(device, "kname"));
            let fstype = text(device, "fstype");
            if !kname.is_empty() && !fstype.is_empty() {
                types.insert(kname, fstype);
            }
            if let Some(children) = device["children"].as_array() {
                walk(children, types);
            }
        }
    }
    let root: Value = serde_json::from_str(json).context("reading lsblk")?;
    let mut types = BTreeMap::new();
    walk(
        root["blockdevices"]
            .as_array()
            .context("lsblk has no blockdevices")?,
        &mut types,
    );
    Ok(types)
}

/// The signature `blkid` finds on a device by reading it (a low-level probe,
/// not udev's database), for a device lsblk does not name a file system of.
pub fn probe_type(path: &str) -> Option<String> {
    let out = exec::run(
        "blkid",
        &["--probe", "--output", "value", "--match-tag", "TYPE", path],
        Duration::from_secs(10),
    )
    .ok()?;
    let text = out.stdout_text().trim().to_owned();
    (out.success() && !text.is_empty()).then_some(text)
}

/// What opening a whole disk exclusively says about it.
#[derive(Debug, PartialEq, Eq)]
pub enum Claim {
    /// Nothing holds the disk or any of its partitions.
    Free,
    /// The kernel refuses (EBUSY): something holds the disk or a partition.
    Busy,
    /// The check itself failed.
    Unknown(String),
}

/// How often and how far apart a busy disk is tried: a claim that is only
/// passing (a `mkfs` or file system check someone runs on a partition, a
/// partition rescan) must not make a disk flicker in and out of the list. The
/// helper's own scans never overlap: `Probe` and `Plan` run one at a time and
/// an ESP scan has ended before the next disk is claimed.
const CLAIM_ATTEMPTS: u32 = 10;
const CLAIM_PAUSE: Duration = Duration::from_millis(50);

/// Run `open` until it succeeds, fails for another reason or has been refused
/// `attempts` times for `EBUSY`.
pub fn claim_with(
    open: &mut dyn FnMut() -> io::Result<()>,
    attempts: u32,
    pause: Duration,
) -> Claim {
    for attempt in 0..attempts {
        match open() {
            Ok(()) => return Claim::Free,
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                if attempt + 1 < attempts {
                    thread::sleep(pause);
                }
            }
            Err(e) => return Claim::Unknown(e.to_string()),
        }
    }
    Claim::Busy
}

/// Open the block device at `path` read-only and exclusive, then close it. On a
/// block device `O_EXCL` claims it the way a mounted file system or a swap area
/// does, and the kernel refuses with `EBUSY` while anything holds it or one of
/// its partitions (a mount, even one unmounted lazily that a process still
/// holds; swap; an external journal; a zram backing device; a device-mapper,
/// MD or btrfs member). The opened file must be the device with `expect`'s
/// number.
fn open_exclusive(path: &str, expect: DevNum) -> io::Result<()> {
    open_exclusive_file(path, expect).map(drop)
}

/// The claim itself: the file, open for as long as the claim is to last.
fn open_exclusive_file(path: &str, expect: DevNum) -> io::Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_EXCL)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_block_device() || topology::split_dev(meta.rdev()) != expect {
        return Err(io::Error::other(format!(
            "{path} is not the block device {}:{}",
            expect.0, expect.1
        )));
    }
    Ok(file)
}

/// A whole disk the helper holds exclusively (`O_EXCL`), as the kernel's claim
/// on it for as long as this lives. While it does, nothing else can claim the
/// disk or any partition of it: not a `mount`, not a `swapon`, not a `mkfs`.
/// That includes the helper's own children, so it is held for the checks that
/// come right before the disk is written and let go of just before
/// `systemd-repart` starts (which formats the new partitions through their own
/// device nodes and so cannot run under it: `mkfs.vfat: unable to open
/// /dev/vda1: Device or resource busy`, measured on the live image, systemd
/// 262).
#[derive(Debug)]
pub struct Hold {
    file: fs::File,
    pub kname: String,
}

impl Hold {
    /// The claim is over: closing the descriptor releases it.
    pub fn release(self) {
        drop(self.file);
    }
}

/// Run `open` until it succeeds, fails for another reason or has been refused
/// `attempts` times for `EBUSY`: [`claim_with`] for a claim that is kept.
pub fn hold_with(
    open: &mut dyn FnMut() -> io::Result<fs::File>,
    attempts: u32,
    pause: Duration,
) -> Result<fs::File, Claim> {
    for attempt in 0..attempts {
        match open() {
            Ok(file) => return Ok(file),
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => {
                if attempt + 1 < attempts {
                    thread::sleep(pause);
                }
            }
            Err(e) => return Err(Claim::Unknown(e.to_string())),
        }
    }
    Err(Claim::Busy)
}

/// Claim `path` (the whole disk `kname`) exclusively and keep the claim.
pub fn hold_disk(sys: &Path, kname: &str, path: &str) -> Result<Hold, Claim> {
    let Some(expect) = device_number(sys, kname) else {
        return Err(Claim::Unknown(format!(
            "sysfs does not give the number of {kname}"
        )));
    };
    let file = hold_with(
        &mut || open_exclusive_file(path, expect),
        CLAIM_ATTEMPTS,
        CLAIM_PAUSE,
    )?;
    Ok(Hold {
        file,
        kname: kname.to_owned(),
    })
}

/// [`hold_disk`] on this system.
pub fn hold(disk: &Disk) -> Result<Hold, String> {
    let kname = kernel_name(&disk.path);
    hold_disk(&Roots::real().sys, &kname, &disk.path)
        .map_err(|claim| claim_reason(&disk.path, &claim).unwrap_or_default())
}

/// The device number sysfs gives a block device.
fn device_number(sys: &Path, kname: &str) -> Option<DevNum> {
    let text = fs::read_to_string(sys.join("class/block").join(kname).join("dev")).ok()?;
    let (major, minor) = text.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Claim a whole disk exclusively and release it at once. Everything but a free
/// disk is a reason it is not offered.
pub fn claim_disk(sys: &Path, kname: &str, path: &str) -> Claim {
    let Some(expect) = device_number(sys, kname) else {
        return Claim::Unknown(format!("sysfs does not give the number of {kname}"));
    };
    claim_with(
        &mut || open_exclusive(path, expect),
        CLAIM_ATTEMPTS,
        CLAIM_PAUSE,
    )
}

/// Why a disk is not free, when [`claim_disk`] says it is not.
pub fn claim_reason(path: &str, claim: &Claim) -> Option<String> {
    match claim {
        Claim::Free => None,
        Claim::Busy => Some(format!(
            "{path} is held open exclusively by the running system (a mounted partition, even one unmounted lazily that a process still holds, swap, a file system journal, a zram backing device or a device stacked on it), though none of what the installer traced says what"
        )),
        Claim::Unknown(why) => Some(format!(
            "{path} could not be opened to check whether it is in use ({why})"
        )),
    }
}

/// The last look at a disk a plan is about to name: claimed again, now.
pub fn recheck(disk: &Disk) -> Option<String> {
    let kname = kernel_name(&disk.path);
    claim_reason(
        &disk.path,
        &claim_disk(&Roots::real().sys, &kname, &disk.path),
    )
}

/// `sfdisk --json` of a disk: its usable range in bytes, `[first, end)`.
pub fn parse_sfdisk(json: &str) -> Option<(u64, u64)> {
    let table: Value = serde_json::from_str(json).ok()?;
    let table = &table["partitiontable"];
    let sector = table["sectorsize"].as_u64().unwrap_or(512);
    let first = table["firstlba"].as_u64()?;
    let last = table["lastlba"].as_u64()?;
    Some((first * sector, (last + 1) * sector))
}

fn align_up(value: u64, to: u64) -> u64 {
    value.div_ceil(to) * to
}

/// The unallocated space of a disk in whole MiB: the gaps between partitions
/// inside `[first, end)`. Without a partition table's own range the usable
/// area is the disk less its first and last MiB (room for GPT's two copies).
pub fn free_extents(
    size: u64,
    usable: Option<(u64, u64)>,
    partitions: &[Partition],
) -> Vec<Extent> {
    let (first, end) = usable.unwrap_or((0, size.saturating_sub(MIB)));
    let first = align_up(first.max(MIB), MIB);
    let end = end.min(size) / MIB * MIB;
    let mut cursor = first;
    let mut extents = Vec::new();
    let mut taken: Vec<(u64, u64)> = partitions
        .iter()
        .map(|p| (p.start_bytes, p.start_bytes + p.size_bytes))
        .collect();
    taken.sort_unstable();
    for (start, stop) in taken {
        if start > cursor {
            let gap_start = align_up(cursor, MIB);
            let gap_end = start / MIB * MIB;
            if gap_end > gap_start {
                extents.push(Extent {
                    start_bytes: gap_start,
                    size_bytes: gap_end - gap_start,
                });
            }
        }
        cursor = cursor.max(stop);
    }
    let gap_start = align_up(cursor, MIB);
    if end > gap_start {
        extents.push(Extent {
            start_bytes: gap_start,
            size_bytes: end - gap_start,
        });
    }
    extents
}

fn sysfs_start(name: &str) -> Option<u64> {
    fs::read_to_string(Path::new("/sys/class/block").join(name).join("start"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// A stable name for each disk: `/dev/disk/by-id` when the hardware has an
/// identity, else the bus path, else the kernel name.
pub fn stable_ids(kname: &str, links: &[(String, String)]) -> String {
    // (directory, name) pairs: whole-disk links only, by preference.
    let mut best: Option<(u8, &str)> = None;
    for (directory, name) in links {
        let rank = match (directory.as_str(), name.as_str()) {
            ("by-id", n) if n.contains("-part") => continue,
            ("by-id", n) if n.starts_with("nvme-eui.") => 0,
            ("by-id", n) if n.starts_with("wwn-") => 1,
            ("by-id", _) => 2,
            ("by-path", n) if n.contains("-part") => continue,
            ("by-path", _) => 3,
            _ => continue,
        };
        if best.is_none_or(|(r, n)| (rank, name.as_str()) < (r, n)) {
            best = Some((rank, name));
        }
    }
    best.map_or_else(|| kname.to_owned(), |(_, name)| name.to_owned())
}

fn dev_links(by: &str, kname: &str) -> Vec<(String, String)> {
    let Ok(entries) = fs::read_dir(Path::new("/dev/disk").join(by)) else {
        return vec![];
    };
    entries
        .flatten()
        .filter(|e| {
            fs::canonicalize(e.path())
                .is_ok_and(|target| target.file_name().is_some_and(|n| n == kname))
        })
        .map(|e| (by.to_owned(), e.file_name().to_string_lossy().into_owned()))
        .collect()
}

/// What a partition says about the system that put it there, from its
/// filesystem and type alone.
pub fn classify(partition: &Partition) -> Option<OtherSystem> {
    let (kind, what) = match partition.filesystem.as_str() {
        "" | "swap" => {
            if partition.type_id == MICROSOFT_RESERVED {
                ("windows", "Microsoft reserved partition".to_string())
            } else {
                return None;
            }
        }
        "ntfs" | "refs" => (
            "windows",
            format!("{} filesystem", partition.filesystem.to_uppercase()),
        ),
        "BitLocker" => ("bitlocker", "BitLocker-encrypted Windows volume".into()),
        "ext2" | "ext3" | "ext4" | "xfs" | "btrfs" | "f2fs" | "LVM2_member"
        | "linux_raid_member" => ("linux", format!("{} filesystem", partition.filesystem)),
        "crypto_LUKS" => ("encrypted", "LUKS-encrypted volume".into()),
        "apfs" | "hfsplus" => ("apple", format!("{} filesystem", partition.filesystem)),
        _ => return None,
    };
    let name = if partition.filesystem_label.is_empty() {
        what
    } else {
        format!("{what} \"{}\"", partition.filesystem_label)
    };
    Some(OtherSystem {
        kind: kind.into(),
        partition: Some(partition.path.clone()),
        description: name,
    })
}

/// The names of the loader directories of a mounted ESP (`<root>/EFI/*`)
/// other than the fallback path. A link is never followed: only real
/// directories count.
pub fn efi_loader_names(esp_root: &Path) -> Vec<String> {
    let efi = esp_root.join("EFI");
    if !fs::symlink_metadata(&efi).is_ok_and(|m| m.is_dir()) {
        return vec![];
    }
    let Ok(entries) = fs::read_dir(&efi) else {
        return vec![];
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.eq_ignore_ascii_case("BOOT"))
        .collect();
    names.sort();
    names.truncate(16);
    names
}

/// What those loader directories say about the computer's other systems.
pub fn loader_systems(names: &[String], partition: &str) -> Vec<OtherSystem> {
    names
        .iter()
        .map(|name| OtherSystem {
            kind: "efi-loader".into(),
            partition: Some(partition.into()),
            description: if name.eq_ignore_ascii_case("microsoft") {
                "Windows Boot Manager (EFI/Microsoft)".into()
            } else {
                // A name from a disk is shown to a person: nothing but
                // printable characters.
                let clean: String = name.chars().filter(|c| !c.is_control()).take(64).collect();
                format!("EFI loader directory EFI/{clean}")
            },
        })
        .collect()
}

/// The loader directories of a mounted ESP as systems found.
pub fn efi_loaders(esp_root: &Path, partition: &str) -> Vec<OtherSystem> {
    loader_systems(&efi_loader_names(esp_root), partition)
}

/// What `ilex-installerd --scan-esp` prints: the child process that mounts one
/// ESP in a mount namespace of its own.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EspScan {
    pub loaders: Vec<String>,
    /// The mount failed.
    pub error: Option<String>,
    /// Unmounting failed (the namespace's end drops the mount regardless).
    pub unmount_error: Option<String>,
}

/// The `--scan-esp` child: mount the ESP read-only (no execution, no devices,
/// no setuid) at `mountpoint`, list its loaders, unmount it explicitly. It runs
/// inside `unshare --mount`, so the mount is visible to this process only and
/// disappears when it exits, whatever happens here.
pub fn scan_esp_child(device: &str, mountpoint: &Path) -> EspScan {
    let mut scan = EspScan::default();
    if !device.starts_with("/dev/") || !fs::symlink_metadata(mountpoint).is_ok_and(|m| m.is_dir()) {
        scan.error = Some("not a device and a plain directory".into());
        return scan;
    }
    let mount = exec::run(
        "mount",
        &[
            "-t",
            "vfat",
            "-o",
            "ro,noexec,nodev,nosuid",
            device,
            &mountpoint.to_string_lossy(),
        ],
        TOOL_TIMEOUT,
    );
    match mount {
        Ok(out) if out.success() => {}
        Ok(out) => {
            scan.error = Some(format!(
                "mount failed: {}",
                out.stderr.lines().last().unwrap_or("no message")
            ));
            return scan;
        }
        Err(e) => {
            scan.error = Some(format!("{e:#}"));
            return scan;
        }
    }
    scan.loaders = efi_loader_names(mountpoint);
    match exec::run("umount", &[&mountpoint.to_string_lossy()], TOOL_TIMEOUT) {
        Ok(out) if out.success() => {}
        Ok(out) => {
            scan.unmount_error = Some(format!(
                "umount failed: {}",
                out.stderr.lines().last().unwrap_or("no message")
            ));
        }
        Err(e) => scan.unmount_error = Some(format!("{e:#}")),
    }
    scan
}

/// List an ESP's loaders by running this program's `--scan-esp` in a mount
/// namespace of its own. Failure is reported, not hidden: the other systems
/// on that disk may then be missing from the list.
fn scan_esp(device: &str, scratch: &mut Scratch, n: usize) -> (Vec<OtherSystem>, Vec<String>) {
    let unreadable = |why: String| {
        (
            vec![],
            vec![format!(
                "could not read the EFI system partition {device} ({why}); other systems on this disk may be missing from the list"
            )],
        )
    };
    let (Ok(exe), Ok(mountpoint)) = (std::env::current_exe(), scratch.subdir(&format!("esp{n}")))
    else {
        return unreadable("no private mount point".into());
    };
    let (exe, mountpoint) = (
        exe.to_string_lossy().into_owned(),
        mountpoint.to_string_lossy().into_owned(),
    );
    let out = exec::run_limited(
        "unshare",
        &[
            "--mount",
            "--propagation",
            "private",
            "--",
            &exe,
            "--scan-esp",
            device,
            &mountpoint,
        ],
        TOOL_TIMEOUT,
        exec::Limits {
            stdout: 64 << 10,
            stderr: 64 << 10,
        },
    );
    let scan: EspScan = match out {
        Ok(out) if out.success() => match serde_json::from_slice(&out.stdout) {
            Ok(scan) => scan,
            Err(e) => return unreadable(format!("unreadable answer: {e}")),
        },
        Ok(out) => {
            return unreadable(out.stderr.lines().last().unwrap_or("it failed").to_owned());
        }
        Err(e) => return unreadable(format!("{e:#}")),
    };
    if let Some(error) = scan.error {
        return unreadable(error);
    }
    let mut notes = vec![];
    if let Some(error) = scan.unmount_error {
        // The mount lived in the child's namespace, which has ended.
        notes.push(format!(
            "{device}: {error} (the private mount ended with its scan)"
        ));
    }
    (loader_systems(&scan.loaders, device), notes)
}

/// A reason every disk is refused: the live medium, or something in use, could
/// not be located.
fn unlocated(reason: &str) -> String {
    format!(
        "the installer cannot tell whether this disk holds the medium it started from or is in use ({reason})"
    )
}

/// Scan every disk of this computer. `run_dir` holds the call's private
/// scratch directory, where an ESP is mounted for reading.
pub fn scan(run_dir: &Path) -> Result<Vec<Disk>> {
    scan_with(run_dir, &Roots::real(), None)
}

/// [`scan`] while the helper holds the disk `held` exclusively: that disk's
/// claim is not asked for again (it would be refused: the holder is the helper
/// itself) and its partitions are not mounted for reading (the claim would
/// refuse that too); everything else about it, and every other disk, is read
/// as in [`scan`].
pub fn scan_held(run_dir: &Path, held: &Hold) -> Result<Vec<Disk>> {
    scan_with(run_dir, &Roots::real(), Some(&held.kname))
}

fn scan_with(run_dir: &Path, roots: &Roots, held: Option<&str>) -> Result<Vec<Disk>> {
    let mut scratch = Scratch::new(run_dir, "scan")?;
    let output = exec::run_ok(
        "lsblk",
        &["--json", "--bytes", "--paths", "--output", LSBLK_COLUMNS],
        TOOL_TIMEOUT,
    )?;
    let lsblk = output.stdout_text();
    let mut raws = parse_lsblk(&lsblk, &sysfs_start, &|name| {
        nvme_transport(&roots.sys, name)
    })?;
    let types = device_types(&lsblk)?;
    // What the system is using is read or the probe fails: the mount tables of
    // every namespace, the swap areas and the loop devices (the kernel's own
    // record of each one's file), none of which may look empty for being
    // unreadable.
    let (mounts, local, unreadable) = topology::read_mounts(&roots.proc)?;
    let swaps = topology::read_swaps(&roots.proc.join("swaps"))?;
    let loops = topology::parse_losetup(
        &exec::run_ok("losetup", &topology::LOSETUP_ARGS, TOOL_TIMEOUT)?.stdout_text(),
    )?;
    let system = System {
        mounts,
        local,
        swaps,
        loops,
        unreadable,
    };
    // The signature of a device that was detached from its mounts: what lsblk
    // read from udev's database, else what blkid finds on the device itself.
    let device_type = |name: &str| {
        types
            .get(name)
            .cloned()
            .or_else(|| probe_type(&format!("/dev/{name}")))
    };
    let tracer = Tracer::new(roots, &system).with_device_types(&device_type);
    let usage = tracer.usage();
    let mut taken = BTreeSet::new();
    let mut esp_count = 0;
    for raw in &mut raws {
        let disk = &mut raw.disk;
        let links: Vec<(String, String)> = ["by-id", "by-path"]
            .iter()
            .flat_map(|by| dev_links(by, &raw.kname))
            .collect();
        let mut id = stable_ids(&raw.kname, &links);
        if !taken.insert(id.clone()) {
            id = format!("{id}@{}", raw.kname);
            taken.insert(id.clone());
        }
        disk.id = id;
        disk.live_medium = usage.live.contains(&raw.kname);
        // What the running system is doing with it: a mounted partition or
        // swap (usage names the disk through its partitions), and anything
        // stacked on the disk or one of its partitions.
        let mut reasons: BTreeSet<String> =
            usage.in_use.get(&raw.kname).cloned().unwrap_or_default();
        for name in std::iter::once(raw.kname.clone())
            .chain(disk.partitions.iter().map(|p| kernel_name(&p.path)))
        {
            match tracer.holders(&name) {
                Ok(holders) if holders.is_empty() => {}
                Ok(holders) => {
                    reasons.insert(format!(
                        "/dev/{name} is used by {} (an LVM, MD or dm-crypt device)",
                        holders.join(", ")
                    ));
                }
                Err(why) => {
                    reasons.insert(unlocated(&why));
                }
            }
        }
        if let Some(why) = usage.unresolved.first() {
            reasons.insert(unlocated(why));
        }
        // The kernel's own claim on the disk is the authority on everything it
        // shows: what the trace did not find, an exclusive open does. A disk
        // that is the live medium or is already in use needs no second answer.
        let ours = held == Some(raw.kname.as_str());
        if reasons.is_empty() && !disk.live_medium && !ours {
            let claim = claim_disk(&roots.sys, &raw.kname, &disk.path);
            reasons.extend(claim_reason(&disk.path, &claim));
        }
        disk.in_use = reasons.into_iter().collect();
        let usable = if disk.partition_table.is_some() {
            exec::run("sfdisk", &["--json", "--", &disk.path], TOOL_TIMEOUT)
                .ok()
                .filter(exec::Output::success)
                .and_then(|o| parse_sfdisk(&o.stdout_text()))
        } else {
            None
        };
        disk.free_extents = free_extents(disk.size_bytes, usable, &disk.partitions);
        for partition in disk.partitions.clone() {
            if partition.type_id == ESP_TYPE && !ours {
                esp_count += 1;
                let (systems, notes) = scan_esp(&partition.path, &mut scratch, esp_count);
                disk.other_systems.extend(systems);
                disk.notes.extend(notes);
            }
            disk.other_systems.extend(classify(&partition));
        }
    }
    raws.sort_by(|a, b| a.disk.id.cmp(&b.disk.id));
    Ok(raws.into_iter().map(|r| r.disk).collect())
}
