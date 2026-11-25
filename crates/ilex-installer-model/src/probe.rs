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
    /// The disk the live system booted from. Never a target.
    pub live_medium: bool,
    pub partition_table: Option<String>,
    pub partitions: Vec<Partition>,
    pub free_extents: Vec<Extent>,
    pub other_systems: Vec<OtherSystem>,
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Extent {
    pub start_bytes: u64,
    pub size_bytes: u64,
}

/// Evidence of something already on a disk the person should hear about.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtherSystem {
    /// `efi-loader`, `windows`, `bitlocker`, `linux`, `apple` or `encrypted`.
    pub kind: String,
    pub partition: Option<String>,
    pub description: String,
}
