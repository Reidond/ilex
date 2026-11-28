//! `Plan(request)`: what a person may ask for, and what the helper answers.
//! The helper checks every field against the probe and the catalogue before it
//! touches anything; nothing here writes to a disk.

use crate::catalogue::Table;
use crate::probe::{Disk, OtherSystem, Probe};
use serde::{Deserialize, Serialize};
use std::fmt;

pub const PLAN_SCHEMA: u32 = 1;
/// How long Apply accepts a plan.
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
    /// The caller can show a recovery key to a person and ask for two of its
    /// groups back (an interface can; the answers file cannot). Needs
    /// encryption. Without it the installation makes no recovery key and does
    /// not enrol the security chip, whatever `tpm` says.
    #[serde(default)]
    pub recovery_key: bool,
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
    /// Parse a request. A malformed request is refused with a message that
    /// never quotes a value of the document: serde's own text repeats the
    /// value of a mistyped field, and a caller may have put anything there.
    pub fn parse(text: &str) -> Result<Self, PlanError> {
        serde_json::from_str(text)
            .map_err(|e| PlanError::new(ErrorCode::InvalidRequest, None, redact_json_error(&e)))
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
                "expected a user name of at most 31 bytes: lower-case letters, digits, _ and -, starting with a letter or _",
            );
        }
        Ok(())
    }
}

/// What serde_json said about a request, without any of its values: the
/// position, and the names of a missing or unknown field (names, not values).
fn redact_json_error(error: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let at = format!("line {}, column {}", error.line(), error.column());
    if error.classify() != Category::Data {
        return format!("the request is not valid JSON ({at})");
    }
    // `missing field `x`` and `unknown field `x`, expected one of ...` carry
    // names; `invalid type: string "..."` and `unknown variant `...`` carry
    // what the caller sent.
    let text = error.to_string();
    for kind in ["missing field", "unknown field"] {
        let name = text
            .strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix(" `"))
            .and_then(|rest| rest.split('`').next())
            .filter(|name| {
                name.len() <= 64 && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            });
        if let Some(name) = name {
            return format!("{kind} `{name}` ({at})");
        }
    }
    format!(
        "a field of the request has the wrong type or an unknown value ({at}); its value is not repeated"
    )
}

/// A UTF-8 locale name as glibc spells it: `en_US.UTF-8`, `C.UTF-8`,
/// `de_DE.UTF-8@euro`. The installed system is UTF-8 only, so the codeset is
/// required. This is a syntax check: the live image carries no list of the
/// locales the installed system can generate (glibc's `SUPPORTED` is not in
/// it), so a well-formed name for a language the system lacks is accepted and
/// falls back to the C locale there.
pub fn is_language(value: &str) -> bool {
    let (rest, modifier) = match value.split_once('@') {
        Some((rest, modifier)) => (rest, Some(modifier)),
        None => (value, None),
    };
    let Some((rest, charset)) = rest.split_once('.') else {
        return false;
    };
    let (name, territory) = match rest.split_once('_') {
        Some((name, territory)) => (name, Some(territory)),
        None => (rest, None),
    };
    value.len() <= 32
        && (name == "C"
            || ((2..=3).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase())))
        && territory.is_none_or(|t| t.len() == 2 && t.bytes().all(|b| b.is_ascii_uppercase()))
        && !(name == "C" && territory.is_some())
        && charset == "UTF-8"
        && modifier.is_none_or(|m| {
            (1..=16).contains(&m.len())
                && m.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

pub fn is_keymap(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphanumeric())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// A zone name of the zoneinfo tree. The `posix/` and `right/` trees repeat
/// every zone (the second with leap seconds) and `posixrules` is a link kept
/// for old programs: none is a zone a person chooses.
pub fn is_time_zone(value: &str) -> bool {
    let parts: Vec<&str> = value.split('/').collect();
    value.len() <= 64
        && (1..=3).contains(&parts.len())
        && !["posix", "right"].contains(&parts[0])
        && value != "posixrules"
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

/// A Unix account name: at most 31 bytes, as systemd and `build/compose.py`
/// limit them.
pub fn is_user_name(value: &str) -> bool {
    let mut bytes = value.bytes();
    value.len() <= 31
        && bytes
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

/// Accounts and groups the installer must never create: the ones Fedora and
/// the installed ilex image define. The helper adds every name the live image
/// itself defines (its `/etc/passwd`, `/etc/group` and `sysusers.d`), which
/// covers most of them; these are the ones the installed system has and the
/// live image may not, and the ones every system needs.
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
    "wheel",
    "users",
    "sshd",
    "dbus",
    "tss",
    "chrony",
    "polkitd",
    "gdm",
    "avahi",
    "colord",
    "geoclue",
    "pipewire",
    "rtkit",
    "libvirt",
    "qemu",
    "kvm",
    "input",
    "video",
    "render",
    "audio",
    "disk",
    "tty",
    "utmp",
    "ilex-installer",
    "ilex-homed",
    "gamemode",
];

/// Is `name` an account or group the installed system defines? `systemd-`
/// names belong to systemd's own accounts.
pub fn is_reserved_user(name: &str) -> bool {
    RESERVED_USERS.contains(&name) || name.starts_with("systemd-")
}

/// What `SetSecret` carries. The value crosses as a sealed memfd, never as a
/// message string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SecretKind {
    /// The disk's passphrase: the key of the LUKS2 volume's one slot, typed
    /// again at every start of the computer.
    DiskPassphrase,
    /// The maintenance administrator's password.
    AdminPassword,
}

impl SecretKind {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "disk-passphrase" => Some(Self::DiskPassphrase),
            "admin-password" => Some(Self::AdminPassword),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::DiskPassphrase => "disk-passphrase",
            Self::AdminPassword => "admin-password",
        }
    }
}

