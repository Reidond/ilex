//! What the root helper reports about this computer: `Probe()`'s document.

use crate::catalogue::Table;
use crate::detect::{Detection, Selected};
use serde::{Deserialize, Serialize};

pub const PROBE_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Probe {
    pub schema: u32,
    pub helper: HelperInfo,
    pub edition: EditionInfo,
    pub firmware: Firmware,
    /// `ilex-enroll`'s probe verdict, verbatim: the one implementation of "is
    /// this machine eligible for automatic unlock".
    pub enrollment: serde_json::Value,
    pub detection: Detection,
    /// The decision after the boot override and choice file, or why it failed.
    pub selection: Option<Selected>,
    pub selection_error: Option<String>,
    /// This edition's catalogue: its compositions, machines and default.
    pub catalogue: Table,
    pub disks: Vec<Disk>,
    pub memory: Memory,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HelperInfo {
    pub version: String,
    pub systemd: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditionInfo {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Firmware {
    /// The ilex installer supports UEFI only.
    pub uefi: bool,
    pub secure_boot: Option<bool>,
    pub setup_mode: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Memory {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    /// Stable across boots when the hardware has an identity: a
    /// `/dev/disk/by-id` name, else the PCI path name, else the kernel name.
    pub id: String,
    /// The kernel device now: informational, never an input.
    pub path: String,
    pub model: String,
    pub serial: String,
    pub size_bytes: u64,
    pub sector_size: u32,
    pub bus: String,
    pub removable: bool,
    /// The disk the live system booted from, found through its mounts, the
    /// devices stacked on it and the files loop devices are backed by. Never
    /// a target.
    pub live_medium: bool,
    /// Why the running system is using this disk (a mounted partition,
    /// active swap, a loop device attached to a file or node on it, a
    /// device-mapper or MD device built on it), or why the helper cannot tell
    /// (a use it could not trace marks every disk): one sentence each. A disk
    /// with any is never a target.
    #[serde(default)]
    pub in_use: Vec<String>,
    /// What the probe could not read (an EFI system partition that would not
    /// mount): the list of other systems may then be incomplete.
    #[serde(default)]
    pub notes: Vec<String>,
    pub partition_table: Option<String>,
    pub partitions: Vec<Partition>,
    pub free_extents: Vec<Extent>,
    pub other_systems: Vec<OtherSystem>,
    /// What the probe read from inside each EFI system partition (mounted
    /// read-only for the look): installing beside another system reuses one,
    /// so how much room it has and what it already holds decide whether that
    /// can be done. Empty for a disk the helper holds (its partitions are not
    /// mounted then); the plan and `Apply` read it from the probe made before
    /// the claim.
    #[serde(default)]
    pub esps: Vec<EspInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Partition {
    pub number: u32,
    pub path: String,
    pub start_bytes: u64,
    pub size_bytes: u64,
    /// GPT type GUID (lower case) or the DOS type byte.
    pub type_id: String,
    pub label: String,
    pub filesystem: String,
    pub filesystem_label: String,
    /// The partition's own UUID (GPT's `PARTUUID`), the name that stays its
    /// own when the kernel renumbers a disk's partitions. Empty on a disk
    /// with a DOS table or when the probe could not read it.
    #[serde(default)]
    pub uuid: String,
}

/// What the probe found inside one EFI system partition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EspInfo {
    /// The partition's node, as in [`Partition::path`].
    pub partition: String,
    /// The partition could be mounted and read. When it could not, nothing
    /// else here is known and a plan beside other systems refuses it.
    pub readable: bool,
    /// Bytes the file system holds in all, and how many of them are free.
    pub total_bytes: Option<u64>,
    pub free_bytes: Option<u64>,
    /// The directories below `EFI/`, other than the fallback `EFI/BOOT`
    /// (`Microsoft`, `ubuntu`, ...), at most sixteen, sorted.
    #[serde(default)]
    pub loaders: Vec<String>,
    /// `EFI/fedora` exists (any case): the directory ilex's own boot files go
    /// to, which another Fedora or an earlier ilex installation already uses.
    #[serde(default)]
    pub fedora_directory: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub start_bytes: u64,
    pub size_bytes: u64,
}

/// Evidence of something already on a disk the person should hear about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtherSystem {
    /// `efi-loader`, `windows`, `bitlocker`, `linux`, `apple`, `encrypted`,
    /// `filesystem` (a signature on the whole disk that is none of those) or
    /// `unreadable-esp` (an EFI system partition that could not be read).
    /// A signature on the whole disk has no `partition`.
    pub kind: String,
    pub partition: Option<String>,
    pub description: String,
}
