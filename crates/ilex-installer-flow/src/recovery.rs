//! The recovery key page and the security chip's row on the encryption page:
//! their wording and rules, once, for every interface (GTK today, Kirigami
//! later). The helper makes the key and decides whether two groups typed back
//! are right (`ilex_installer_model::recovery`); what is here is what a person
//! reads, how a group is checked before it is sent, and what the page is when
//! the key was already shown to an interface that no longer exists.
//!
//! The key is shown once, while the image downloads. The interface that took
//! it holds it (a [`Secret`], wiped when the page is left); one that was
//! restarted finds the helper saying the key was taken, cannot show it, and
//! asks only for the two groups from the paper.

use crate::account::Check;
use crate::messages::Message;
use crate::secret::Secret;
use ilex_installer_model::recovery::{self, ALPHABET, GROUP_CHARS, GROUPS, GroupProblem};
use ilex_installer_model::state::{TpmOutcome, TpmStatus};

pub const HEADING: &str = "Recovery key";
pub const INTRODUCTION: &str = "If the passphrase is forgotten, or the computer's security chip stops unlocking the disk, this key opens it. Without the passphrase or this key, the data on the disk cannot be recovered.";
pub const WRITE_IT_DOWN: &str = "Write it down. It is shown only now.";
pub const KEEP_IT: &str = "Keep the paper somewhere safe and away from this computer: anyone who has it and the disk can read the disk.";
/// What the page says when the key cannot be shown any more.
pub const GONE_TITLE: &str = "The recovery key was shown before this window restarted";
pub const GONE_DETAIL: &str = "It cannot be shown again. If you wrote it down, type the two groups asked for below to confirm it. If you did not, power the computer off and start the installation again: the disk is erased each time.";
pub const ASK: &str =
    "Type two groups of the key from your paper to confirm that you wrote it down.";
/// The installation goes on behind the page.
pub const GOES_ON: &str = "The installation goes on while you do this.";
pub const CONFIRM_LABEL: &str = "Confirm";
pub const CONFIRMING_LABEL: &str = "Confirming…";
pub const WAITING_KEY: &str = "Getting the recovery key from the installer's helper…";

/// The groups of the key as the page draws them, in order (eight groups of
/// eight letters).
pub fn key_groups(key: &Secret) -> Vec<String> {
    recovery::groups(key.expose())
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// How a typed group stands: empty (nothing shown as wrong), refused with a
/// sentence, or the right shape (whether it is the right group is the
/// helper's to say).
pub fn group_check(typed: &Secret) -> Check {
    match recovery::group_problem(typed.expose()) {
        None => Check::Valid,
        Some(GroupProblem::Empty) => Check::Empty,
        Some(GroupProblem::Short) => Check::Invalid(Message::new(
            "That group is too short",
            format!("A group has {GROUP_CHARS} letters."),
        )),
        Some(GroupProblem::Long) => Check::Invalid(Message::new(
            "That group is too long",
            format!("A group has {GROUP_CHARS} letters."),
        )),
        Some(GroupProblem::NotInKey) => Check::Invalid(Message::new(
            "A recovery key has no such letter",
            format!(
                "It has only these letters: {}.",
                ALPHABET
                    .chars()
                    .map(String::from)
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        )),
    }
}

/// The label of the field for group `number` (1-based).
pub fn group_label(number: usize) -> String {
    format!("Group {number} of {GROUPS}")
}

/// The helper said these are not the groups of its key.
pub fn mismatch() -> Message {
    Message::new(
        "Those are not the groups of the recovery key",
        "Check what you wrote down against the groups asked for, and type them again.",
    )
}

/// Something other than a mismatch went wrong.
pub fn confirm_failed(why: &str) -> Message {
    Message::new(
        "The recovery key could not be confirmed",
        crate::messages::sentence_of(why),
    )
}

/// The key could not be fetched from the helper.
pub fn take_failed(why: &str) -> Message {
    Message::new(
        "The recovery key could not be fetched",
        crate::messages::sentence_of(why),
    )
}

// --- the security chip's row --------------------------------------------------------

pub const CHIP_TITLE: &str = "Unlock automatically with this computer's security chip";
pub const CHIP_ON_LINE: &str = "The computer unlocks the disk by itself when it starts, as long as its start-up software and Secure Boot settings are unchanged. You get a recovery key to write down during the installation; the passphrase works too.";
pub const CHIP_NEEDS_ENCRYPTION: &str = "Needs an encrypted disk.";

/// A reason, as a sentence that follows "Not available:" or "…at start:".
fn capitalised(reason: &str) -> String {
    let mut chars = reason.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}

/// What the row says when the computer cannot do it.
pub fn chip_unavailable_line(reason: &str) -> String {
    format!(
        "Not available: {}. The disk asks for its passphrase every time the computer starts.",
        recovery::tpm_reason(reason)
    )
}

/// What the row shows: whether the person can change it, its position and the
/// line under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChipView {
    /// The switch can be flipped: encrypted, and the computer is eligible.
    pub sensitive: bool,
    pub on: bool,
    pub line: String,
}

// --- the done page ------------------------------------------------------------------

/// The sentence of the done page about unlocking, and whether the disk asks for
/// the passphrase. `None` for an unencrypted installation.
pub fn done_line(outcome: Option<&TpmOutcome>) -> String {
    match outcome {
        Some(outcome) if outcome.status == TpmStatus::EnrolledUnverified => {
            "The disk unlocks automatically: the computer's security chip opens it when the computer starts. The passphrase and the recovery key still open it too, if the chip ever does not.".to_owned()
        }
        Some(outcome) => {
            let opens = if matches!(outcome.reason.as_str(), "no-recovery-key" | "recovery-key-failed") {
                "The passphrase opens the disk, in the keyboard layout you chose."
            } else {
                "The passphrase or the recovery key opens the disk."
            };
            format!(
                "The computer asks for the passphrase at every start: {}. {opens}",
                recovery::tpm_reason(&outcome.reason)
            )
        }
        // The installation ended before the chip's step said anything (an
        // interface that restarted while it ran): the safe thing to say.
        None => "The computer asks for the passphrase at every start, in the keyboard layout you chose.".to_owned(),
    }
}

/// Said once, in the review's row: what automatic unlock will be.
pub fn review_line(will_enroll: bool, reason: &str) -> String {
    if will_enroll {
        "On: the security chip unlocks the disk; the passphrase and a recovery key still work"
            .to_owned()
    } else {
        format!(
            "Off: the disk asks for its passphrase at every start ({})",
            recovery::tpm_reason(reason)
        )
    }
}

/// Used by the page for the capital letter of a reason inside a message.
pub fn sentence_case(text: &str) -> String {
    capitalised(text)
}
