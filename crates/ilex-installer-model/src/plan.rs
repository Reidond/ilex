//! `Plan(request)`: what a person may ask for, and what the helper answers.
//! The helper checks every field against the probe and the catalogue before it
//! touches anything; nothing here writes to a disk.

use crate::catalogue::Table;
use crate::probe::{Disk, OtherSystem, Probe};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PLAN_SCHEMA: u32 = 1;
/// How long Apply (a later PR) accepts a plan.
pub const PLAN_VALID_SECONDS: u64 = 15 * 60;

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// The whole-disk layout (installer/repart/whole-disk): an ESP, an XBOOTLDR
/// `/boot` and the root filesystem in the rest.
pub const ESP_BYTES: u64 = 512 * MIB;
pub const BOOT_BYTES: u64 = GIB;
/// A stated minimum, to be set from a measured install.
pub const MIN_ROOT_BYTES: u64 = 30 * GIB;
/// The disk must also hold the partition table and alignment.
pub const MIN_DISK_BYTES: u64 = ESP_BYTES + BOOT_BYTES + MIN_ROOT_BYTES + 2 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    /// Erase the disk and install into all of it.
    WholeDisk,
    /// Install into free space beside what is there (a later PR).
    FreeSpace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TpmMode {
    /// Enrol the TPM when the probe says this computer is eligible.
    Auto,
    Off,
}

fn yes() -> bool {
    true
}
fn keyboard() -> String {
    "us".into()
}
fn language() -> String {
    "en_US.UTF-8".into()
}
fn time_zone() -> String {
    "Etc/UTC".into()
}
fn host_name() -> String {
    "ilex".into()
}

/// Every field the installer lets a person choose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanRequest {
    /// A tag of this edition's catalogue.
    pub composition: String,
    /// A disk's stable id from the probe.
    pub disk: String,
    pub mode: Mode,
    /// Encrypt the root filesystem (LUKS2). On unless turned off.
    #[serde(default = "yes")]
    pub encrypt: bool,
    /// Defaults to `auto` with encryption and `off` without.
    #[serde(default)]
    pub tpm: Option<TpmMode>,
    /// A console keymap name.
    #[serde(default = "keyboard")]
    pub keyboard: String,
    #[serde(default = "language")]
    pub language: String,
    #[serde(default = "time_zone")]
    pub time_zone: String,
    #[serde(default = "host_name")]
    pub host_name: String,
    /// The maintenance and recovery administrator's user name.
    pub administrator: String,
}

impl PlanRequest {
    pub fn parse(text: &str) -> Result<Self, PlanError> {
        serde_json::from_str(text)
            .map_err(|e| PlanError::new(ErrorCode::InvalidRequest, None, e.to_string()))
    }

    pub fn tpm_mode(&self) -> TpmMode {
        self.tpm.unwrap_or(if self.encrypt {
            TpmMode::Auto
        } else {
            TpmMode::Off
        })
    }

    /// The request with every default resolved, as the plan records it.
    pub fn normalized(&self) -> Self {
        Self {
            tpm: Some(self.tpm_mode()),
            ..self.clone()
        }
    }

    /// Syntax only: what can be checked without the probe or the system.
    pub fn check_syntax(&self) -> Result<(), PlanError> {
        let bad = |field: &str, what: &str| {
            Err(PlanError::new(
                ErrorCode::InvalidField,
                Some(field),
                format!("{field}: {what}"),
            ))
        };
        if !is_language(&self.language) {
            return bad("language", "expected a locale such as en_US.UTF-8");
        }
        if !is_keymap(&self.keyboard) {
            return bad(
                "keyboard",
                "expected a console keymap name such as us or de-nodeadkeys",
            );
        }
        if !is_time_zone(&self.time_zone) {
            return bad("time_zone", "expected a zone name such as Europe/Berlin");
        }
        if !is_host_name(&self.host_name) {
            return bad(
                "host_name",
                "expected a host name: up to 64 letters, digits, dots and hyphens",
            );
        }
        if !is_user_name(&self.administrator) {
            return bad(
                "administrator",
                "expected a user name: lower-case letters, digits, _ and -, starting with a letter or _",
            );
        }
        Ok(())
    }
}

