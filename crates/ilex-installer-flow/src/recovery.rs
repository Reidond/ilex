//! The recovery key page and the security chip's row on the encryption page:
//! their wording and rules, once, for every interface (GTK today, Kirigami
//! later). The helper makes the key and decides whether the key typed back is
//! right (`ilex_installer_model::recovery`: all eight groups, then the key's own
//! keyslot, as phase 0's local completion does); what is here is what a person
//! reads, how the typed key is checked for its shape before it is sent, and what
//! the page is when the key was already shown to an interface that no longer
//! exists or cannot be fetched.
//!
//! The key is shown once, while the image downloads. The interface that took
//! it holds it (a [`Secret`], wiped when the page is left); one that was
//! restarted finds the helper saying the key was taken, cannot show it, and
//! asks only for the whole key from the paper. A person whose key cannot be
//! shown (or whose volume will not take it) can go on without one: the disk then
//! opens with its passphrase only, and says how to get a key later.

use crate::account::Check;
use crate::messages::Message;
use crate::secret::Secret;
use ilex_installer_model::recovery::{self, ALPHABET, GROUP_CHARS, GROUPS, KeyProblem};
use ilex_installer_model::state::{TpmOutcome, TpmStatus};

pub const HEADING: &str = "Recovery key";
pub const INTRODUCTION: &str = "If the passphrase is forgotten, or the computer's security chip stops unlocking the disk, this key opens it. Without the passphrase or this key, the data on the disk cannot be recovered.";
pub const WRITE_IT_DOWN: &str = "Write it down. It is shown only now.";
pub const KEEP_IT: &str = "Keep the paper somewhere safe and away from this computer: anyone who has it and the disk can read the disk.";
/// What the page says when the key cannot be shown any more.
pub const GONE_TITLE: &str = "The recovery key was shown before this window restarted";
pub const GONE_DETAIL: &str = "It cannot be shown again. If you wrote it down, type the whole key from your paper below to confirm it. If you did not, you can continue without a recovery key, or power the computer off and start the installation again: the disk is erased each time.";
pub const ASK: &str =
    "Type the whole recovery key from your paper to confirm that you wrote it down.";
/// The field's title and what it says before anything is typed.
pub const KEY_LABEL: &str = "Recovery key";
pub const KEY_HINT: &str =
    "Eight groups of eight letters. Hyphens, spaces and capitals do not matter.";
/// The installation goes on behind the page.
pub const GOES_ON: &str = "The installation goes on while you do this.";
pub const CONFIRM_LABEL: &str = "Confirm";
pub const CONFIRMING_LABEL: &str = "Confirming…";
pub const WAITING_KEY: &str = "Getting the recovery key from the installer's helper…";

/// Going on without a key: offered only when the key cannot be shown or the
/// volume refused the confirmation; it says what the disk is then.
pub const SKIP_TITLE: &str = "Continue without a recovery key";
pub const SKIP_DETAIL: &str = "The disk will then unlock with its passphrase only, and the computer's security chip will not unlock it by itself. If the passphrase is forgotten, the data cannot be recovered. To get a recovery key later, sign in and run: sudo ilex-enroll complete --root / --reissue-recovery";
pub const SKIP_LABEL: &str = "Continue without a recovery key";
pub const SKIPPING_LABEL: &str = "Continuing…";

/// The groups of the key as the page draws them, in order (eight groups of
/// eight letters).
pub fn key_groups(key: &Secret) -> Vec<String> {
    recovery::groups(key.expose())
        .into_iter()
        .map(str::to_owned)
        .collect()
}

/// How the typed key stands: empty or still short (nothing shown as wrong; the
/// hint counts the letters), refused with a sentence, or the shape of a whole key
/// (whether it is the right one is the helper's to say).
pub fn key_check(typed: &Secret) -> Check {
    match recovery::key_problem(typed.expose()) {
        None => Check::Valid,
        Some(KeyProblem::Empty | KeyProblem::Short { .. }) => Check::Empty,
        Some(KeyProblem::Long) => Check::Invalid(Message::new(
            "That is more than a recovery key",
            format!("A key has {GROUPS} groups of {GROUP_CHARS} letters."),
        )),
        Some(KeyProblem::NotInKey { group }) => Check::Invalid(Message::new(
            "A recovery key has no such letter",
            format!(
                "Group {group} has a letter no key has. A key has only these letters: {}.",
                ALPHABET
                    .chars()
                    .map(String::from)
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        )),
    }
}

/// The line under the field: the hint, then how far the typing is.
pub fn key_progress(typed: &Secret) -> String {
    match recovery::key_problem(typed.expose()) {
        Some(KeyProblem::Empty) | None | Some(KeyProblem::Long | KeyProblem::NotInKey { .. }) => {
            KEY_HINT.to_owned()
        }
        Some(KeyProblem::Short { letters }) => format!(
            "{letters} of {} letters: {} of {GROUPS} groups are complete.",
            GROUPS * GROUP_CHARS,
            letters / GROUP_CHARS
        ),
    }
}

/// The helper said that is not the key it holds.
pub fn mismatch() -> Message {
    Message::new(
        "That is not the recovery key",
        "Check what you wrote down against the whole key, group by group, and type it again.",
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

/// Going on without a key could not be carried out.
pub fn skip_failed(why: &str) -> Message {
    Message::new(
        "The installation could not go on without a recovery key",
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
            // A key was made and never confirmed: say how to get one.
            let later = if outcome.reason == "recovery-key-failed" {
                " To get a recovery key later, run: sudo ilex-enroll complete --root / --reissue-recovery"
            } else {
                ""
            };
            format!(
                "The computer asks for the passphrase at every start: {}. {opens}{later}",
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