/// The most a secret may hold (a sealed memfd of this size or less).
pub const SECRET_MAX_BYTES: usize = 4096;

/// The longest disk passphrase, in bytes: what `cryptsetup` itself reads at a
/// prompt, and so what the start of the installed computer can be given.
pub const PASSPHRASE_MAX_BYTES: usize = 512;

/// Why a text cannot be a disk passphrase, or `None` when it can. The
/// passphrase is typed at a console prompt at every start of the computer, so
/// whatever the prompt cannot take (a line break ends it, a control character
/// is not typed) is refused here, once, for the interface, the helper and the
/// answers file alike.
pub fn passphrase_problem(text: &str) -> Option<&'static str> {
    if text.is_empty() {
        Some("the passphrase is empty")
    } else if text.len() > PASSPHRASE_MAX_BYTES {
        Some("the passphrase is longer than 512 bytes")
    } else if text.chars().any(char::is_control) {
        Some("the passphrase has a control character (a line break, a tab or a NUL)")
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    /// Not JSON, or not this schema.
    InvalidRequest,
    InvalidField,
    /// The computer started in BIOS (legacy) mode; the installer is UEFI only.
    UnsupportedFirmware,
    UnknownComposition,
    UnknownDisk,
    /// The disk is the one this system booted from.
    LiveMedium,
    /// The running system is using the disk: a mounted partition, swap, or a
    /// device stacked on it.
    InUse,
    DiskTooSmall,
    UnsupportedMode,
    TpmNeedsEncryption,
    ReservedName,
    /// The registry could not be reached or answered with something unusable.
    Registry,
    ImageNotFound,
    /// `systemd-repart` refused the layout.
    Repart,
    /// Apply names a plan the helper does not know (it forgets its plans when
    /// it restarts, and keeps the latest eight).
    UnknownPlan,
    /// The plan is older than [`PLAN_VALID_SECONDS`].
    PlanExpired,
    /// The disk is not the one the plan describes: another device answers to
    /// its id or the partitions on it changed since the plan was made.
    DiskChanged,
    /// `TakeRecoveryKey`, `AcknowledgeRecoveryKey` or
    /// `ContinueWithoutRecoveryKey` for a plan whose installation has no
    /// recovery key (not asked for, not made yet, or already over).
    NoRecoveryKey,
    /// `TakeRecoveryKey` after the key was taken: it is handed over once.
    RecoveryKeyTaken,
    /// `AcknowledgeRecoveryKey` with a key that is not the installation's (any
    /// of its eight groups), or before the key was taken.
    RecoveryKeyMismatch,
    /// `ContinueWithoutRecoveryKey` after the key was confirmed: the key stays.
    RecoveryKeyConfirmed,
    /// A secret the plan needs has not been supplied (`SetSecret`).
    MissingSecret,
    /// The secret's file descriptor, kind or content is not acceptable.
    InvalidSecret,
    /// An installation is running; probing, planning and applying wait for it.
    InstallationActive,
    /// The image's signature does not verify against the shipped key and
    /// policy, or there is none: nothing was written.
    Signature,
    /// `Finish(save-log:…)` names a disk that is not a removable volume.
    NotRemovable,
    /// `Finish(save-log:…)` names the disk the installation wrote to.
    InstallTarget,
    /// `Finish(save-log:…)` names a stick with no file system the installer
    /// can write a file to.
    NoFileSystem,
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
    if !probe.firmware.uefi {
        return Err(PlanError::new(
            ErrorCode::UnsupportedFirmware,
            None,
            "this computer started in BIOS (legacy) mode; ilex installs on UEFI computers only. Restart it in UEFI mode",
        ));
    }
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
    if !disk.in_use.is_empty() {
        return Err(PlanError::new(
            ErrorCode::InUse,
            Some("disk"),
            format!(
                "{} is in use by the running system: {}",
                disk.id,
                disk.in_use.join("; ")
            ),
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
    if request.recovery_key && !request.encrypt {
        return Err(PlanError::new(
            ErrorCode::InvalidField,
            Some("recovery_key"),
            "a recovery key needs an encrypted disk",
        ));
    }
    if request.tpm == Some(TpmMode::Auto) && !request.encrypt {
        return Err(PlanError::new(
            ErrorCode::TpmNeedsEncryption,
            Some("tpm"),
            "automatic unlock needs an encrypted disk",
        ));
    }
    if is_reserved_user(&request.administrator)
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
    /// A recovery key will be made and shown (the request asked, encrypted).
    #[serde(default)]
    pub recovery_key: bool,
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
