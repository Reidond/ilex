//! The encryption page's rules: whether the disk is encrypted (it is, unless
//! turned off), and the passphrase that unlocks it, typed twice. The passphrase
//! is what the person types at every start of the computer, at a console
//! prompt, so what a prompt cannot take is refused here with a sentence (the
//! model states the rule once, for the helper and the answers file too); beyond
//! that there is no policy: a short passphrase gets a hint, not a refusal.
//!
//! The page also names the keyboard layout the passphrase is typed in. The
//! layout chosen on the first page is the session's now, and the installed
//! computer's console is put in the same layout before it asks (the helper gives
//! the initramfs the keymap on the kernel command line), so what is typed here
//! is what is typed there.

use crate::account::Check;
use crate::keyboard::Layout;
use crate::messages::Message;
use crate::secret::Secret;
use ilex_installer_model::plan::{PASSPHRASE_MAX_BYTES, passphrase_problem};

/// Passphrases shorter than this get a hint, not a refusal.
pub const PASSPHRASE_ADVISED_CHARS: usize = 12;

/// The page's two fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptionChecks {
    pub passphrase: Check,
    pub confirmation: Check,
}

impl EncryptionChecks {
    pub fn all_valid(&self) -> bool {
        self.passphrase.is_valid() && self.confirmation.is_valid()
    }
}

pub fn passphrase(value: &Secret) -> Check {
    if value.is_empty() {
        return Check::Empty;
    }
    if value.len() > PASSPHRASE_MAX_BYTES {
        return Check::Invalid(Message::new(
            "The passphrase is too long",
            format!("Use at most {PASSPHRASE_MAX_BYTES} bytes."),
        ));
    }
    if passphrase_problem(value.expose()).is_some() {
        return Check::Invalid(Message::new(
            "The passphrase has a character that cannot be typed at the prompt",
            "Line breaks, tabs and other control characters are not allowed.",
        ));
    }
    Check::Valid
}

/// `confirmation` is judged against the passphrase whatever its own state.
pub fn confirmation(passphrase: &Secret, confirmation: &Secret) -> Check {
    if confirmation.is_empty() {
        return Check::Empty;
    }
    if passphrase.expose() != confirmation.expose() {
        return Check::Invalid(Message::title("The passphrases do not match"));
    }
    Check::Valid
}

/// A hint for a passphrase that is valid but short, or `None`.
pub fn advice(value: &Secret) -> Option<Message> {
    let characters = value.expose().chars().count();
    (characters > 0 && characters < PASSPHRASE_ADVISED_CHARS).then(|| {
        Message::new(
            "A longer passphrase is safer",
            format!("Anyone who has the disk can try passphrases on another computer, as fast as it can: use at least {PASSPHRASE_ADVISED_CHARS} characters."),
        )
    })
}

/// What the page says about the layout: where the passphrase is typed, in
/// words.
pub fn layout_line(layout: Option<&Layout>) -> String {
    match layout {
        Some(layout) => format!(
            "You type it in the {} layout: here, and every time the computer starts.",
            layout.name
        ),
        None => "You type it in the keyboard layout chosen on the first page: here, and every time the computer starts.".to_owned(),
    }
}

/// The paragraph at the top of the page.
pub const INTRODUCTION: &str = "Encryption keeps what is on the disk unreadable to anyone who has the disk or the computer but not the passphrase. The computer asks for the passphrase every time it starts. If it is forgotten, the data cannot be recovered.";

/// The line under the switch while encryption is off.
pub const OFF_LINE: &str = "Without encryption, anyone who has the disk can read everything on it.";
