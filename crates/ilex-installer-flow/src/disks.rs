//! The disk page's content, made from the helper's probe: which disks the
//! installation can go to and what is on each, why the others are not offered,
//! and the ways of installing: erase the whole disk, or install beside what is
//! on it when it has the room and an EFI system partition to share (the model's
//! own rule, `free_space::assess`, so the page says what the helper will).

use crate::flow::human_bytes;
use ilex_installer_model::free_space::{self, Offer};
use ilex_installer_model::plan::{MIN_DISK_BYTES, Mode};
use ilex_installer_model::probe::{Disk, Probe};

/// One disk of this computer, as the page lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskChoice {
    /// The disk's stable id: what `Plan` takes.
    pub id: String,
    /// The model the disk reports, or its id.
    pub title: String,
    /// Size, how it is attached and its serial number.
    pub subtitle: String,
    /// What is on it, one line each: the partitions, and every other system
    /// the probe found. Whole-disk mode erases all of it.
    pub contents: Vec<String>,
    /// Something to know before choosing it (a removable disk).
    pub warning: Option<String>,
    /// Why the disk is not offered; `None` when it is.
    pub unavailable: Option<String>,
    /// Installing beside what is on the disk: what that would do, or the
    /// sentence that says why it cannot be done here.
    pub beside: Result<Beside, String>,
}

/// What installing beside the other systems of a disk does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Beside {
    /// One line for the mode's row: how much unallocated space is used.
    pub summary: String,
    /// One line each: where ilex goes, what is shared and what stays.
    pub lines: Vec<String>,
}

impl DiskChoice {
    pub fn is_offered(&self) -> bool {
        self.unavailable.is_none()
    }

    /// What the disk's row says under its title, one line each: size and
    /// connection, what is on it, the warning, and for a disk that is not
    /// offered why.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = vec![self.subtitle.clone()];
        lines.extend(self.contents.iter().cloned());
        lines.extend(self.warning.iter().cloned());
        if let Some(why) = &self.unavailable {
            lines.push(format!("Not available: {why}."));
        }
        lines
    }
}

/// A way of installing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeChoice {
    pub mode: Mode,
    pub title: &'static str,
    /// What the way does on the chosen disk, or why it cannot be used there.
    pub subtitle: String,
    pub available: bool,
}

/// Everything the disk page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskView {
    /// The disks that can be installed to first, then the ones that cannot,
    /// each group in the probe's order.
    pub choices: Vec<DiskChoice>,
}

/// How the disk is attached, as a person says it.
fn bus_word(bus: &str) -> String {
    match bus {
        "nvme" => "NVMe".into(),
        "sata" => "SATA".into(),
        "usb" => "USB".into(),
        "mmc" => "SD card or eMMC".into(),
        "virtio" => "virtual disk".into(),
        "scsi" => "SCSI".into(),
        "unknown" | "" => "unknown connection".into(),
        other => other.to_owned(),
    }
}

/// Why `disk` cannot be installed to, as a clause (`it is too small (…)`), or
/// `None` when it can.
pub fn unavailable_reason(disk: &Disk) -> Option<String> {
    if disk.live_medium {
        Some("it is the medium the installer started from".to_owned())
    } else if let Some(reason) = disk.in_use.first() {
        Some(format!("it is in use: {reason}"))
    } else if disk.size_bytes < MIN_DISK_BYTES {
        Some(format!(
            "it is too small ({}; at least {} are needed)",
            human_bytes(disk.size_bytes),
            human_bytes(MIN_DISK_BYTES)
        ))
    } else {
        None
    }
}

/// The name a person knows a disk by: its model, or its id.
pub fn title_of(disk: &Disk) -> String {
    if disk.model.trim().is_empty() {
        disk.id.clone()
    } else {
        disk.model.trim().to_owned()
    }
}

/// Model and serial number: what the review page and the confirmation name
/// the disk by.
pub fn label_of(disk: &Disk) -> String {
    let serial = disk.serial.trim();
    if serial.is_empty() {
        title_of(disk)
    } else {
        format!("{} {serial}", title_of(disk))
    }
}

/// The same two names for a disk as the plan records it.
pub fn planned_title(plan: &ilex_installer_model::plan::Plan) -> String {
    let model = plan.disk.model.trim();
    if model.is_empty() {
        plan.disk.id.clone()
    } else {
        model.to_owned()
    }
}

