//! `Finish(action)`: what ends an installer session (restart, power off, save
//! the log to a stick), and the one rule that says which disk a log may be
//! written to. The helper applies it to a fresh probe before it mounts
//! anything, and an interface applies the same rule to list the sticks it
//! offers, so the two cannot disagree.

use crate::plan::{ErrorCode, PlanError};
use crate::probe::{Disk, Partition, Probe};

/// The prefix of the action that writes the log: `save-log:<device id>`.
pub const SAVE_LOG_PREFIX: &str = "save-log:";

/// The file systems the helper will write a log into, by what `lsblk` calls
/// them. The ones a person's stick is formatted with.
pub const LOG_FILESYSTEMS: [&str; 6] = ["vfat", "exfat", "ext4", "ext3", "ext2", "ntfs"];

/// A partition smaller than this is not a place for a log.
pub const LOG_MIN_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishAction {
    /// Restart the computer.
    Restart,
    /// Power it off.
    PowerOff,
    /// Write a tarball of the installation's logs to this disk (its stable id
    /// from the probe).
    SaveLog { device: String },
}

impl FinishAction {
    /// Parse the action text. A malformed one is refused with a message that
    /// never quotes it.
    pub fn parse(text: &str) -> Result<Self, PlanError> {
        let bad = || {
            PlanError::new(
                ErrorCode::InvalidField,
                Some("action"),
                "the action is restart, poweroff or save-log:<device id>",
            )
        };
        match text {
            "restart" => Ok(Self::Restart),
            "poweroff" => Ok(Self::PowerOff),
            _ => {
                let device = text.strip_prefix(SAVE_LOG_PREFIX).ok_or_else(bad)?;
                // A device id is one plain name: what udev makes of the
                // hardware's own identity.
                if device.is_empty()
                    || device.len() > 255
                    || device.starts_with('.')
                    || device.contains('/')
                    || device.chars().any(char::is_control)
                {
                    return Err(bad());
                }
                Ok(Self::SaveLog {
                    device: device.to_owned(),
                })
            }
        }
    }

    /// The text `parse` reads.
    pub fn text(&self) -> String {
        match self {
            Self::Restart => "restart".into(),
            Self::PowerOff => "poweroff".into(),
            Self::SaveLog { device } => format!("{SAVE_LOG_PREFIX}{device}"),
        }
    }

    /// The action's word for markers and logs (never the device).
    pub fn word(&self) -> &'static str {
        match self {
            Self::Restart => "restart",
            Self::PowerOff => "poweroff",
            Self::SaveLog { .. } => "save-log",
        }
    }
}

/// What the helper answers when a log was saved: the file it wrote and where.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    /// The file's name on the stick's file system.
    pub file: String,
    /// The disk it was written to (its id) and how a person names it.
    pub device: String,
    pub label: String,
    /// The file's size.
    pub bytes: u64,
}

/// The partition of `disk` a log is written into: the first one, in table
/// order, whose file system is one the helper writes and that is big enough.
pub fn log_partition(disk: &Disk) -> Option<&Partition> {
    disk.partitions
        .iter()
        .find(|p| LOG_FILESYSTEMS.contains(&p.filesystem.as_str()) && p.size_bytes >= LOG_MIN_BYTES)
}

/// May a log be written to the disk `id`? It must be in the probe, a removable
/// volume (a stick or a card, never a fixed disk), not the medium the
/// installer started from, not the disk the installation wrote to (`target`,
/// its id), not in use by the running system, and have a partition with a file
/// system the helper writes. The refusal says which of these it is.
pub fn check_log_device<'a>(
    probe: &'a Probe,
    id: &str,
    target: Option<&str>,
) -> Result<(&'a Disk, &'a Partition), PlanError> {
    let refuse = |code, message: String| Err(PlanError::new(code, Some("device"), message));
    let Some(disk) = probe.disks.iter().find(|d| d.id == id) else {
        return refuse(
            ErrorCode::UnknownDisk,
            "that device is not one the installer found".into(),
        );
    };
    if disk.live_medium {
        return refuse(
            ErrorCode::LiveMedium,
            format!("{} is the medium this installer started from", disk.id),
        );
    }
    if target == Some(disk.id.as_str()) {
        return refuse(
            ErrorCode::InstallTarget,
            format!(
                "{} is the disk the installation was written to; a log is never written there",
                disk.id
            ),
        );
    }
    if !disk.removable {
        return refuse(
            ErrorCode::NotRemovable,
            format!(
                "{} is not a removable disk; the log goes to a USB stick or a card",
                disk.id
            ),
        );
    }
    if !disk.in_use.is_empty() {
        return refuse(
            ErrorCode::InUse,
            format!(
                "{} is in use by the running system: {}",
                disk.id,
                disk.in_use.join("; ")
            ),
        );
    }
    match log_partition(disk) {
        Some(partition) => Ok((disk, partition)),
        None => refuse(
            ErrorCode::NoFileSystem,
            format!(
                "{} has no partition with a file system the installer can write to (FAT, exFAT, ext4 or NTFS, at least 8 MiB)",
                disk.id
            ),
        ),
    }
}
