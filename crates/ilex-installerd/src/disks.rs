//! The disks of this computer: what is on them, where the free space is, and
//! which one the live system came from. Reading only: the one thing this module
//! mounts is an EFI system partition, read-only, to name the loaders in it.

use crate::exec;
use anyhow::{Context, Result};
use ilex_installer_model::probe::{Disk, Extent, OtherSystem, Partition};
use serde_json::Value;
use std::{collections::BTreeSet, fs, path::Path, time::Duration};

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

/// Devices that are not somewhere to install: compressed RAM, network block
/// devices, floppy drives.
fn is_installable(kname: &str) -> bool {
    !["zram", "ram", "nbd", "fd", "loop", "sr"]
        .iter()
        .any(|prefix| {
            kname
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.bytes().all(|b| b.is_ascii_digit()))
        })
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
/// `start_of` gives a partition's first sector in 512-byte units (sysfs).
pub fn parse_lsblk(json: &str, start_of: &dyn Fn(&str) -> Option<u64>) -> Result<Vec<Raw>> {
    let root: Value = serde_json::from_str(json).context("reading lsblk")?;
    let devices = root["blockdevices"]
        .as_array()
        .context("lsblk has no blockdevices")?;
    let mut disks = Vec::new();
    for device in devices {
        let path = text(device, "name");
        let kname = kernel_name(&text(device, "kname"));
        if text(device, "type") != "disk"
            || !is_installable(&kname)
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

/// The kernel names of the disks the live system is mounted from, read from
/// `/proc/self/mountinfo`: dracut's `dmsquash-live` mounts the medium at
/// `/run/initramfs/live`; an installer started otherwise still names its
/// optical or ISO file system. `parent` maps a partition name to its disk.
pub fn live_disks(mountinfo: &str, parent: &dyn Fn(&str) -> Option<String>) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in mountinfo.lines() {
        let Some((head, tail)) = line.split_once(" - ") else {
            continue;
        };
        let head: Vec<&str> = head.split(' ').collect();
        let tail: Vec<&str> = tail.split(' ').collect();
        let (Some(mount_point), Some(fstype), Some(source)) =
            (head.get(4), tail.first(), tail.get(1))
        else {
            continue;
        };
        if !(*mount_point == "/run/initramfs/live" || ["iso9660", "udf"].contains(fstype)) {
            continue;
        }
        if let Some(device) = source.strip_prefix("/dev/") {
            let name = device.rsplit('/').next().unwrap_or(device);
            found.insert(parent(name).unwrap_or_else(|| name.to_owned()));
        }
    }
    found
}

/// The disk a partition belongs to, from sysfs (`/sys/class/block/<name>` is a
/// symlink into its parent's directory); `None` for a whole disk.
pub fn sysfs_parent(name: &str) -> Option<String> {
    let link = fs::canonicalize(Path::new("/sys/class/block").join(name)).ok()?;
    link.join("partition").exists().then_some(())?;
    link.parent()?.file_name()?.to_str().map(str::to_owned)
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

/// The loader directories of a mounted ESP (`<root>/EFI/*`) other than the
/// fallback path, as systems found.
pub fn efi_loaders(esp_root: &Path, partition: &str) -> Vec<OtherSystem> {
    let Ok(entries) = fs::read_dir(esp_root.join("EFI")) else {
        return vec![];
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.eq_ignore_ascii_case("BOOT"))
        .collect();
    names.sort();
    names
        .into_iter()
        .take(16)
        .map(|name| OtherSystem {
            kind: "efi-loader".into(),
            partition: Some(partition.into()),
            description: if name.eq_ignore_ascii_case("microsoft") {
                "Windows Boot Manager (EFI/Microsoft)".into()
            } else {
                format!("EFI loader directory EFI/{name}")
            },
        })
        .collect()
}

/// Mount an ESP read-only (no execution, no devices, no setuid), list its
/// loaders and unmount it.
fn scan_esp(device: &str, scratch: &Path) -> Vec<OtherSystem> {
    let mountpoint = scratch.join("esp");
    if fs::create_dir_all(&mountpoint).is_err() {
        return vec![];
    }
    let mounted = exec::run(
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
    )
    .is_ok_and(|o| o.success());
    if !mounted {
        return vec![];
    }
    let found = efi_loaders(&mountpoint, device);
    let _ = exec::run("umount", &[&mountpoint.to_string_lossy()], TOOL_TIMEOUT);
    found
}

/// Scan every disk of this computer. `scratch` is a private directory for the
/// read-only ESP mount.
pub fn scan(scratch: &Path) -> Result<Vec<Disk>> {
    let output = exec::run_ok(
        "lsblk",
        &["--json", "--bytes", "--paths", "--output", LSBLK_COLUMNS],
        TOOL_TIMEOUT,
    )?;
    let mut raws = parse_lsblk(&output.stdout_text(), &sysfs_start)?;
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let live = live_disks(&mountinfo, &sysfs_parent);
    let mut taken = BTreeSet::new();
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
        disk.live_medium = live.contains(&raw.kname);
        let usable = if disk.partition_table.is_some() {
            exec::run("sfdisk", &["--json", "--", &disk.path], TOOL_TIMEOUT)
                .ok()
                .filter(exec::Output::success)
                .and_then(|o| parse_sfdisk(&o.stdout_text()))
        } else {
            None
        };
        disk.free_extents = free_extents(disk.size_bytes, usable, &disk.partitions);
        for partition in &disk.partitions {
            if partition.type_id == ESP_TYPE {
                disk.other_systems
                    .extend(scan_esp(&partition.path, scratch));
            }
            disk.other_systems.extend(classify(partition));
        }
    }
    raws.sort_by(|a, b| a.disk.id.cmp(&b.disk.id));
    Ok(raws.into_iter().map(|r| r.disk).collect())
}
