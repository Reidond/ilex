//! Installing beside what is on the disk ("free space" mode): what a disk must
//! look like for it, where `systemd-repart` will put the two partitions, and
//! how a leftover of an earlier attempt is told from anything else.
//!
//! Free-space mode is additive. It creates ilex's `/boot` and root in
//! unallocated space and shares the disk's EFI system partition; it never
//! formats, resizes, moves or deletes a partition that is there. Every rule
//! here is a reason that is true, and a refusal says in a sentence which one
//! is not. The helper applies the same rules to the probe it makes, the
//! interface applies them to show what is possible, and the simulation applies
//! them to the fixtures it plays.

use crate::plan::{BOOT_BYTES, MIN_ROOT_BYTES};
use crate::probe::{Disk, EspInfo, Extent, OtherSystem, Partition};

const MIB: u64 = 1024 * 1024;

/// The GPT type of an EFI system partition.
pub const ESP_TYPE: &str = "c12a7328-f81f-11d2-ba4b-00a0c93ec93b";
/// The GPT types `systemd-repart` matches ilex's own definitions to
/// (`Type=xbootldr`, `Type=root`). A partition of either type that is on the
/// disk already would be taken for ilex's and grown or left alone instead of
/// a new one being made, so a disk with one is refused.
pub const XBOOTLDR_TYPE: &str = "bc13c2ff-59e6-4262-a352-b275fd6f7172";
pub const ROOT_X86_64_TYPE: &str = "4f68bce3-e8cd-4db1-96e7-fbcaf984b709";

/// What ilex's boot files need free on a shared EFI system partition: its
/// shim, GRUB and the fallback loader are about 8 MiB, and an update writes
/// each new file beside the old one. (Measured: `bootupctl backend install`
/// of `generic.gnome` added 8,074 KiB of files to an empty FAT32 partition.)
pub const ESP_MIN_FREE_BYTES: u64 = 24 * MIB;

/// The GPT type of a BIOS boot partition. `bootupd` may install GRUB for a BIOS
/// boot there and into the MBR's boot code (bytes 0 to 440); a disk that has one
/// belongs to another system's BIOS boot chain, which ilex does not go near.
pub const BIOS_BOOT_TYPE: &str = "21686148-6449-6e6f-744e-656564454649";

/// The partitions of a disk that ilex's installer named (`ilex-` label), of
/// whatever type.
pub fn ilex_partitions(disk: &Disk) -> Vec<&Partition> {
    disk.partitions
        .iter()
        .filter(|p| p.label.starts_with("ilex-"))
        .collect()
}

/// The partition of the disk that is its EFI system partition, when there is
/// exactly one.
pub fn esp_partitions(disk: &Disk) -> Vec<&Partition> {
    disk.partitions
        .iter()
        .filter(|p| p.type_id == ESP_TYPE)
        .collect()
}

/// `31 GiB`, `30.9 GiB`: rounded down, so that a size just short of a limit
/// never reads as the limit.
fn gib(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes.is_multiple_of(GIB) || bytes >= 100 * GIB {
        format!("{} GiB", bytes / GIB)
    } else {
        let tenths = bytes * 10 / GIB;
        format!("{}.{} GiB", tenths / 10, tenths % 10)
    }
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes / MIB)
}

/// Where `systemd-repart` puts ilex's two partitions: it takes each new
/// partition in turn (`/boot`, then the root) and puts it in the *smallest*
/// free area that holds its minimum size (measured with systemd 262: with a
/// 5 GiB and a 60 GiB gap, `/boot` went to the 5 GiB gap; with 80 GiB and
/// 118 GiB, both went to the smaller), and the root then grows to fill the
/// area it is in. A person does not choose the area: the page says where ilex
/// will go, and the plan is repart's own dry run of exactly this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    /// The free area `/boot` goes to (before `/boot` is taken out of it).
    pub boot: Extent,
    /// The free area the root goes to, after `/boot` is taken out of it where
    /// the two share one: the root fills all of it.
    pub root: Extent,
}

/// The smallest area of `free` that holds `need` bytes, the first of equals.
fn smallest_fit(free: &[Extent], need: u64) -> Option<usize> {
    free.iter()
        .enumerate()
        .filter(|(_, e)| e.size_bytes >= need)
        .min_by_key(|(i, e)| (e.size_bytes, *i))
        .map(|(i, _)| i)
}

/// [`Placement`] of `/boot` (1 GiB) and the root (30 GiB at least) in the free
/// extents of a disk.
pub fn place(extents: &[Extent]) -> Option<Placement> {
    let mut free = extents.to_vec();
    free.sort_by_key(|e| e.start_bytes);
    let boot_at = smallest_fit(&free, BOOT_BYTES)?;
    let boot = free[boot_at];
    free[boot_at] = Extent {
        start_bytes: boot.start_bytes + BOOT_BYTES,
        size_bytes: boot.size_bytes - BOOT_BYTES,
    };
    let root_at = smallest_fit(&free, MIN_ROOT_BYTES)?;
    Some(Placement {
        boot,
        root: free[root_at],
    })
}

/// What a disk can be installed beside: the EFI system partition ilex shares,
/// where its partitions go, and what stays.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    /// The disk's EFI system partition, which ilex adds its boot files to.
    pub esp: Partition,
    /// What the probe read inside it.
    pub esp_info: EspInfo,
    pub placement: Placement,
    /// Every partition that is on the disk and stays as it is.
    pub kept: Vec<Partition>,
}

/// The least a disk's free space must hold for ilex in one gap (`/boot` and
/// the root together).
pub const MIN_GAP_BYTES: u64 = BOOT_BYTES + MIN_ROOT_BYTES;