/// `en_US.UTF-8`, `C.UTF-8`, `de_DE@euro`.
pub fn is_language(value: &str) -> bool {
    let (rest, modifier) = match value.split_once('@') {
        Some((rest, modifier)) => (rest, Some(modifier)),
        None => (value, None),
    };
    let (rest, charset) = match rest.split_once('.') {
        Some((rest, charset)) => (rest, Some(charset)),
        None => (rest, None),
    };
    let (name, territory) = match rest.split_once('_') {
        Some((name, territory)) => (name, Some(territory)),
        None => (rest, None),
    };
    value.len() <= 32
        && (1..=3).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphabetic())
        && territory.is_none_or(|t| t.len() == 2 && t.bytes().all(|b| b.is_ascii_uppercase()))
        && charset.is_none_or(|c| c == "UTF-8")
        && modifier.is_none_or(|m| !m.is_empty() && m.bytes().all(|b| b.is_ascii_alphanumeric()))
}

pub fn is_keymap(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

pub fn is_time_zone(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    value.len() <= 64
        && (1..=3).contains(&parts.len())
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
        })
}

/// systemd's `hostname_is_valid` for a plain host name: dot-separated labels
/// of letters, digits and hyphens, none empty, none starting or ending with a
/// hyphen, at most 64 characters in all.
pub fn is_host_name(value: &str) -> bool {
    value.len() <= 64
        && !value.is_empty()
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

pub fn is_user_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 32
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

/// Accounts the installer must never create: the system's own, and the
/// installer's.
const RESERVED_USERS: &[&str] = &[
    "root",
    "nobody",
    "daemon",
    "bin",
    "sys",
    "adm",
    "sync",
    "shutdown",
    "halt",
    "mail",
    "operator",
    "games",
    "ftp",
    "systemd-homed",
    "ilex-installer",
    "gdm",
    "polkitd",
    "wheel",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// Not JSON, or not this schema.
    InvalidRequest,
    InvalidField,
    UnknownComposition,
    UnknownDisk,
    /// The disk is the one this system booted from.
    LiveMedium,
    DiskTooSmall,
    UnsupportedMode,
    TpmNeedsEncryption,
    ReservedName,
    /// The registry could not be reached or answered with something unusable.
    Registry,
    ImageNotFound,
    /// `systemd-repart` refused the layout.
    Repart,
    Internal,
}

/// A refusal, as the helper returns it on the bus (the error message is this
/// document).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanError {
    pub code: ErrorCode,
    pub field: Option<String>,
    pub message: String,
}

impl PlanError {
    pub fn new(code: ErrorCode, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code,
            field: field.map(str::to_owned),
            message: message.into(),
        }
    }
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PlanError {}

/// The parts of the probe a plan is made from, once the request is valid.
#[derive(Debug)]
pub struct Checked<'a> {
    pub request: PlanRequest,
    pub composition: &'a crate::catalogue::Composition,
    pub disk: &'a Disk,
}

