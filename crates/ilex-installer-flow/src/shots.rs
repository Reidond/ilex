//! The states the renderers draw: every page and every error state, as the
//! steps a person takes from a fixture scenario's start (`--self-check
//! --render`). One table for every interface, so that GTK and Kirigami draw
//! the same states and a trial can compare their counts: each toolkit's
//! renderer turns a [`Do`] into its own input and waits until the application
//! reports the page, the check and the dialog the [`Shot`] names.

use crate::flow::PageId;

/// What a shot does after its window is up and the helper has said where an
/// installation is.
#[derive(Clone, Copy)]
pub enum Do {
    Continue,
    Back,
    Jump(PageId),
    /// From now on the compositor refuses a layout switch.
    FailSwitches,
    Keyboard(&'static str),
    Language(&'static str),
    Machine(&'static str),
    Name(&'static str),
    Password(&'static str),
    Confirm(&'static str),
    /// A disk row (its id).
    Disk(&'static str),
    /// The review page's destructive button, and the dialog's two answers.
    Install,
    Accept,
    Dismiss,
    /// The installing page's log, opened.
    ExpandLog,
    /// Until the installation (which goes on by itself) has reached this page.
    Wait(PageId),
    /// The review page's Try again (look again, then check).
    Retry,
    /// A page's Try again after the helper could not be asked (its banner).
    LookAgain,
    /// The passwords typed on the administrator page are gone from the
    /// fields (the flow wiped its copy).
    PasswordsWiped,
    Restart,
    RetryInstall,
    ChooseLog,
    SaveLog(&'static str),
}

pub struct Shot {
    pub name: &'static str,
    pub scenario: &'static str,
    /// The page the shot ends on, and the word of its check (`Flow::check_word`)
    /// when it is the point of the shot (`""`: any, as long as none is running).
    pub page: PageId,
    pub check: &'static str,
    /// The dialog the shot ends with open (`confirm`, `log`), if any.
    pub dialog: &'static str,
    pub steps: &'static [Do],
}

pub const PASSWORD: &str = "correct horse battery";

macro_rules! shots {
    ($( $name:literal, $scenario:literal, $page:ident, $check:literal, $dialog:literal, [ $($step:expr),* $(,)? ] );* $(;)?) => {
        &[ $( Shot { name: $name, scenario: $scenario, page: PageId::$page, check: $check, dialog: $dialog, steps: &[ $($step),* ] } ),* ]
    };
}

#[rustfmt::skip]
pub const SHOTS: &[Shot] = shots![
    "welcome", "generic", Welcome, "", "", [];
    "welcome-german", "generic", Welcome, "", "", [Do::Keyboard("de-nodeadkeys"), Do::Language("de_DE.UTF-8")];
    "welcome-bios", "bios", Welcome, "", "", [];
    "welcome-helper-down", "helper-down", Welcome, "", "", [];
    "welcome-keyboard-failed", "generic", Welcome, "", "", [Do::FailSwitches, Do::Keyboard("de-nodeadkeys")];
    "machine-not-recognised", "generic", Machine, "", "", [Do::Continue];
    "machine-detected", "ms-7c35", Machine, "", "", [Do::Continue];
    "machine-detected-board", "ms-7c35-board", Machine, "", "", [Do::Continue];
    "machine-chosen", "ms-7c35", Machine, "", "", [Do::Continue, Do::Machine("generic.gnome")];
    "machine-conflict", "conflict", Machine, "", "", [Do::Continue];
    "machine-no-composition", "no-composition", Machine, "", "", [Do::Continue];
    "machine-boot-argument", "boot-override", Machine, "", "", [Do::Continue];
    "machine-boot-argument-unknown", "bad-override", Machine, "", "", [Do::Continue];
    "machine-helper-down", "helper-down", Machine, "", "", [Do::Jump(PageId::Machine)];
    "machine-bios", "bios", Machine, "", "", [Do::Jump(PageId::Machine)];
    "disk", "generic", Disk, "", "", [Do::Continue, Do::Continue];
    "disk-chosen", "generic", Disk, "", "", [Do::Continue, Do::Continue, Do::Disk("ata-WDC_WDS500G2B0A_214512800345")];
    "disk-none", "no-disk", Disk, "", "", [Do::Continue, Do::Continue];
    "disk-helper-down", "helper-down", Disk, "", "", [Do::Jump(PageId::Disk)];
    "disk-bios", "bios", Disk, "", "", [Do::Jump(PageId::Disk)];
    "account-empty", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue];
    "account-name-invalid", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("Ilex Maint")];
    "account-name-reserved", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("root")];
    "account-password-short", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password("abc"), Do::Confirm("abc")];
    "account-password-mismatch", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm("correct horse batter")];
    "account-valid", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD)];
    "account-refused-by-helper", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("rpcuser"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review", "generic", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-machine", "ms-7c35", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-other-disk", "generic", Review, "ok", "", [Do::Continue, Do::Continue, Do::Disk("ata-WDC_WDS500G2B0A_214512800345"), Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-no-disk", "no-disk", Review, "no-disk", "", [Do::Continue, Do::Continue, Do::Jump(PageId::Account), Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-registry-down", "registry-down", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-back-to-account", "generic", Account, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Back];
    "review-erase-dialog", "generic", Review, "ok", "confirm", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install];
    "review-erase-cancelled", "generic", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Dismiss];
    "review-apply-refused", "generic+apply-refused", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "review-apply-not-started", "generic+apply-lost", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "installing-early", "generic+early", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "installing", "generic+stalled", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "installing-log", "generic+stalled", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::ExpandLog];
    "installing-reattached", "generic+attach-installing", Installing, "", "", [];
    "done", "generic", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Done), Do::PasswordsWiped];
    "done-restarting", "generic", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Done), Do::Restart];
    "done-restart-refused", "generic+finish-fails", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Done), Do::Restart];
    "done-reattached", "generic+attach-done", Done, "", "", [];
    "problem-image", "generic+fail-image", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-partition", "generic+fail-partition", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-helper-stopped", "generic+helper-stopped", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-lost", "generic+lost", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-reattached", "generic+attach-failed", Problem, "", "", [];
    "problem-restart-refused", "generic+fail-image+finish-fails", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::Restart];
    "problem-restarting", "generic+fail-image", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::Restart, Do::PasswordsWiped];
    "installing-starting", "generic+attach-starting", Installing, "", "", [];
    "installing-after-lost-answer", "generic+apply-slow+stalled", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "welcome-state-down", "generic+state-down", Welcome, "", "", [];
    "welcome-state-down-retried", "generic+state-down+attach-failed", Problem, "", "", [Do::LookAgain];
    "welcome-not-started", "generic+attach-starting-refused", Welcome, "", "", [];
    "welcome-not-started-idle", "generic+attach-starting-refused-idle", Welcome, "", "", [];
    "installing-other-installation", "generic+apply-lost-other", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "review-active-rechecked", "generic+probe-active", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Retry];
    "review-active-state-down", "generic+probe-active+state-down-second", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Retry];
    "review-active-state-down-retried", "generic+probe-active+state-down-second", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Retry, Do::Retry];
    "review-apply-active-state-down", "generic+apply-active+state-down-second", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "disk-chosen-gone", "generic+vanish", Disk, "", "", [Do::Continue, Do::Continue, Do::Disk("ata-WDC_WDS500G2B0A_214512800345"), Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Retry];
    "problem-lost-then-installing", "generic+lost-then-running", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem)];
    "problem-lost-then-done", "generic+lost-then-done", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem)];
    "problem-retry", "generic+fail-image", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::RetryInstall];
    "problem-save-log", "generic+fail-image+sticks", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog];
    "problem-save-log-no-stick", "generic+fail-image+bare", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog];
    "problem-save-log-saved", "generic+fail-image+sticks", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog, Do::SaveLog("usb-SanDisk_Cruzer_Blade_4C530001-0:0")];
    "problem-save-log-failed", "generic+fail-image+sticks+log-fails", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog, Do::SaveLog("usb-SanDisk_Cruzer_Blade_4C530001-0:0")];
];
