//! The review page's content, made from the helper's plan and the choices
//! that went into it: everything chosen, the image that will be installed (its
//! tag, version, digest and download size), the partitions that will be
//! created, and what will be erased. Nothing has been written when this is
//! shown: the destructive button below it is the only way on.

use crate::flow::human_bytes;
use ilex_installer_model::plan::{Mode, Plan, PlannedPartition};

/// A labelled value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub title: &'static str,
    pub value: String,
}

/// A partition the plan will create.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartitionRow {
    /// What it is for (`EFI system partition`, `/boot`, `Root file system`).
    pub title: String,
    /// Size and file system (`1 GiB · ext4`).
    pub detail: String,
}

/// What the review page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    /// Edition, configuration, language, keyboard layout and administrator.
    pub choices: Vec<Row>,
    /// The disk and the way of installing.
    pub disk: Vec<Row>,
    pub partitions: Vec<PartitionRow>,
    /// The image: tag it tracks, version, digest, download size.
    pub image: Vec<Row>,
    /// `Everything on <model> <serial> will be erased`.
    pub erase: String,
    /// Every other system the probe found on the disk.
    pub systems: Vec<String>,
    /// The plan's own warnings (a removable disk, a list that may be short).
    pub warnings: Vec<String>,
    /// The destructive button's label.
    pub button: String,
    /// The disk as the erase sentence names it (model and serial number).
    pub disk_label: String,
}

/// What a partition is for, in words.
fn purpose(partition: &PlannedPartition) -> String {
    match partition.kind.as_str() {
        "esp" => "EFI system partition".to_owned(),
        "xbootldr" => "Boot partition (/boot)".to_owned(),
        "root" => "Root file system (/)".to_owned(),
        other => format!("{other} partition"),
    }
}

fn filesystem(partition: &PlannedPartition) -> String {
    if partition.encrypted {
        format!("{}, encrypted", partition.filesystem)
    } else {
        partition.filesystem.clone()
    }
}

/// The confirmation's wording: a title and a body, with the cancel and
/// destructive labels. The interface shows them in a dialog and gives the
/// answer to `Flow::confirm_install` or `Flow::cancel_install`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    pub title: String,
    pub body: String,
    pub cancel: &'static str,
    pub confirm: String,
}

impl Review {
    /// `choices` are the person's own (already named in words); the plan
    /// supplies the rest.
    pub fn new(plan: &Plan, choices: Vec<Row>) -> Self {
        let disk_label = crate::disks::planned_label(plan);
        let size = human_bytes(plan.disk.size_bytes);
        let mode = match plan.request.mode {
            Mode::WholeDisk => "Erase the whole disk",
            Mode::FreeSpace => "Install beside what is on the disk",
        };
        let count = plan.erases.systems.len();
        let mut warnings = plan.warnings.clone();
        // The helper words its own warning about other systems; this page says
        // it once, in the erase sentence and the list under it.
        warnings.retain(|w| !w.starts_with("everything on "));
        Self {
            choices,
            disk: vec![
                Row {
                    title: "Disk",
                    value: format!("{disk_label} ({size})"),
                },
                Row {
                    title: "Way of installing",
                    value: mode.to_owned(),
                },
            ],
            partitions: plan
                .partitions
                .iter()
                .map(|p| PartitionRow {
                    title: purpose(p),
                    detail: format!("{} · {}", human_bytes(p.size_bytes), filesystem(p)),
                })
                .collect(),
            image: vec![
                Row {
                    title: "Image",
                    value: plan.image.target.clone(),
                },
                Row {
                    title: "Version",
                    value: plan
                        .image
                        .version
                        .clone()
                        .unwrap_or_else(|| "not reported".to_owned()),
                },
                Row {
                    title: "Digest",
                    value: plan.image.digest.clone(),
                },
                Row {
                    title: "Download",
                    value: human_bytes(plan.image.download_bytes),
                },
            ],
            erase: if count == 0 {
                format!("Everything on {disk_label} will be erased.")
            } else {
                format!(
                    "Everything on {disk_label} will be erased, including {count} other system{} found on it.",
                    if count == 1 { "" } else { "s" }
                )
            },
            systems: plan
                .erases
                .systems
                .iter()
                .map(|s| s.description.clone())
                .collect(),
            warnings,
            button: format!("Erase {} and install", crate::disks::planned_title(plan)),
            disk_label,
        }
    }

    /// The dialog that stands between the button and the helper's `Apply`.
    pub fn confirmation(&self) -> Confirmation {
        Confirmation {
            title: format!("Erase {}?", self.disk_label),
            body: format!(
                "{} This cannot be undone. The installation then begins and takes a while: it downloads about {}.",
                self.erase,
                self.image
                    .iter()
                    .find(|r| r.title == "Download")
                    .map_or_else(String::new, |r| r.value.clone())
            ),
            cancel: "Cancel",
            confirm: "Erase and install".to_owned(),
        }
    }
}
