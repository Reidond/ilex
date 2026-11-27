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
        ErrorCode::MissingSecret => Message::new("A password is missing", cause),
        ErrorCode::InvalidSecret => Message::new("A password cannot be used", cause),
        ErrorCode::InstallationActive => Message::new("An installation is already running", cause),
        ErrorCode::Signature => Message::new(
            "The image's signature could not be verified",
            "Nothing was written to the disk. Check that the clock is right and try again; if it fails again, this installer and the published image do not match.",
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
