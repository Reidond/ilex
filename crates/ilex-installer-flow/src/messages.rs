//! What the person reads when something is refused or unavailable. The model
//! and the helper speak in codes and developer sentences; every interface
//! shows these instead, so the wording is one and the same for GTK and Kirigami
//! and changes in one place. All strings are English in the first version.

use crate::flow::human_bytes;
use ilex_installer_model::plan::{ErrorCode, MIN_DISK_BYTES, PlanError};
use serde::Serialize;
use std::fmt;

/// The label of every page's Continue button, and of the one on the page that
/// is asking the helper (the person cannot press it then).
pub const CONTINUE_LABEL: &str = "Continue";
pub const CHECKING_LABEL: &str = "Checking…";
/// What the closing page says while the helper checks the choices.
pub const CHECKING_TITLE: &str = "Checking your choices";
pub const CHECKING_DETAIL: &str = "The installer is asking its helper to check them against this computer and the image registry.";
/// A short line under the layout list while the compositor takes the layout.
pub const SWITCHING_LAYOUT: &str = "Switching the keyboard layout…";
/// What the helper is doing between the erase being confirmed and the first
/// stage: it checks the image's signature, looks at the disk once more and
/// claims it, and writes nothing.
pub const STARTING_TITLE: &str = "Starting the installation";
pub const STARTING_DETAIL: &str = "The installer's helper is checking the disk and the image once more before it writes anything.";

/// A refusal or a problem: a short title that says what happened, and what
/// the person can do about it (or the model's own sentence naming the cause).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Message {
    pub title: String,
    /// May be empty.
    pub detail: String,
}

impl Message {
    pub fn new(title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            detail: detail.into(),
        }
    }

    /// The title alone.
    pub fn title(title: impl Into<String>) -> Self {
        Self::new(title, "")
    }

    /// Title and detail as the sentence a banner or a note shows: `Title.
    /// Detail` (the title alone when there is no detail).
    pub fn sentence(&self) -> String {
        if self.detail.is_empty() {
            self.title.clone()
        } else {
            format!("{}. {}", self.title, self.detail)
        }
    }

    /// Title and detail as one line, for logs and serial markers.
    pub fn one_line(&self) -> String {
        if self.detail.is_empty() {
            self.title.clone()
        } else {
            format!("{}: {}", self.title, self.detail)
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.one_line())
    }
}