pub fn planned_label(plan: &ilex_installer_model::plan::Plan) -> String {
    let serial = plan.disk.serial.trim();
    if serial.is_empty() {
        planned_title(plan)
    } else {
        format!("{} {serial}", planned_title(plan))
    }
}

fn contents_of(disk: &Disk) -> Vec<String> {
    let mut lines = Vec::new();
    match (disk.partitions.len(), &disk.partition_table) {
        (0, None) if disk.other_systems.is_empty() => {
            lines.push("Empty: no partition table or file system was found".to_owned());
        }
        (0, _) => {}
        (1, _) => lines.push("1 partition".to_owned()),
        (n, _) => lines.push(format!("{n} partitions")),
    }
    for system in &disk.other_systems {
        lines.push(system.description.clone());
    }
    lines
}

/// What installing beside the systems on `disk` does, in words.
fn beside_of(offer: &Offer) -> Beside {
    let placement = &offer.placement;
    let kept = offer.kept.len();
    let mut lines = vec![format!(
        "ilex is added in unallocated space: /boot ({}) and the root file system ({}).",
        human_bytes(ilex_installer_model::plan::BOOT_BYTES),
        human_bytes(placement.root.size_bytes)
    )];
    lines.push(format!(
        "The EFI system partition ({}, {} free) is shared: ilex adds its boot files to it and the other systems keep theirs.",
        offer.esp.path,
        human_bytes(offer.esp_info.free_bytes.unwrap_or(0))
    ));
    lines.push(format!(
        "{} stay{} exactly as {} {}.",
        if kept == 1 {
            "The one partition on the disk".to_owned()
        } else {
            format!("All {kept} partitions on the disk")
        },
        if kept == 1 { "s" } else { "" },
        if kept == 1 { "it" } else { "they" },
        if kept == 1 { "is" } else { "are" }
    ));
    Beside {
        summary: free_space::describe(offer),
        lines,
    }
}

impl DiskChoice {
    fn new(disk: &Disk) -> Self {
        let serial = disk.serial.trim();
        let mut subtitle = format!("{} · {}", human_bytes(disk.size_bytes), bus_word(&disk.bus));
        if !serial.is_empty() {
            subtitle.push_str(&format!(" · serial {serial}"));
        }
        let unavailable = unavailable_reason(disk);
        Self {
            id: disk.id.clone(),
            title: title_of(disk),
            subtitle,
            contents: contents_of(disk),
            warning: (unavailable.is_none() && disk.removable).then(|| {
                "This is a removable disk. ilex can be installed to it, but it will not stay in this computer unless you leave it plugged in."
                    .to_owned()
            }),
            unavailable,
            beside: free_space::assess(disk).map(|offer| beside_of(&offer)),
        }
    }
}

/// The ways of installing `disk`: erasing it is always possible; beside what
/// is on it only where the model's rule says so (and otherwise the row says
/// why not).
pub fn modes_for(choice: Option<&DiskChoice>) -> Vec<ModeChoice> {
    let beside = match choice.map(|c| &c.beside) {
        Some(Ok(beside)) => (true, beside.summary.clone()),
        Some(Err(why)) => (false, format!("Not available on this disk: {why}.")),
        None => (false, "Choose a disk first.".to_owned()),
    };
    vec![
        ModeChoice {
            mode: Mode::WholeDisk,
            title: "Erase the whole disk",
            subtitle: "Everything on the disk is erased and ilex is installed on all of it."
                .to_owned(),
            available: true,
        },
        ModeChoice {
            mode: Mode::FreeSpace,
            title: "Install beside the other systems",
            subtitle: beside.1,
            available: beside.0,
        },
    ]
}

impl DiskView {
    pub fn from_probe(probe: &Probe) -> Self {
        let all: Vec<DiskChoice> = probe.disks.iter().map(DiskChoice::new).collect();
        let (offered, unavailable): (Vec<_>, Vec<_>) =
            all.into_iter().partition(DiskChoice::is_offered);
        Self {
            choices: offered.into_iter().chain(unavailable).collect(),
        }
    }

    pub fn choice(&self, id: &str) -> Option<&DiskChoice> {
        self.choices.iter().find(|c| c.id == id)
    }

    /// The disks that can be installed to, in the order they are listed.
    pub fn offered(&self) -> impl Iterator<Item = &DiskChoice> {
        self.choices.iter().filter(|c| c.is_offered())
    }
}