/// Can ilex be installed beside what is on `disk`? `Ok` says where; `Err` is
/// the reason, a sentence for a person that starts with a lower-case clause
/// (`it has no EFI system partition ...`).
pub fn assess(disk: &Disk) -> Result<Offer, String> {
    match disk.partition_table.as_deref() {
        Some("gpt") => {}
        Some(other) => {
            return Err(format!(
                "it has a {} partition table; installing beside other systems needs a GPT disk with an EFI system partition",
                other.to_uppercase()
            ));
        }
        None => {
            return Err(
                "it has no partition table, so nothing is beside ilex; erase the whole disk instead"
                    .to_owned(),
            );
        }
    }
    // ilex's own partitions (`ilex-boot`, `ilex-root`, ...): an earlier
    // installation, finished or not. Installing beside never removes a
    // partition (the only ones it ever removes are those its own run made, as
    // its own undo), so the person takes them off with a partition tool, or
    // chooses to erase the whole disk.
    let ilex = ilex_partitions(disk);
    if !ilex.is_empty() {
        let names = ilex
            .iter()
            .map(|p| format!("{} ({})", p.label, gib(p.size_bytes)))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(format!(
            "it already holds partitions of an earlier ilex installation ({names}); installing beside other systems never removes a partition, so remove them with a partition tool first, or choose to erase the whole disk"
        ));
    }
    if let Some(bios) = disk.partitions.iter().find(|p| p.type_id == BIOS_BOOT_TYPE) {
        return Err(format!(
            "it has a BIOS boot partition ({}), which belongs to another system's BIOS boot chain and which ilex's boot loader installer could write to; installing beside it is not supported",
            bios.path
        ));
    }
    if let Some(foreign) = disk
        .partitions
        .iter()
        .find(|p| p.type_id == XBOOTLDR_TYPE || p.type_id == ROOT_X86_64_TYPE)
    {
        return Err(format!(
            "it has a partition ({}) of the type ilex uses for its own /boot or root partition, and the tool that adds ilex's partitions would take it for ilex's; installing beside it is not supported",
            foreign.path
        ));
    }
    let esps = esp_partitions(disk);
    let esp = match esps.as_slice() {
        [] => {
            return Err(
                "it has no EFI system partition to share; erase the whole disk instead".to_owned(),
            );
        }
        [one] => *one,
        _ => {
            return Err(
                "it has more than one EFI system partition, and ilex cannot tell which to share"
                    .to_owned(),
            );
        }
    };
    if esp.filesystem != "vfat" {
        let what = if esp.filesystem.is_empty() {
            "no file system".to_owned()
        } else {
            format!("a {} file system", esp.filesystem)
        };
        return Err(format!(
            "its EFI system partition ({}) holds {what}, not FAT; ilex never reformats another system's boot partition",
            esp.path
        ));
    }
    let Some(info) = disk
        .esps
        .iter()
        .find(|i| i.partition == esp.path)
        .filter(|i| i.readable)
    else {
        return Err(format!(
            "its EFI system partition ({}) could not be read, so ilex cannot tell whether it has room",
            esp.path
        ));
    };
    match info.free_bytes {
        Some(free) if free >= ESP_MIN_FREE_BYTES => {}
        Some(free) => {
            return Err(format!(
                "its EFI system partition ({}) has {} free and ilex's boot files need {}; ilex never resizes another system's boot partition",
                esp.path,
                mib(free),
                mib(ESP_MIN_FREE_BYTES)
            ));
        }
        None => {
            return Err(format!(
                "its EFI system partition ({}) did not say how much room it has",
                esp.path
            ));
        }
    }
    if info.fedora_directory {
        return Err(format!(
            "its EFI system partition ({}) already holds EFI/fedora, which another Fedora or an earlier ilex installation uses and ilex's boot files would replace",
            esp.path
        ));
    }
    let Some(placement) = place(&disk.free_extents) else {
        let largest = disk
            .free_extents
            .iter()
            .map(|e| e.size_bytes)
            .max()
            .unwrap_or(0);
        return Err(if disk.free_extents.is_empty() {
            format!(
                "it has no unallocated space; ilex needs {} (shrink a partition with the other system's own tools first)",
                gib(MIN_GAP_BYTES)
            )
        } else {
            format!(
                "its largest unallocated space is {} and ilex needs {} in one piece (shrink a partition with the other system's own tools first)",
                gib(largest),
                gib(MIN_GAP_BYTES)
            )
        });
    };
    Ok(Offer {
        esp: esp.clone(),
        esp_info: info.clone(),
        placement,
        kept: disk.partitions.clone(),
    })
}

/// A sentence for the disk page: where ilex would go, from an [`Offer`].
pub fn describe(offer: &Offer) -> String {
    format!(
        "{} of unallocated space will be used; the EFI system partition is shared and every other partition stays as it is",
        gib(offer.placement.root.size_bytes)
    )
}

/// What a kept partition is, for the review page: the other system the probe
/// recognised on it, or its file system, or its type. The EFI system partition
/// is named as the one that is shared.
pub fn kept_description(partition: &Partition, systems: &[OtherSystem]) -> String {
    let system = systems
        .iter()
        .find(|s| s.partition.as_deref() == Some(partition.path.as_str()))
        .map(|s| s.description.as_str());
    if partition.type_id == ESP_TYPE {
        return match system {
            Some(found) => format!("EFI system partition, shared ({found})"),
            None => "EFI system partition, shared".to_owned(),
        };
    }
    if let Some(found) = system {
        return found.to_owned();
    }
    match partition.filesystem.as_str() {
        "" => "no file system found".to_owned(),
        other => format!("{other} file system"),
    }
}