/// Why a call to the helper did not give an answer.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientError {
    /// There is no helper on the bus (not started, crashed, or no bus).
    HelperUnavailable(String),
    /// The bus policy refused this user.
    AccessDenied(String),
    /// The helper refused the request: its `PlanError` document.
    Refused(PlanError),
    /// The helper answered, but not in the schema this interface speaks.
    BadAnswer(String),
    /// A method the helper does not have yet (a later change adds it).
    NotYet(&'static str),
    Other(String),
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HelperUnavailable(why) => {
                write!(f, "the installer's helper is not available: {why}")
            }
            Self::AccessDenied(why) => write!(f, "the bus refused the installer: {why}"),
            Self::Refused(error) => write!(f, "the helper refused the request: {}", error.message),
            Self::BadAnswer(why) => write!(f, "the helper's answer is not understood: {why}"),
            Self::NotYet(method) => write!(f, "{method} is not available in this version"),
            Self::Other(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ClientError {}

impl ClientError {
    /// What to show.
    pub fn message(&self) -> Message {
        match self {
            Self::HelperUnavailable(why) => Message::new(
                "The installer's helper is not running",
                format!(
                    "Nothing can be installed without it. Restart the computer and try again; \
                     Ctrl+Alt+F2 opens a shell that can look at the log ({why})."
                ),
            ),
            Self::AccessDenied(why) => Message::new(
                "The installer may not use its helper",
                format!("The system refused the request: {why}."),
            ),
            Self::Refused(error) => plan_error(error),
            Self::BadAnswer(why) => Message::new(
                "The helper's answer was not understood",
                format!("The installer and its helper do not match: {why}."),
            ),
            Self::NotYet(method) => Message::new(
                "Not available in this version",
                format!("The installer cannot do this yet ({method})."),
            ),
            Self::Other(why) => Message::new("Something went wrong", why.clone()),
        }
    }
}

/// The wording of each refusal the helper can make. The model's own sentence
/// is kept as the detail where it names the cause (it never quotes a value of
/// the request).
pub fn plan_error(error: &PlanError) -> Message {
    let cause = error.message.as_str();
    match error.code {
        ErrorCode::InvalidRequest | ErrorCode::InvalidField => {
            Message::new("One of the choices is not valid", cause)
        }
        ErrorCode::UnsupportedFirmware => Message::new(
            "This computer started in legacy BIOS mode",
            "ilex installs on computers that start in UEFI mode. Restart this computer in UEFI mode (the firmware settings have the switch) and start the installer again.",
        ),
        ErrorCode::UnknownComposition => {
            Message::new("That configuration is not offered by this installer", cause)
        }
        ErrorCode::UnknownDisk => Message::new("That disk is no longer there", cause),
        ErrorCode::LiveMedium => {
            Message::new("That disk is the one the installer started from", cause)
        }
        ErrorCode::InUse => Message::new("That disk is in use", cause),
        ErrorCode::DiskTooSmall => Message::new("That disk is too small", cause),
        ErrorCode::UnsupportedMode => {
            Message::new("That way of installing is not available yet", cause)
        }
        ErrorCode::FreeSpaceUnavailable => Message::new(
            "ilex cannot be installed beside the other systems on that disk",
            format!("{cause}. Choose another disk, or erase the whole disk."),
        ),
        ErrorCode::TpmNeedsEncryption => Message::new(
            "Automatic unlock needs an encrypted disk",
            "Turn encryption on, or turn automatic unlock off.",
        ),
        ErrorCode::ReservedName => Message::new(
            "That name is reserved",
            "Pick a distinct maintenance name such as ilexmaint: system accounts and daily-profile names cannot be used.",
        ),
        ErrorCode::Registry => Message::new(
            "The image registry cannot be reached",
            "Check the network connection and that the clock is right, then try again.",
        ),
        ErrorCode::ImageNotFound => Message::new(
            "That image is not published",
            "The registry has no image for this configuration yet.",
        ),
        ErrorCode::Repart => Message::new("The disk layout could not be prepared", cause),
        // What Apply adds. No page applies yet; these are the sentences the
        // progress and problem pages start from.
        ErrorCode::UnknownPlan | ErrorCode::PlanExpired => Message::new(
            "The installation plan is no longer valid",
            "The helper keeps a plan for fifteen minutes and forgets it when it restarts. Go back to the review and confirm the choices again.",
        ),
        ErrorCode::DiskChanged => Message::new("The disk is not the one that was checked", cause),
        ErrorCode::NoRecoveryKey => Message::new(
            "There is no recovery key to show",
            "The installation has none (it was not asked for, or it is over).",
        ),
        ErrorCode::RecoveryKeyTaken => Message::new(
            "The recovery key was shown already",
            "It is handed over once and cannot be shown again.",
        ),
        ErrorCode::RecoveryKeyMismatch => crate::recovery::mismatch(),
        ErrorCode::RecoveryKeyConfirmed => Message::new(
            "The recovery key was confirmed already",
            "The installation keeps it; there is nothing to skip.",
        ),
        ErrorCode::MissingSecret => Message::new("A password is missing", cause),
        ErrorCode::InvalidSecret => Message::new("A password cannot be used", cause),
        ErrorCode::InstallationActive => Message::new("An installation is already running", cause),
        ErrorCode::Signature => Message::new(
            "The image's signature could not be verified",
            "Nothing was written to the disk. Check that the clock is right and try again; if it fails again, this installer and the published image do not match.",
        ),
        // What Finish adds: the place a log may be written to.
        ErrorCode::NotRemovable => Message::new("That is not a USB stick or a card", cause),
        ErrorCode::InstallTarget => Message::new(
            "That is the disk ilex was installed on",
            "A log is never written to it. Use a different USB stick.",
        ),
        ErrorCode::NoFileSystem => Message::new(
            "The installer cannot write to that stick",
            "It needs a partition formatted as FAT, exFAT, ext4 or NTFS. Format the stick on another computer, or use another one.",
        ),
        ErrorCode::Internal => Message::new("The installer's helper failed", cause),
    }
}

/// Nothing can be installed to: one sentence per disk says why, then what to
/// do. The detail has one line per disk.
pub fn no_disk(reasons: &[String]) -> Message {
    Message::new(
        "No disk can be installed to",
        format!(
            "{}\n\nConnect a disk of at least {} that nothing is using, then try again.",
            reasons.join("\n"),
            human_bytes(MIN_DISK_BYTES)
        ),
    )
}

/// The disk the person chose is not there any more (or is in use now) when the
/// computer is looked at again: the disk page says so and has them choose.
pub fn disk_gone(title: &str) -> Message {
    Message::new(
        "The disk you chose is no longer available",
        format!(
            "{title} is not there any more, or something is using it now. Nothing was changed. Choose a disk to go on."
        ),
    )
}

/// The compositor did not take the keyboard layout the person chose.
pub fn keyboard_switch_failed(why: &str) -> Message {
    Message::new(
        "The keyboard layout could not be switched",
        format!(
            "The installer's session did not take it ({why}). Try again, or pick another layout: \
             what you type here and, later, the disk passphrase must match the layout you chose."
        ),
    )
}

// --- the installation -----------------------------------------------------------

/// What a stage does, as a clause for a sentence ("while preparing the disk").
pub fn stage_phrase(stage: ilex_installer_model::state::Stage) -> &'static str {
    use ilex_installer_model::state::Stage;
    match stage {
        Stage::Idle => "starting",
        Stage::Partition => "preparing the disk",
        Stage::Encrypt => "encrypting the disk",
        Stage::Mount => "mounting the new system",
        Stage::Image => "downloading and installing ilex",
        Stage::Configure => "configuring the system",
        Stage::Account => "creating the administrator",
        Stage::Recovery => "confirming the recovery key",
        Stage::Tpm => "setting up automatic unlock",
        Stage::Finalise => "finishing",
        Stage::Done => "finishing",
    }
}

/// The helper's reason for a failure, as one bounded sentence a person can
/// read: its first line, no control characters, ended with a full stop.
pub fn sentence_of(reason: &str) -> String {
    let line = reason.lines().next().unwrap_or("").trim();
    let mut text: String = line.chars().filter(|c| !c.is_control()).take(400).collect();
    if line.chars().count() > 400 {
        text.push('…');
    }
    if text.is_empty() {
        return "The helper gave no reason".to_owned();
    }
    let mut chars = text.chars();
    let mut sentence: String = chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_default();
    sentence.push_str(chars.as_str());
    if !sentence.ends_with(['.', '…', '!', '?']) {
        sentence.push('.');
    }
    sentence
}

/// The installation failed in `stage` for `reason`; `disk_changed` says whether
/// the helper had begun to write to the disk (`disk` names it).
pub fn install_failed(
    stage: ilex_installer_model::state::Stage,
    reason: &str,
    disk_changed: bool,
    disk: Option<&str>,
) -> Message {
    // The helper's own reason may already say what became of the disk (a
    // helper that stopped does): then only the way forward is added.
    let says_so =
        reason.contains("disk was already changed") || reason.contains("disk was not changed");
    let consequence = match (disk_changed, disk) {
        _ if says_so && disk_changed => {
            "Try again to install from the start, or save the log and restart.".to_owned()
        }
        _ if says_so => "Try again, or save the log and restart.".to_owned(),
        (true, Some(disk)) => format!(
            "{disk} has been changed: what was on it is gone. Try again to install from the start, or save the log and restart."
        ),
        (true, None) => "The disk has been changed: what was on it is gone. Try again to install from the start, or save the log and restart.".to_owned(),
        (false, _) => "Nothing was written to the disk.".to_owned(),
    };
    Message::new(
        format!("The installation failed while {}", stage_phrase(stage)),
        format!("{} {consequence}", sentence_of(reason)),
    )
}

/// The helper has not answered the last few questions about the installation:
/// said above the stage list while it is asked again (a helper that crashed
/// starts again by itself within a few seconds).
pub fn helper_silent(error: &ClientError) -> Message {
    let detail = match error {
        // The title would say it again.
        ClientError::HelperUnavailable(_) => {
            "It is being asked again; a helper that stopped starts again by itself.".to_owned()
        }
        other => {
            let message = other.message();
            format!(
                "{}. It is being asked again.",
                sentence_of(&message.title).trim_end_matches('.')
            )
        }
    };
    Message::new("The installer's helper is not answering", detail)
}

/// The installer stopped hearing from its helper during an installation.
pub fn helper_lost(_error: &ClientError) -> Message {
    Message::new(
        "The installer lost contact with its helper",
        "The installation may still be running, or may have stopped. Try again to ask the helper where it is; if it stays like this, restart the computer and install again.",
    )
}

/// `Apply` was asked and its answer never came, and the helper says no
/// installation began.
pub fn not_started(cause: &ClientError) -> Message {
    Message::new(
        "The installation did not start",
        format!(
            "{} Nothing was written to the disk.",
            sentence_of(&cause.to_string())
        ),
    )
}

/// Why an `Apply` the interface waited for did not begin, when the helper
/// recorded no refusal for it.
pub const NOT_BEGUN_CAUSE: &str =
    "The installer's helper did not begin it: it refused it, or it stopped while it checked.";

/// A restarted interface found `Apply` checking and then found that the helper
/// did not begin: it refused (`why` is the refusal it recorded for that plan), or
/// it stopped while it checked. The interface has no choices to go back to: it
/// starts again from the welcome page.
pub fn not_begun(why: Option<&PlanError>) -> Message {
    let cause = match why {
        Some(error) => sentence_of(&ClientError::Refused(error.clone()).to_string()),
        None => NOT_BEGUN_CAUSE.to_owned(),
    };
    Message::new(
        "The installation did not start",
        format!(
            "{cause} Nothing was written to the disk. The installer started again, so its choices are gone: go through the pages again."
        ),
    )
}

/// What the closing page says once Restart or Power off was accepted.
pub const RESTARTING: &str = "Restarting…";
pub const POWERING_OFF: &str = "Powering off…";

/// A Restart or Power off the helper did not carry out.
pub fn finish_failed(error: &ClientError) -> Message {
    let message = error.message();
    Message::new(
        "The computer did not restart",
        format!(
            "{} Hold the power button to turn it off, or press Ctrl+Alt+F2 for a shell.",
            if message.detail.is_empty() {
                message.title
            } else {
                format!("{}. {}", message.title, message.detail)
            }
        ),
    )
}

/// The log was saved to a stick.
pub fn log_saved(saved: &ilex_installer_model::finish::Saved) -> Message {
    Message::new(
        "The log was saved",
        format!(
            "{} was written to {}. It is safe to unplug the stick.",
            saved.file, saved.label
        ),
    )
}

/// The log could not be saved.
pub fn log_failed(error: &ClientError) -> Message {
    let message = error.message();
    Message::new("The log was not saved", {
        if message.detail.is_empty() {
            message.title
        } else {
            format!("{}. {}", message.title, message.detail)
        }
    })
}
