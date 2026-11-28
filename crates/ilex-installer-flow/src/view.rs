//! What every page shows, as plain data: [`Flow::view`] turns the flow's state
//! into one [`View`], and an interface draws it. The wording that is not a
//! refusal (headings, paragraphs, labels, group titles, the order in which a
//! banner or a notice takes precedence over another, which button is shown or
//! pressable, which glyph a problem has, when a field is marked invalid, what
//! the fields' values are) is decided here, once, for every toolkit: GTK
//! (`ilex-installer --features gui`) and Kirigami (`--features kde`) apply it
//! and decide nothing. A toolkit still owns how a [`Glyph`] looks, what a
//! "group" or a "banner" is made of, and where a widget goes.
//!
//! The view is serialisable: the KDE edition hands it to QML as JSON, so that
//! no page's logic, wording or visibility rule lives in QML or in the Rust glue
//! that talks to it. The two lists that never change while a page is shown
//! (the languages and the keyboard layouts, several hundred entries) are in the
//! [`Catalogue`] instead of in every view.
//!
//! Nothing here is secret: the passwords are not in the view (only how many
//! times the flow has wiped them, for the interface to clear its fields), and
//! the administrator's name is not private.

use serde::Serialize;

use crate::disks;
use crate::flow::{
    Control, Flow, KeyboardStatus, LogSave, PageId, ProbeState, ProblemKind, ReviewBody,
};
use crate::language::{self, LANGUAGES};
use crate::machine::Severity;
use crate::messages::{self, Message};
use crate::progress::Status;
use crate::{encryption, recovery};

// --- wording ---------------------------------------------------------------------

pub const TRY_AGAIN: &str = "Try again";

pub const WELCOME_TITLE: &str = "Welcome";
pub const WELCOME_HEADING: &str = "Welcome to ilex";
pub const WELCOME_INTRO: &str =
    "This installs ilex on this computer. Nothing is changed until you confirm it on the last page.";
pub const LANGUAGE_LABEL: &str = "Language";
pub const LANGUAGE_HELP: &str =
    "The language of the installed computer. The installer itself is in English.";
pub const KEYBOARD_LABEL: &str = "Keyboard layout";
pub const KEYBOARD_HELP: &str =
    "The layout is used here at once, and for the disk passphrase when the computer starts.";
pub const TRY_LAYOUT_LABEL: &str = "Type here to try the layout";

pub const MACHINE_TITLE: &str = "This computer";
pub const MACHINE_HEADING: &str = "This computer";
pub const MACHINE_INTRO: &str = "Pick the configuration to install. Computers ilex knows get their own; any other x86-64 computer uses the generic one.";
pub const MACHINE_LOADING_TITLE: &str = "Looking at this computer";
pub const MACHINE_LOADING_DETAIL: &str =
    "The installer is reading what this computer is and which disks it has.";
pub const COMPUTER_LABEL: &str = "This computer";
pub const CONFIGURATION: &str = "Configuration";

pub const DISK_TITLE: &str = "Disk";
pub const DISK_HEADING: &str = "Where to install";
pub const DISK_INTRO: &str = "Choose the disk ilex is installed on. Nothing is changed on it until you confirm on the last page.";
pub const DISK_LOADING_TITLE: &str = "Looking at the disks";
pub const DISK_LOADING_DETAIL: &str = "The installer is reading which disks this computer has.";
pub const DISK_GROUP: &str = "Disk";
pub const DISK_AWAY: &str = "Not available";
pub const DISK_AWAY_INTRO: &str = "ilex cannot be installed to these disks.";
pub const DISK_MODES: &str = "How";

pub const ENCRYPTION_TITLE: &str = "Encryption";
pub const ENCRYPTION_HEADING: &str = "Encryption";
pub const ENCRYPT_TITLE: &str = "Encrypt this disk";
pub const ENCRYPT_SUBTITLE: &str =
    "Recommended. The disk asks for its passphrase every time the computer starts.";
pub const PASSPHRASE_LABEL: &str = "Passphrase";
pub const PASSPHRASE_CONFIRMATION_LABEL: &str = "Confirm the passphrase";

pub const RECOVERY_TITLE: &str = "Recovery key";

pub const ACCOUNT_TITLE: &str = "Administrator";
pub const ACCOUNT_HEADING: &str = "Administrator";
pub const ACCOUNT_INTRO: &str = "This account maintains the computer and recovers it when something goes wrong: it can use sudo. You create your own daily account when the computer starts for the first time.";
pub const NAME_LABEL: &str = "User name";
pub const PASSWORD_LABEL: &str = "Password";
pub const CONFIRMATION_LABEL: &str = "Confirm the password";

