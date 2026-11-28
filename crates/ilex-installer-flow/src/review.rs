//! The review page's content, made from the helper's plan and the choices
//! that went into it: everything chosen, the image that will be installed (its
//! tag, version, digest and download size), the partitions that will be
//! created, and what will be erased (whole-disk mode) or what is kept and what
//! is shared (beside the other systems). Nothing has been written when this is
//! shown: the button below it is the only way on.

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
    /// The two lists above as the page shows them: each system as
    /// `Found on the disk`, each warning as `Note`.
    pub notes: Vec<Row>,
    /// The button's label.
    pub button: String,
    /// The disk as the erase sentence names it (model and serial number).
    pub disk_label: String,
    /// Installing beside the other systems: nothing is erased. Every partition
    /// that is kept exactly as it is, and the EFI system partition that is
    /// shared. Empty in whole-disk mode.
    pub kept: Vec<PartitionRow>,
    pub shared: Option<String>,
    /// `Erase` or `Install`: whether the button and its dialog destroy.
    pub destructive: bool,
}

/// What a partition is for, in words.
fn purpose(partition: &PlannedPartition) -> String {
    match partition.kind.as_str() {
        "esp" => "EFI system partition".to_owned(),
        "xbootldr" => "Boot partition (/boot)".to_owned(),
        kind if kind.starts_with("root") => "Root file system (/)".to_owned(),
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
/// confirming labels. The interface shows them in a dialog and gives the
/// answer to `Flow::confirm_install` or `Flow::cancel_install`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Confirmation {
    pub title: String,
    pub body: String,
    pub cancel: &'static str,
    pub confirm: String,
    /// The answer destroys something (the dialog draws it so): erasing a disk
    /// or removing partitions. Installing beside the other systems does not.
    pub destructive: bool,
}

impl Review {
    /// `choices` are the person's own (already named in words); the plan
    /// supplies the rest.
    pub fn new(plan: &Plan, choices: Vec<Row>) -> Self {
        let disk_label = crate::disks::planned_label(plan);
        let size = human_bytes(plan.disk.size_bytes);
        let mode = match plan.request.mode {
            Mode::WholeDisk => "Erase the whole disk",
            Mode::FreeSpace => "Install beside the other systems",
        };
        let beside = plan.request.mode == Mode::FreeSpace;
        let count = plan.erases.systems.len();
        let mut warnings = plan.warnings.clone();
        // The helper words its own warning about other systems; this page says
        // it once, in the erase sentence and the list under it (or, beside
        // them, in the lists of what is kept and shared).
        warnings.retain(|w| !w.starts_with("everything on "));
        let systems: Vec<String> = plan
            .erases
            .systems
            .iter()
            .map(|s| s.description.clone())
            .collect();
        let notes = systems
            .iter()
            .map(|system| Row {
                title: "Found on the disk",
                value: system.clone(),
            })
            .chain(warnings.iter().map(|warning| Row {
                title: "Note",
                value: warning.clone(),
            }))
            .collect();
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
            erase: if beside {
                format!(
                    "Nothing on {disk_label} is erased: ilex is added in its unallocated space and shares its EFI system partition."
                )
            } else if count == 0 {
                format!("Everything on {disk_label} will be erased.")
            } else {
                format!(
                    "Everything on {disk_label} will be erased, including {count} other system{} found on it.",
                    if count == 1 { "" } else { "s" }
                )
            },
            systems,
            notes,
            warnings,
            button: if beside {
                "Install beside the other systems".to_owned()
            } else {
                format!("Erase {} and install", crate::disks::planned_title(plan))
            },
            disk_label,
            kept: plan
                .kept
                .iter()
                .map(|k| PartitionRow {
                    title: k.description.clone(),
                    detail: format!("{} · {}", k.path, human_bytes(k.size_bytes)),
                })
                .collect(),
            shared: plan.esp.as_ref().map(|esp| {
                format!(
                    "{} ({} free) is shared: ilex adds its boot files under EFI/fedora and its own entry to the firmware's boot menu; the files and entries of the other systems are not changed.",
                    esp.path,
                    human_bytes(esp.free_bytes)
                )
            }),
            destructive: !beside,
        }
    }

    /// The dialog that stands between the button and the helper's `Apply`.
    pub fn confirmation(&self) -> Confirmation {
        if !self.destructive {
            // Beside other systems: it says exactly what is created and what is
            // kept, and that nothing is erased.
            let created = self
                .partitions
                .iter()
                .map(|p| format!("{} ({})", p.title, p.detail))
                .collect::<Vec<_>>()
                .join("; ");
            let kept = self
                .kept
                .iter()
                .map(|p| format!("{} ({})", p.title, p.detail))
                .collect::<Vec<_>>()
                .join("; ");
            return Confirmation {
                title: format!(
                    "Install ilex beside the other systems on {}?",
                    self.disk_label
                ),
                body: format!(
                    "ilex creates: {created}. It adds its boot files to the shared EFI system partition. Kept exactly as they are: {kept}. Nothing is erased. The installation then begins and takes a while: it downloads about {}.",
                    self.image
                        .iter()
                        .find(|r| r.title == "Download")
                        .map_or_else(String::new, |r| r.value.clone())
                ),
                cancel: "Cancel",
                confirm: "Install beside".to_owned(),
                destructive: false,
            };
        }
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
            destructive: true,
        }
    }
}
