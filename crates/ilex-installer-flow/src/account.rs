//! The administrator page's rules: the maintenance and recovery account's name
//! and password. The name is checked with the model's own functions (the
//! helper applies the same ones, plus the names its live image defines, when
//! it plans); the password has no policy in the model, so what is checked here
//! is only what the tools that will consume it cannot take.

use crate::messages::Message;
use crate::secret::Secret;
use ilex_installer_model::plan::{is_reserved_user, is_user_name};
use std::collections::BTreeSet;

/// The longest password, in bytes. `chpasswd` reads `name:password` from one
/// line of standard input; this is far below any limit of it or of LUKS.
pub const PASSWORD_MAX_BYTES: usize = 255;

/// Passwords shorter than this get a hint, not a refusal: the model has no
/// policy to enforce.
pub const PASSWORD_ADVISED_CHARS: usize = 8;

/// How one field stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    /// Nothing typed yet: Continue stays off, but nothing is shown as wrong
    /// (an interface may say "required" after the person tried to continue).
    Empty,
    /// Typed, and refused.
    Invalid(Message),
    Valid,
}

impl Check {
    pub fn is_valid(&self) -> bool {
        matches!(self, Self::Valid)
    }

    /// The refusal to show beside the field, if any.
    pub fn message(&self) -> Option<&Message> {
        match self {
            Self::Invalid(message) => Some(message),
            _ => None,
        }
    }
}

/// The three fields of the page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountChecks {
    pub name: Check,
    pub password: Check,
    pub confirmation: Check,
}

impl AccountChecks {
    pub fn all_valid(&self) -> bool {
        self.name.is_valid() && self.password.is_valid() && self.confirmation.is_valid()
    }
}

/// `profiles` are the public profiles of the catalogue: their names belong to
/// the daily user and may not be the maintenance account's.
pub fn name(value: &str, profiles: &BTreeSet<&str>) -> Check {
    if value.is_empty() {
        return Check::Empty;
    }
    if value.len() > 31 {
        return Check::Invalid(Message::new(
            "The name is too long",
            "Use at most 31 characters.",
        ));
    }
    if !is_user_name(value) {
        let detail = match value.chars().next() {
            Some(first) if !(first.is_ascii_lowercase() || first == '_') => {
                "Start with a lower-case letter or an underscore."
            }
            _ => "Use lower-case letters, digits, underscores and hyphens only.",
        };
        return Check::Invalid(Message::new("That is not a valid user name", detail));
    }
    if is_reserved_user(value) || profiles.contains(value) {
        return Check::Invalid(Message::new(
            "That name is reserved",
            "Pick a distinct maintenance name such as ilexmaint: system accounts and daily-profile names cannot be used.",
        ));
    }
    Check::Valid
}

pub fn password(value: &Secret) -> Check {
    if value.is_empty() {
        return Check::Empty;
    }
    if value.len() > PASSWORD_MAX_BYTES {
        return Check::Invalid(Message::new(
            "The password is too long",
            format!("Use at most {PASSWORD_MAX_BYTES} bytes."),
        ));
    }
    if value.expose().chars().any(char::is_control) {
        return Check::Invalid(Message::new(
            "The password has a character that cannot be used",
            "Line breaks, tabs and other control characters are not allowed.",
        ));
    }
    Check::Valid
}

/// `confirmation` is checked only against a password that is itself usable.
pub fn confirmation(password: &Secret, confirmation: &Secret) -> Check {
    if confirmation.is_empty() {
        return Check::Empty;
    }
    if password.expose() != confirmation.expose() {
        return Check::Invalid(Message::title("The passwords do not match"));
    }
    Check::Valid
}

/// A hint for a password that is valid but short, or `None`.
pub fn password_advice(value: &Secret) -> Option<Message> {
    let characters = value.expose().chars().count();
    (characters > 0 && characters < PASSWORD_ADVISED_CHARS).then(|| {
        Message::new(
            "A longer password is safer",
            format!("This account can administer the computer: use at least {PASSWORD_ADVISED_CHARS} characters."),
        )
    })
}