pub const REVIEW_TITLE: &str = "Review";
pub const REVIEW_HEADING: &str = "Review";
pub const REVIEW_INTRO: &str = "Check what will be installed. Go back to change anything.";
pub const REVIEW_END: &str =
    "Nothing has been written to any disk yet. The button below is the only way on.";
pub const REVIEW_BUTTON: &str = "Erase and install";
pub const GROUP_CHOICES: &str = "Your choices";
pub const GROUP_DISK: &str = "The disk";
pub const GROUP_PARTITIONS: &str = "What is created";
pub const GROUP_PARTITIONS_INTRO: &str = "These partitions are made on the disk.";
pub const GROUP_IMAGE: &str = "The image";
pub const GROUP_NOTES: &str = "Worth knowing";

pub const INSTALLING_TITLE: &str = "Installing";
pub const INSTALLING_INTRO: &str = "The image is downloaded from the registry and written to the disk. This takes a while; leave the computer on.";
pub const SHOW_LOG: &str = "Show the log";

pub const DONE_TITLE: &str = "Done";
pub const RESTART: &str = "Restart";
pub const POWER_OFF: &str = "Power off";

pub const PROBLEM_TITLE: &str = "Problem";
pub const LOG_CAPTION: &str = "The last lines of the log";
pub const SAVE_LOG: &str = "Save the log to a USB stick";

pub const LOG_DIALOG_TITLE: &str = "Save the log";
pub const LOG_LOOKING_TITLE: &str = "Looking for USB sticks";
pub const LOG_SAVING_TITLE: &str = "Writing the log";
pub const LOG_SAVING_DETAIL: &str = "Do not unplug the stick.";
pub const LOG_CHOOSE: &str = "Choose where to save the log";
pub const LOG_CHOOSE_INTRO: &str = "The log is a single file with what the installer did: no passwords. It is written to a stick that is not the one the installer started from.";
pub const LOG_NO_STICK: &str = "No USB stick or card was found. Plug one in and look again.";
pub const LOG_LOOK_AGAIN: &str = "Look again";
pub const LOG_SAVE_HERE: &str = "Save here";
pub const LOG_CLOSE: &str = "Close";

pub const STARTING_PAGE: &str = "Starting the installer";

// --- the data --------------------------------------------------------------------

/// A picture a page uses, by what it means. Each toolkit maps it to its own
/// icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Glyph {
    Error,
    Disk,
    Network,
    Success,
}

/// How loud a banner is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Info,
    Warning,
    Error,
}

/// A line across the top of a page: a problem the person should see on whatever
/// page they are on, with a button when asking again can mend it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Banner {
    pub kind: Kind,
    pub text: String,
    /// The button's label, when there is one.
    pub retry: Option<&'static str>,
}

/// A page's Continue button.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Next {
    pub label: &'static str,
    pub enabled: bool,
}

/// What every page has around its content.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Chrome {
    /// The page's name in the navigation (and a header's title).
    pub title: &'static str,
    pub banner: Option<Banner>,
    /// The Continue button; pages that end an installation have none.
    pub next: Option<Next>,
    /// May the person go back from here (Back, Escape)?
    pub can_pop: bool,
}

/// A button as the flow says it: its label, whether it is shown, whether it
/// can be pressed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Button {
    pub label: String,
    pub visible: bool,
    pub enabled: bool,
}

impl Button {
    fn of(label: &str, control: Control) -> Self {
        Self {
            label: label.to_owned(),
            visible: control.visible,
            enabled: control.enabled,
        }
    }
}

/// A line of text under a field or a list: an error, or dimmed advice.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Note {
    pub text: String,
    pub error: bool,
}

/// A choice from a list that does not change: the language, the layout.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Combo {
    pub label: &'static str,
    pub help: &'static str,
    /// The index into the [`Catalogue`]'s list.
    pub selected: Option<usize>,
}