/// Check every field against the probe and the catalogue. The helper adds the
/// checks that need this system's files (zone and keymap names).
pub fn check_request<'a>(
    request: &PlanRequest,
    probe: &'a Probe,
) -> Result<Checked<'a>, PlanError> {
    request.check_syntax()?;
    let table: &Table = &probe.catalogue;
    let Some(composition) = table.composition(&request.composition) else {
        let message = if request.composition.split('.').next() == Some("vm") {
            "vm is never installed on hardware".to_string()
        } else {
            format!(
                "unknown composition '{}'; offered: {}",
                request.composition,
                table.offered()
            )
        };
        return Err(PlanError::new(
            ErrorCode::UnknownComposition,
            Some("composition"),
            message,
        ));
    };
    if request.mode == Mode::FreeSpace {
        return Err(PlanError::new(
            ErrorCode::UnsupportedMode,
            Some("mode"),
            "installing into free space is not available yet; use whole-disk",
        ));
    }
    let Some(disk) = probe.disks.iter().find(|d| d.id == request.disk) else {
        return Err(PlanError::new(
            ErrorCode::UnknownDisk,
            Some("disk"),
            format!(
                "no disk '{}' in the probe; found: {}",
                request.disk,
                probe
                    .disks
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    };
    if disk.live_medium {
        return Err(PlanError::new(
            ErrorCode::LiveMedium,
            Some("disk"),
            format!("{} is the medium this installer started from", disk.id),
        ));
    }
    if disk.size_bytes < MIN_DISK_BYTES {
        return Err(PlanError::new(
            ErrorCode::DiskTooSmall,
            Some("disk"),
            format!(
                "{} holds {} bytes; the installation needs at least {MIN_DISK_BYTES}",
                disk.id, disk.size_bytes
            ),
        ));
    }
    if request.tpm == Some(TpmMode::Auto) && !request.encrypt {
        return Err(PlanError::new(
            ErrorCode::TpmNeedsEncryption,
            Some("tpm"),
            "automatic unlock needs an encrypted disk",
        ));
    }
    if RESERVED_USERS.contains(&request.administrator.as_str())
        || table.profiles.contains_key(&request.administrator)
    {
        return Err(PlanError::new(
            ErrorCode::ReservedName,
            Some("administrator"),
            format!(
                "{} is reserved (system accounts and daily-profile names); pick a distinct maintenance name such as ilexmaint",
                request.administrator
            ),
        ));
    }
    Ok(Checked {
        request: request.normalized(),
        composition,
        disk,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub schema: u32,
    /// 128 random bits, hex. What Apply names.
    pub id: String,
    pub created: u64,
    pub valid_seconds: u64,
    /// Seeds the partition UUIDs, so Apply creates exactly what was planned.
    pub seed: String,
    pub edition: String,
    pub request: PlanRequest,
    pub composition: PlannedComposition,
    pub image: PlannedImage,
    pub disk: PlannedDisk,
    pub partitions: Vec<PlannedPartition>,
    pub erases: Erase,
    pub encryption: PlannedEncryption,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedComposition {
    pub tag: String,
    pub machine: String,
    pub shells: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedImage {
    /// What the installed system tracks: `ghcr.io/reidond/ilex:<tag>`.
    pub target: String,
    /// What is installed: `ghcr.io/reidond/ilex@sha256:...`, the manifest the
    /// registry answered for the tag when the plan was made.
    pub source: String,
    pub digest: String,
    /// The compressed download: the manifest's config and layer sizes.
    pub download_bytes: u64,
    pub version: Option<String>,
    pub revision: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedDisk {
    pub id: String,
    pub path: String,
    pub model: String,
    pub serial: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlannedPartition {
    pub number: u32,
    /// The kernel name it will have.
    pub path: String,
    /// repart's type alias: `esp`, `xbootldr` or `root`.
    pub kind: String,
    pub label: String,
    pub uuid: String,
    pub start_bytes: u64,
    pub size_bytes: u64,
    pub filesystem: String,
    pub encrypted: bool,
}

/// What whole-disk mode destroys.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Erase {
    pub disk: String,
    pub partitions: Vec<String>,
    pub systems: Vec<OtherSystem>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedEncryption {
    pub enabled: bool,
    pub tpm: PlannedTpm,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlannedTpm {
    pub requested: TpmMode,
    /// Enrolment would be attempted: requested, encrypted and the probe's
    /// verdict is eligible.
    pub will_enroll: bool,
    /// The probe's reason code (`pcr7-sha256`, `secure-boot-off`, ...).
    pub reason: String,
}
