//! The daily user page's rules and wording: whether the installed computer is
//! set up for a daily user, and which. The installation creates no account for
//! it and asks for no password: `sudo ilexctl home setup`, run as the
//! maintenance administrator when the computer first starts, creates the
//! encrypted home and asks for its password then. What the installer does is
//! write the choice into `/etc/ilex/setup.json` (the record the Anaconda
//! installer wrote) and say what to run at the login prompt.
//!
//! The rules are the Anaconda path's, copied (the helper applies them again
//! when it plans):
//!
//! * the daily home is encrypted, so a daily user needs an encrypted disk
//!   (Anaconda's `validate_storage`);
//! * the daily user's name may not be the maintenance administrator's
//!   (Anaconda's `validate_users`, which reserved a profile's name; here a
//!   custom user's name is known too, so it is checked as well);
//! * a custom name and capacity are those `ilexctl home setup --name` and
//!   `--disk-size` accept.

use crate::account::Check;
use crate::messages::Message;
use ilex_installer_model::plan::{
    DAILY_SIZE_DEFAULT_GIB, DAILY_SIZE_MAX_GIB, DAILY_SIZE_MIN_GIB, DailyUser, PlanError,
    is_daily_name, is_reserved_user,
};

pub const TITLE: &str = "Daily user";
pub const HEADING: &str = "Daily user";
pub const INTRODUCTION: &str = "Your own account for every day is created when the computer starts for the first time: it gets its own encrypted home, protected by a password you choose then. Here you choose how it is set up. You can also skip this and create an account later.";
pub const GROUP: &str = "Set up";
pub const NAME_LABEL: &str = "User name";
pub const SIZE_LABEL: &str = "Capacity in GiB (optional)";
pub const SIZE_HELP: &str = "How big the home can grow. Without a number it is 20 GiB.";
pub const NEEDS_ENCRYPTION: &str =
    "A daily home needs an encrypted disk. Go back and turn encryption on to set one up.";
/// The end of the done page's sentence when a daily user was chosen.
pub const DONE_SETUP: &str = "then run sudo ilexctl home setup in a terminal: it creates your daily account and asks for its password.";
pub const AFTER: &str = "Nothing is created now. After the first start, sign in as the administrator and run sudo ilexctl home setup: it asks for the daily password.";

/// What the person chose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Daily {
    #[default]
    None,
    /// A public profile of the image (its id).
    Profile(String),
    Custom,
}

const NONE_ID: &str = "none";
const CUSTOM_ID: &str = "custom";
const PROFILE_PREFIX: &str = "profile:";

impl Daily {
    /// The id a page's row has, and the argument of the `daily` event:
    /// `none`, `custom` or `profile:<id>` (a profile id is `[a-z][a-z0-9-]*`,
    /// so none of the three can be another's).
    pub fn id(&self) -> String {
        match self {
            Self::None => NONE_ID.to_owned(),
            Self::Custom => CUSTOM_ID.to_owned(),
            Self::Profile(id) => format!("{PROFILE_PREFIX}{id}"),
        }
    }

    pub fn parse(id: &str) -> Option<Self> {
        match id {
            NONE_ID => Some(Self::None),
            CUSTOM_ID => Some(Self::Custom),
            _ => id
                .strip_prefix(PROFILE_PREFIX)
                .filter(|p| !p.is_empty())
                .map(|p| Self::Profile(p.to_owned())),
        }
    }

    pub fn is_none(&self) -> bool {
        *self == Self::None
    }
}

/// The page's two fields, for a custom user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DailyChecks {
    pub name: Check,
    pub size: Check,
}

impl DailyChecks {
    pub fn all_valid(&self) -> bool {
        self.name.is_valid() && self.size.is_valid()
    }
}

/// `administrator` is the maintenance account's name: the daily user may not
/// have it.
pub fn name(value: &str, administrator: &str) -> Check {
    if value.is_empty() {
        return Check::Empty;
    }
    if value.len() > 31 {
        return Check::Invalid(Message::new(
            "The name is too long",
            "Use at most 31 characters.",
        ));
    }
    if !is_daily_name(value) {
        let detail = match value.chars().next() {
            Some(first) if !first.is_ascii_lowercase() => "Start with a lower-case letter.",
            _ => "Use lower-case letters, digits, underscores and hyphens only.",
        };
        return Check::Invalid(Message::new("That is not a valid user name", detail));
    }
    if value == administrator {
        return Check::Invalid(Message::new(
            "That is the administrator's name",
            "The daily user needs a different name from the maintenance administrator.",
        ));
    }
    if is_reserved_user(value) {
        return Check::Invalid(Message::new(
            "That name is reserved",
            "System account names cannot be used.",
        ));
    }
    Check::Valid
}

/// Empty is fine (the default); anything else is a whole number of GiB in the
/// range `ilexctl home setup --disk-size` takes.
pub fn size(text: &str) -> Check {
    let text = text.trim();
    if text.is_empty() {
        return Check::Valid;
    }
    match parse_size(text) {
        Some(_) => Check::Valid,
        None => Check::Invalid(Message::new(
            "That is not a capacity",
            format!(
                "Use a whole number of GiB from {DAILY_SIZE_MIN_GIB} to {DAILY_SIZE_MAX_GIB}, or leave it empty for {DAILY_SIZE_DEFAULT_GIB}."
            ),
        )),
    }
}

fn parse_size(text: &str) -> Option<u64> {
    text.trim()
        .parse::<u64>()
        .ok()
        .filter(|g| (DAILY_SIZE_MIN_GIB..=DAILY_SIZE_MAX_GIB).contains(g))
}

/// The choice as the plan request carries it.
pub fn user(daily: &Daily, name: &str, size: &str) -> Option<DailyUser> {
    match daily {
        Daily::None => None,
        Daily::Profile(id) => Some(DailyUser::Profile { id: id.clone() }),
        Daily::Custom => Some(DailyUser::Custom {
            name: name.to_owned(),
            disk_size_gib: parse_size(size),
        }),
    }
}

/// The helper's refusal of the daily user, in words that name it (the generic
/// sentence for a reserved name is about the administrator).
pub fn refusal(error: &PlanError) -> Message {
    Message::new("The daily user cannot be set up", error.message.as_str())
}

/// How the choice reads on the review page.
pub fn review_line(user: &DailyUser, display_name: Option<&str>) -> String {
    match user {
        DailyUser::Profile { id } => match display_name {
            Some(display) => format!("{display} ({id}), set up at the first start"),
            None => format!("{id}, set up at the first start"),
        },
        DailyUser::Custom {
            name,
            disk_size_gib,
        } => format!(
            "{name} ({} GiB), set up at the first start",
            disk_size_gib.unwrap_or(DAILY_SIZE_DEFAULT_GIB)
        ),
    }
}