/// The line under the layout list: that the compositor is switching, or why it
/// did not take the layout.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct KeyboardLine {
    pub text: String,
    pub failed: bool,
    pub retry: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Welcome {
    pub chrome: Chrome,
    pub heading: &'static str,
    pub intro: &'static str,
    pub language: Combo,
    pub keyboard: Combo,
    /// The layout test field's label; the field is read only when `echo_try`.
    pub try_label: &'static str,
    pub echo_try: bool,
    pub keyboard_line: Option<KeyboardLine>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Choice {
    pub id: String,
    pub title: String,
    pub subtitle: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MachineBody {
    Loading {
        title: &'static str,
        detail: &'static str,
    },
    Problem {
        title: String,
        detail: String,
        retry: &'static str,
    },
    Choices {
        computer_label: &'static str,
        computer: String,
        group: &'static str,
        choices: Vec<Choice>,
        selected: String,
        /// How the configuration was chosen, when the banner does not say.
        notice: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Machine {
    pub chrome: Chrome,
    pub heading: &'static str,
    pub intro: &'static str,
    pub body: MachineBody,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Mode {
    pub title: &'static str,
    pub subtitle: &'static str,
    pub available: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Away {
    pub title: &'static str,
    pub intro: &'static str,
    pub rows: Vec<Choice>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DiskBody {
    Loading {
        title: &'static str,
        detail: &'static str,
    },
    /// Nothing to choose: the probe failed, or no disk can be installed to.
    Empty {
        glyph: Glyph,
        title: String,
        detail: String,
        retry: &'static str,
    },
    Choices {
        heading: &'static str,
        intro: &'static str,
        group: &'static str,
        offered: Vec<Choice>,
        selected: String,
        away: Option<Away>,
        modes_title: &'static str,
        modes: Vec<Mode>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Disk {
    pub chrome: Chrome,
    /// Why the page shows again: the disk that was chosen is gone.
    pub notice: Option<Banner>,
    pub body: DiskBody,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Field {
    pub label: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Account {
    pub chrome: Chrome,
    pub heading: &'static str,
    pub intro: &'static str,
    /// The choices cannot change while the helper checks them.
    pub editable: bool,
    /// How many times the flow wiped the passwords: when it changes, the
    /// fields that hold them are cleared.
    pub wiped: u64,
    pub name: Field,
    pub name_value: String,
    pub name_note: Option<Note>,
    pub password: Field,
    pub confirmation: Field,
    /// Under both password fields: a refusal (both fields are then invalid) or
    /// dimmed advice.
    pub password_note: Option<Note>,
}

/// A switch as the flow says it: what it is called, the line under it, where
/// it is and whether it can be flipped.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Switch {
    pub title: &'static str,
    pub line: String,
    pub on: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Encryption {
    pub chrome: Chrome,
    pub heading: &'static str,
    pub intro: &'static str,
    pub encrypt: Switch,
    /// Unlock with the security chip, where the computer can: the line under
    /// the switch says why not where it cannot.
    pub chip: Switch,
    /// How many times the flow wiped the passphrase: when it changes, the
    /// fields that hold it are cleared (the same count as the account's).
    pub wiped: u64,
    /// The passphrase fields, the line about the layout and the notes under
    /// them are shown while encryption is on.
    pub fields: bool,
    /// The fields can be typed in (encryption is on and the helper is not
    /// being asked).
    pub editable: bool,
    pub passphrase: Field,
    pub confirmation: Field,
    /// The keyboard layout the passphrase is typed in, in words.
    pub layout_line: String,
    /// Under both fields: a refusal (both fields are then invalid) or dimmed
    /// advice.
    pub note: Option<Note>,
    /// While encryption is off: what that means.
    pub off_line: Option<&'static str>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub title: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Group {
    pub title: &'static str,
    pub intro: &'static str,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewBodyView {
    Waiting {
        title: String,
        detail: String,
    },
    Problem {
        glyph: Glyph,
        title: String,
        detail: String,
        retry: Button,
    },
    Checked {
        heading: &'static str,
        intro: &'static str,
        groups: Vec<Group>,
        erase: String,
        end: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Review {
    pub chrome: Chrome,
    pub body: ReviewBodyView,
    /// The destructive button.
    pub button: Button,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct StageRow {
    /// A stable key (the stage's name).
    pub key: String,
    pub label: &'static str,
    pub status: StageStatus,
    pub fraction: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Installing {
    pub chrome: Chrome,
    pub heading: String,
    pub intro: &'static str,
    pub overall: f64,
    pub message: String,
    pub rows: Vec<StageRow>,
    pub show_log: &'static str,
    pub log: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Done {
    pub chrome: Chrome,
    pub glyph: Glyph,
    pub heading: String,
    pub detail: String,
    pub rows: Vec<Row>,
    pub restart: Button,
    pub power_off: Button,
    pub note: Option<Note>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Problem {
    pub chrome: Chrome,
    pub glyph: Glyph,
    pub heading: String,
    pub detail: String,
    /// Shown only with something in it.
    pub log_caption: &'static str,
    pub log: Vec<String>,
    pub retry: Button,
    pub save_log: Button,
    pub restart: Button,
    pub note: Option<Note>,
}

/// The recovery key's groups. The one secret the view carries: the page shows
/// it to the person once, so its `Debug` prints nothing of it (the
/// serialisation, which draws it, is not logged).
#[derive(Clone, PartialEq, Serialize)]
#[serde(transparent)]
pub struct KeyGroups(pub Vec<String>);

impl std::fmt::Debug for KeyGroups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyGroups(..)")
    }
}

/// A sentence with its heading: a refusal, or why a page has nothing to show.
/// `text` is the two as one sentence (`Message::sentence`), for a label that
/// has one line for both.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Said {
    pub title: String,
    pub detail: String,
    pub text: String,
}

impl Said {
    fn of(message: &Message) -> Self {
        Self {
            title: message.title.clone(),
            detail: message.detail.clone(),
            text: message.sentence(),
        }
    }
}

/// Going on without a recovery key, as the page offers it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Skip {
    pub title: &'static str,
    /// What the disk is then, and how to get a key later.
    pub detail: &'static str,
    pub button: Button,
    /// The helper is asked.
    pub skipping: bool,
    /// Why it did not work.
    pub problem: Option<Said>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Recovery {
    pub chrome: Chrome,
    pub heading: &'static str,
    pub intro: &'static str,
    /// The key's groups, while this interface holds the key.
    pub key: Option<KeyGroups>,
    /// "Write it down. It is shown only now." and where to keep it (with the
    /// key).
    pub warning: Option<&'static str>,
    pub keep: Option<&'static str>,
    /// Why there is no key: being fetched, shown before this window
    /// restarted, or could not be fetched.
    pub no_key: Option<Said>,
    /// What is asked, then the field for the whole key.
    pub ask: &'static str,
    pub field: Field,
    /// Under the field: the hint, or how far the typing is.
    pub hint: String,
    /// A refusal of the shape of what was typed (the field is then invalid).
    pub check: Option<Said>,
    /// The helper's refusal of what was typed.
    pub problem: Option<Said>,
    pub confirm: Button,
    /// The key is with the helper: the field waits.
    pub confirming: bool,
    /// Going on without a key, when it is offered.
    pub skip: Option<Skip>,
    /// The installation goes on: its last sentence and how far it is.
    pub progress_line: String,
    pub overall: f64,
}

/// The erase confirmation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Confirm {
    pub title: String,
    pub body: String,
    /// Cancel is first and has the focus: Tab reaches the destructive answer.
    pub cancel: &'static str,
    pub confirm: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Stick {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// Why it cannot take the log; the stick is then shown greyed.
    pub problem: Option<String>,
    pub save: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogBody {
    Looking {
        title: &'static str,
    },
    Choosing {
        group: &'static str,
        intro: &'static str,
        sticks: Vec<Stick>,
        empty: &'static str,
        look: &'static str,
    },
    Saving {
        title: &'static str,
        detail: &'static str,
    },
    Saved {
        glyph: Glyph,
        title: String,
        detail: String,
        close: &'static str,
    },
    Failed {
        glyph: Glyph,
        title: String,
        detail: String,
        retry: &'static str,
        close: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LogDialog {
    pub title: &'static str,
    /// The dialog cannot be closed while the log is being written.
    pub can_close: bool,
    pub body: LogBody,
}

/// What the whole window shows now.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct View {
    /// Nothing of the pages until the helper has said where an installation is.
    pub starting: Option<&'static str>,
    pub page: &'static str,
    /// The pages the navigation holds, bottom first.
    pub stack: Vec<&'static str>,
    pub welcome: Welcome,
    pub machine: Machine,
    pub disk: Disk,
    pub encryption: Encryption,
    pub account: Account,
    pub review: Review,
    pub installing: Option<Installing>,
    pub recovery: Option<Recovery>,
    pub done: Option<Done>,
    pub problem: Option<Problem>,
    pub confirm: Option<Confirm>,
    pub log: Option<LogDialog>,
}

/// What never changes while the window is up.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Catalogue {
    pub languages: Vec<String>,
    pub layouts: Vec<String>,
}

// --- building it -----------------------------------------------------------------

impl Flow {
    /// The two lists a page chooses from.
    pub fn catalogue(&self) -> Catalogue {
        Catalogue {
            languages: LANGUAGES.iter().map(language::Language::label).collect(),
            layouts: self.layouts().all().iter().map(|l| l.name.clone()).collect(),
        }
    }

    /// The banner every page shares: the problem that stops the installation,
    /// with Try again when asking the helper again can mend it.
    fn blocking_banner(&self) -> Option<Banner> {
        self.blocking_problem().map(|problem| Banner {
            kind: Kind::Error,
            text: problem.sentence(),
            retry: self.can_retry_probe().then_some(TRY_AGAIN),
        })
    }

    /// Continue: its label, and pressable unless this is the page and the flow
    /// says no (a page that is not on top keeps its button as it is).
    fn next(&self, page: PageId) -> Next {
        Next {
            label: self.continue_label(),
            enabled: self.page() != page || self.can_continue(),
        }
    }

    fn chrome(&self, page: PageId, title: &'static str, banner: Option<Banner>) -> Chrome {
        // No way back while the helper is asked, or once the erase was
        // confirmed; and none from the pages an installation ends on.
        let can_pop = !self.is_frozen()
            && !matches!(
                page,
                PageId::Welcome
                    | PageId::Installing
                    | PageId::Recovery
                    | PageId::Done
                    | PageId::Problem
            );
        Chrome {
            title,
            banner,
            next: matches!(
                page,
                PageId::Welcome
                    | PageId::Machine
                    | PageId::Disk
                    | PageId::Encryption
                    | PageId::Account
            )
            .then(|| self.next(page)),
            can_pop,
        }
    }

    fn welcome_view(&self) -> Welcome {
        // An installation this interface was waiting to see begin did not: the
        // banner says so until Continue (a problem that stops the installation
        // comes first).
        let banner = self.blocking_banner().or_else(|| {
            self.start_notice().map(|notice| Banner {
                kind: Kind::Warning,
                text: notice.sentence(),
                retry: None,
            })
        });
        let keyboard_line = match self.keyboard_status() {
            KeyboardStatus::Applied => None,
            KeyboardStatus::Switching => Some(KeyboardLine {
                text: messages::SWITCHING_LAYOUT.to_owned(),
                failed: false,
                retry: None,
            }),
            KeyboardStatus::Failed(message) => Some(KeyboardLine {
                text: message.sentence(),
                failed: true,
                retry: Some(TRY_AGAIN),
            }),
        };
        Welcome {
            chrome: self.chrome(PageId::Welcome, WELCOME_TITLE, banner),
            heading: WELCOME_HEADING,
            intro: WELCOME_INTRO,
            language: Combo {
                label: LANGUAGE_LABEL,
                help: LANGUAGE_HELP,
                selected: language::index_of(self.language()),
            },
            keyboard: Combo {
                label: KEYBOARD_LABEL,
                help: KEYBOARD_HELP,
                selected: self.layouts().index_of(self.keyboard()),
            },
            try_label: TRY_LAYOUT_LABEL,
            echo_try: self.echo_try(),
            keyboard_line,
        }
    }

    fn machine_view_page(&self) -> Machine {
        let mut banner = self.blocking_banner();
        let body = match (self.probe_state(), self.machine_view()) {
            (ProbeState::Pending, _) => MachineBody::Loading {
                title: MACHINE_LOADING_TITLE,
                detail: MACHINE_LOADING_DETAIL,
            },
            (ProbeState::Failed(message), _) => {
                // The problem is the page: no banner above it as well.
                banner = None;
                MachineBody::Problem {
                    title: message.title.clone(),
                    detail: message.detail.clone(),
                    retry: TRY_AGAIN,
                }
            }
            (ProbeState::Ready(_), Some(view)) => {
                let notice = view.notice();
                let text = notice.message.sentence();
                // A problem that stops the installation (a BIOS boot found
                // after the person left the first page) comes before the
                // notice about how the configuration was chosen, which is then
                // said in the page's own text.
                let quiet = if banner.is_some() {
                    Some(text)
                } else if notice.severity != Severity::Info {
                    let kind = match notice.severity {
                        Severity::Error => Kind::Error,
                        _ => Kind::Warning,
                    };
                    banner = Some(Banner {
                        kind,
                        text,
                        retry: None,
                    });
                    None
                } else {
                    Some(text)
                };
                MachineBody::Choices {
                    computer_label: COMPUTER_LABEL,
                    computer: view.computer.clone(),
                    group: CONFIGURATION,
                    choices: view
                        .choices
                        .iter()
                        .map(|c| Choice {
                            id: c.tag.clone(),
                            title: c.title.clone(),
                            subtitle: c.subtitle.clone(),
                        })
                        .collect(),
                    selected: self.machine_tag().unwrap_or_default().to_owned(),
                    notice: quiet,
                }
            }
            (ProbeState::Ready(_), None) => MachineBody::Loading {
                title: MACHINE_LOADING_TITLE,
                detail: MACHINE_LOADING_DETAIL,
            },
        };
        Machine {
            chrome: self.chrome(PageId::Machine, MACHINE_TITLE, banner),
            heading: MACHINE_HEADING,
            intro: MACHINE_INTRO,
            body,
        }
    }

    fn disk_page_view(&self) -> Disk {
        let empty = |glyph: Glyph, message: &Message| DiskBody::Empty {
            glyph,
            title: message.title.clone(),
            detail: message.detail.clone(),
            retry: TRY_AGAIN,
        };
        let body = match (self.probe_state(), self.disk_view()) {
            (ProbeState::Pending, _) => DiskBody::Loading {
                title: DISK_LOADING_TITLE,
                detail: DISK_LOADING_DETAIL,
            },
            (ProbeState::Failed(message), _) => empty(Glyph::Error, message),
            (ProbeState::Ready(_), Some(view)) => match self.disk_problem() {
                Some(problem) => empty(Glyph::Disk, &problem),
                None => {
                    let row = |c: &disks::DiskChoice| Choice {
                        id: c.id.clone(),
                        title: c.title.clone(),
                        subtitle: c.lines().join("\n"),
                    };
                    let away: Vec<Choice> = view
                        .choices
                        .iter()
                        .filter(|c| !c.is_offered())
                        .map(row)
                        .collect();
                    DiskBody::Choices {
                        heading: DISK_HEADING,
                        intro: DISK_INTRO,
                        group: DISK_GROUP,
                        offered: view
                            .choices
                            .iter()
                            .filter(|c| c.is_offered())
                            .map(row)
                            .collect(),
                        selected: self.disk().unwrap_or_default().to_owned(),
                        away: (!away.is_empty()).then_some(Away {
                            title: DISK_AWAY,
                            intro: DISK_AWAY_INTRO,
                            rows: away,
                        }),
                        modes_title: DISK_MODES,
                        modes: disks::modes()
                            .iter()
                            .map(|m| Mode {
                                title: m.title,
                                subtitle: m.subtitle,
                                available: m.available,
                            })
                            .collect(),
                    }
                }
            },
            (ProbeState::Ready(_), None) => DiskBody::Loading {
                title: DISK_LOADING_TITLE,
                detail: DISK_LOADING_DETAIL,
            },
        };
        Disk {
            chrome: self.chrome(PageId::Disk, DISK_TITLE, self.blocking_banner()),
            notice: self.disk_notice().map(|m| Banner {
                kind: Kind::Warning,
                text: m.sentence(),
                retry: None,
            }),
            body,
        }
    }

    fn account_view(&self) -> Account {
        let checks = self.account_checks();
        // Only a refusal is an error; advice is dimmed text.
        let password_note = match checks
            .password
            .message()
            .or_else(|| checks.confirmation.message())
        {
            Some(refusal) => Some(Note {
                text: refusal.sentence(),
                error: true,
            }),
            None => self.password_advice().map(|advice| Note {
                text: advice.sentence(),
                error: false,
            }),
        };
        Account {
            chrome: self.chrome(PageId::Account, ACCOUNT_TITLE, self.blocking_banner()),
            heading: ACCOUNT_HEADING,
            intro: ACCOUNT_INTRO,
            editable: !self.is_checking(),
            wiped: self.secrets_wiped(),
            name: Field { label: NAME_LABEL },
            name_value: self.administrator().to_owned(),
            name_note: checks.name.message().map(|m| Note {
                text: m.sentence(),
                error: true,
            }),
            password: Field {
                label: PASSWORD_LABEL,
            },
            confirmation: Field {
                label: CONFIRMATION_LABEL,
            },
            password_note,
        }
    }

    fn encryption_view(&self) -> Encryption {
        let editable = !self.is_frozen();
        let on = self.encrypt();
        let chip = self.chip_view();
        let checks = self.encryption_checks();
        // Only a refusal is an error; advice is dimmed text. Neither is shown
        // while encryption is off.
        let note = if on {
            match checks
                .passphrase
                .message()
                .or_else(|| checks.confirmation.message())
            {
                Some(refusal) => Some(Note {
                    text: refusal.sentence(),
                    error: true,
                }),
                None => self.passphrase_advice().map(|advice| Note {
                    text: advice.sentence(),
                    error: false,
                }),
            }
        } else {
            None
        };
        Encryption {
            chrome: self.chrome(PageId::Encryption, ENCRYPTION_TITLE, self.blocking_banner()),
            heading: ENCRYPTION_HEADING,
            intro: encryption::INTRODUCTION,
            encrypt: Switch {
                title: ENCRYPT_TITLE,
                line: ENCRYPT_SUBTITLE.to_owned(),
                on,
                enabled: editable,
            },
            chip: Switch {
                title: recovery::CHIP_TITLE,
                line: chip.line,
                on: chip.on,
                enabled: chip.sensitive,
            },
            wiped: self.secrets_wiped(),
            fields: on,
            editable: editable && on,
            passphrase: Field {
                label: PASSPHRASE_LABEL,
            },
            confirmation: Field {
                label: PASSPHRASE_CONFIRMATION_LABEL,
            },
            layout_line: self.passphrase_layout_line(),
            note,
            off_line: (!on).then_some(encryption::OFF_LINE),
        }
    }

    fn recovery_page_view(&self) -> Option<Recovery> {
        let v = self.recovery_view()?;
        Some(Recovery {
            chrome: Chrome {
                title: RECOVERY_TITLE,
                banner: None,
                next: None,
                can_pop: false,
            },
            heading: v.heading,
            intro: v.introduction,
            key: v.groups.map(KeyGroups),
            warning: v.warning,
            keep: v.keep,
            no_key: v.no_key.as_ref().map(Said::of),
            ask: v.ask,
            field: Field {
                label: v.typed.label,
            },
            hint: v.typed.hint,
            check: v.typed.check.message().map(Said::of),
            problem: v.problem.as_ref().map(Said::of),
            confirm: Button::of(v.confirm_label, v.confirm),
            confirming: v.confirming,
            skip: v.skip.map(|skip| Skip {
                title: skip.title,
                detail: skip.detail,
                button: Button::of(skip.label, skip.button),
                skipping: skip.skipping,
                problem: skip.problem.as_ref().map(Said::of),
            }),
            progress_line: v.progress_line,
            overall: v.overall,
        })
    }

    fn review_view(&self) -> Review {
        let page = self.review_page();
        let chrome = Chrome {
            title: REVIEW_TITLE,
            banner: None,
            next: None,
            can_pop: !self.is_frozen(),
        };
        let mut label = REVIEW_BUTTON.to_owned();
        let body = match &page.body {
            ReviewBody::Waiting(message) => ReviewBodyView::Waiting {
                title: message.title.clone(),
                detail: message.detail.clone(),
            },
            ReviewBody::Problem {
                message,
                icon,
                retry,
            } => ReviewBodyView::Problem {
                glyph: match icon {
                    crate::flow::ProblemIcon::Disk => Glyph::Disk,
                    crate::flow::ProblemIcon::Error => Glyph::Error,
                },
                title: message.title.clone(),
                detail: message.detail.clone(),
                retry: Button::of(TRY_AGAIN, *retry),
            },
            ReviewBody::Checked(review) => {
                label.clone_from(&review.button);
                let rows = |rows: &[crate::review::Row]| -> Vec<Row> {
                    rows.iter()
                        .map(|r| Row {
                            title: r.title.to_owned(),
                            value: r.value.clone(),
                        })
                        .collect()
                };
                let mut groups = vec![
                    Group {
                        title: GROUP_CHOICES,
                        intro: "",
                        rows: rows(&review.choices),
                    },
                    Group {
                        title: GROUP_DISK,
                        intro: "",
                        rows: rows(&review.disk),
                    },
                    Group {
                        title: GROUP_PARTITIONS,
                        intro: GROUP_PARTITIONS_INTRO,
                        rows: review
                            .partitions
                            .iter()
                            .map(|p| Row {
                                title: p.title.clone(),
                                value: p.detail.clone(),
                            })
                            .collect(),
                    },
                    Group {
                        title: GROUP_IMAGE,
                        intro: "",
                        rows: rows(&review.image),
                    },
                    Group {
                        title: GROUP_NOTES,
                        intro: "",
                        rows: rows(&review.notes),
                    },
                ];
                groups.retain(|g| !g.rows.is_empty());
                ReviewBodyView::Checked {
                    heading: REVIEW_HEADING,
                    intro: REVIEW_INTRO,
                    groups,
                    erase: review.erase.clone(),
                    end: REVIEW_END,
                }
            }
        };
        Review {
            chrome,
            body,
            // Its text is the plan's while there is one.
            button: Button::of(&label, page.button),
        }
    }

    /// Everything the window shows, in one value.
    pub fn view(&self) -> View {
        let installing = self.installing_view().map(|v| {
            let waiting = v.waiting.as_ref().map(|w| Banner {
                kind: Kind::Warning,
                text: w.sentence(),
                retry: None,
            });
            Installing {
                chrome: Chrome {
                    title: INSTALLING_TITLE,
                    banner: waiting,
                    next: None,
                    can_pop: false,
                },
                heading: v.heading,
                intro: INSTALLING_INTRO,
                overall: v.overall,
                message: v.message,
                rows: v
                    .rows
                    .iter()
                    .map(|r| StageRow {
                        key: format!("{:?}", r.stage),
                        label: r.label,
                        status: match r.status {
                            Status::Pending => StageStatus::Pending,
                            Status::Running => StageStatus::Running,
                            Status::Done => StageStatus::Done,
                            Status::Failed => StageStatus::Failed,
                        },
                        fraction: r.fraction,
                    })
                    .collect(),
                show_log: SHOW_LOG,
                log: v.log,
            }
        });
        let done = self.done_view().map(|v| Done {
            chrome: Chrome {
                title: DONE_TITLE,
                banner: None,
                next: None,
                can_pop: false,
            },
            glyph: Glyph::Success,
            heading: v.heading,
            detail: v.detail,
            rows: v
                .rows
                .iter()
                .map(|r| Row {
                    title: r.title.to_owned(),
                    value: r.value.clone(),
                })
                .collect(),
            restart: Button::of(RESTART, Control::shown(v.can_finish)),
            power_off: Button::of(POWER_OFF, Control::shown(v.can_finish)),
            note: closing_note(v.finishing, v.error.as_ref()),
        });
        let problem = self.problem_view().map(|v| Problem {
            chrome: Chrome {
                title: PROBLEM_TITLE,
                banner: None,
                next: None,
                can_pop: false,
            },
            glyph: match v.kind {
                ProblemKind::Lost => Glyph::Network,
                ProblemKind::Failed { .. } => Glyph::Error,
            },
            heading: v.message.title.clone(),
            detail: v.message.detail.clone(),
            log_caption: LOG_CAPTION,
            log: v.log,
            retry: Button::of(TRY_AGAIN, v.retry),
            save_log: Button::of(SAVE_LOG, v.save_log),
            restart: Button::of(RESTART, v.restart),
            note: closing_note(v.finishing, v.error.as_ref()),
        });
        let page = self.page();
        View {
            starting: self.is_attaching().then_some(STARTING_PAGE),
            page: page.id(),
            stack: page.stack().iter().map(|p| p.id()).collect(),
            welcome: self.welcome_view(),
            machine: self.machine_view_page(),
            disk: self.disk_page_view(),
            encryption: self.encryption_view(),
            account: self.account_view(),
            review: self.review_view(),
            installing,
            recovery: self.recovery_page_view(),
            done,
            problem,
            confirm: self.confirm_dialog().map(|c| Confirm {
                title: c.title,
                body: c.body,
                cancel: c.cancel,
                confirm: c.confirm,
            }),
            log: self.log_dialog_view(),
        }
    }

    fn log_dialog_view(&self) -> Option<LogDialog> {
        let body = match self.log_state() {
            LogSave::Closed => return None,
            LogSave::Looking => LogBody::Looking {
                title: LOG_LOOKING_TITLE,
            },
            LogSave::Choosing(choices) => LogBody::Choosing {
                group: LOG_CHOOSE,
                intro: LOG_CHOOSE_INTRO,
                sticks: choices
                    .iter()
                    .map(|c| Stick {
                        id: c.id.clone(),
                        title: c.title.clone(),
                        subtitle: match &c.problem {
                            Some(problem) => format!("{}\n{}", c.subtitle, problem.sentence()),
                            None => c.subtitle.clone(),
                        },
                        problem: c.problem.as_ref().map(Message::sentence),
                        save: LOG_SAVE_HERE,
                    })
                    .collect(),
                empty: LOG_NO_STICK,
                look: LOG_LOOK_AGAIN,
            },
            LogSave::Saving { .. } => LogBody::Saving {
                title: LOG_SAVING_TITLE,
                detail: LOG_SAVING_DETAIL,
            },
            LogSave::Saved(saved) => {
                let message = messages::log_saved(saved);
                LogBody::Saved {
                    glyph: Glyph::Success,
                    title: message.title,
                    detail: message.detail,
                    close: LOG_CLOSE,
                }
            }
            LogSave::Failed(message) => LogBody::Failed {
                glyph: Glyph::Error,
                title: message.title.clone(),
                detail: message.detail.clone(),
                retry: TRY_AGAIN,
                close: LOG_CLOSE,
            },
        };
        Some(LogDialog {
            title: LOG_DIALOG_TITLE,
            can_close: !matches!(self.log_state(), LogSave::Saving { .. }),
            body,
        })
    }
}

/// The line under the closing pages' buttons: that the computer is going down,
/// or why it is not.
fn closing_note(finishing: Option<&'static str>, error: Option<&Message>) -> Option<Note> {
    match (finishing, error) {
        (Some(text), _) => Some(Note {
            text: text.to_owned(),
            error: false,
        }),
        (None, Some(error)) => Some(Note {
            text: error.sentence(),
            error: true,
        }),
        (None, None) => None,
    }
}
