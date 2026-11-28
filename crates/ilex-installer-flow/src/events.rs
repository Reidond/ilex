//! What a person does, as the flow's own vocabulary: an [`Event`] is one input
//! (a button, a choice, text typed into a field), and [`Flow::handle`] says
//! what to call on the flow for it. Every interface turns its widgets' signals
//! into events and carries out the [`Step`] that comes back, so which flow
//! method an input means is decided here once, not per toolkit.
//!
//! An event also has a name and one text argument ([`Event::parse`]), which is
//! how an interface that cannot pass Rust values (QML) sends one. Passwords
//! are an argument like the rest on that path and are wiped by the caller
//! straight after parsing.

use crate::flow::{Flow, KeyboardStatus, LogSave, PageId, Step};
use crate::secret::Secret;
use ilex_installer_model::plan::Mode;

#[derive(Debug)]
pub enum Event {
    /// Ask the helper about this computer (again).
    Probe,
    /// An index into [`crate::language::LANGUAGES`].
    Language(usize),
    /// An index into the flow's layouts.
    Layout(usize),
    /// Try the layout again after the compositor did not take it.
    RetryLayout,
    /// What is typed in the layout test field.
    TryText(Secret),
    Machine(String),
    /// The daily user page's row (`none`, `custom`, `profile:<id>`), and the
    /// custom user's two fields.
    Daily(String),
    DailyName(String),
    DailySize(String),
    Name(String),
    Password(Secret),
    Confirmation(Secret),
    /// The encryption switch.
    Encrypt(bool),
    /// The security chip's switch.
    Chip(bool),
    /// The disk's passphrase and its confirmation.
    Passphrase(Secret),
    PassphraseConfirmation(Secret),
    /// What is typed in the recovery page's field: the whole key.
    RecoveryKey(Secret),
    /// The recovery page's Confirm.
    ConfirmRecovery,
    /// The recovery page's "Continue without a recovery key".
    ContinueWithoutKey,
    Continue,
    /// A Wi-Fi network chosen from the network page's list (its name).
    Network(String),
    /// What is typed in the network page's password field.
    NetworkPassword(Secret),
    /// The network page's Connect, and its Refresh.
    Connect,
    Refresh,
    Disk(String),
    /// The disk page's way of installing: `whole-disk` or `free-space`.
    Mode(String),
    /// The review page's Try again: look again, then check the choices.
    Retry,
    /// The review page's destructive button.
    Install,
    /// The erase dialog's answer.
    Confirmed(bool),
    /// The problem page's Try again.
    RetryInstall,
    Restart,
    PowerOff,
    /// The log dialog: look for sticks (open it, or look again).
    ChooseLog,
    SaveLog(String),
    CloseLog,
    /// The navigation now shows this page (the person went back).
    Visible(PageId),
    /// Show a page directly (the renderer only).
    Jump(PageId),
    /// The session has settled: report memory.
    Stats,
}

impl Event {
    /// An event from its name and text argument (`continue`, `language` `3`,
    /// `name` `ilexmaint`). `Err` names what was wrong; nothing is guessed.
    pub fn parse(name: &str, argument: &str) -> Result<Self, String> {
        let index = || -> Result<usize, String> {
            argument
                .parse()
                .map_err(|_| format!("{name}: {argument:?} is not an index"))
        };
        let switch = || -> Result<bool, String> {
            match argument {
                "on" => Ok(true),
                "off" => Ok(false),
                _ => Err(format!("{name}: {argument:?} is not on or off")),
            }
        };
        let page = || -> Result<PageId, String> {
            PageId::from_id(argument).ok_or_else(|| format!("{name}: no page {argument:?}"))
        };
        Ok(match name {
            "probe" => Self::Probe,
            "language" => Self::Language(index()?),
            "layout" => Self::Layout(index()?),
            "retry-layout" => Self::RetryLayout,
            "try-text" => Self::TryText(Secret::new(argument)),
            "machine" => Self::Machine(argument.to_owned()),
            "daily" => Self::Daily(argument.to_owned()),
            "daily-name" => Self::DailyName(argument.to_owned()),
            "daily-size" => Self::DailySize(argument.to_owned()),
            "name" => Self::Name(argument.to_owned()),
            "password" => Self::Password(Secret::new(argument)),
            "confirmation" => Self::Confirmation(Secret::new(argument)),
            "encrypt" => Self::Encrypt(switch()?),
            "chip" => Self::Chip(switch()?),
            "passphrase" => Self::Passphrase(Secret::new(argument)),
            "passphrase-confirmation" => Self::PassphraseConfirmation(Secret::new(argument)),
            "recovery-key" => Self::RecoveryKey(Secret::new(argument)),
            "confirm-recovery" => Self::ConfirmRecovery,
            "skip-recovery" => Self::ContinueWithoutKey,
            "continue" => Self::Continue,
            "network" => Self::Network(argument.to_owned()),
            "network-password" => Self::NetworkPassword(Secret::new(argument)),
            "connect" => Self::Connect,
            "refresh" => Self::Refresh,
            "disk" => Self::Disk(argument.to_owned()),
            "mode" => Self::Mode(argument.to_owned()),
            "retry" => Self::Retry,
            "install" => Self::Install,
            "confirm" => Self::Confirmed(true),
            "cancel" => Self::Confirmed(false),
            "retry-install" => Self::RetryInstall,
            "restart" => Self::Restart,
            "power-off" => Self::PowerOff,
            "choose-log" => Self::ChooseLog,
            "save-log" => Self::SaveLog(argument.to_owned()),
            "close-log" => Self::CloseLog,
            "visible" => Self::Visible(page()?),
            "jump" => Self::Jump(page()?),
            "stats" => Self::Stats,
            other => return Err(format!("no event {other:?}")),
        })
    }
}

/// What an interface is showing, in the flow's words: for the renderer to wait
/// on instead of a fixed time, and for a test to read.
#[derive(Clone, Debug, PartialEq)]
pub struct Observed {
    pub page: PageId,
    /// `Flow::check_word`: `not-run`, `running`, `ok`, `no-disk`, `failed`; on
    /// the recovery page, the key's: `key` (shown), `taking` (being fetched)
    /// or `gone` (not to be shown again).
    pub check: &'static str,
    pub probe_pending: bool,
    /// The look at the network the welcome page's Continue waits for, or the
    /// network page's Refresh, has not been answered.
    pub network_pending: bool,
    pub keyboard_switching: bool,
    /// The first question to the helper (where an installation is) has not
    /// been answered.
    pub attaching: bool,
    /// The dialog the flow says is open: `confirm` (the erase or install
    /// dialog), `log` or `` for none.
    pub dialog: &'static str,
}

impl Observed {
    /// Nothing is under way: no probe, no check, no layout switch.
    pub fn settled(&self) -> bool {
        self.check != "running"
            && !self.probe_pending
            && !self.network_pending
            && !self.keyboard_switching
            && !self.attaching
    }

    /// What nothing has been asked yet looks like.
    pub fn initial() -> Self {
        Self {
            page: PageId::Welcome,
            check: "not-run",
            probe_pending: true,
            network_pending: false,
            keyboard_switching: false,
            attaching: true,
            dialog: "",
        }
    }
}

impl Flow {
    /// The word of what the page is made of: the plan check's, and on the
    /// recovery page the key's.
    fn observed_check(&self) -> &'static str {
        if self.page() == PageId::Network {
            return self.network_word();
        }
        if self.page() != PageId::Recovery {
            return self.check_word();
        }
        match self.recovery_view() {
            Some(view) if view.groups.is_some() => "key",
            Some(view)
                if view
                    .no_key
                    .as_ref()
                    .is_some_and(|m| m.title == crate::recovery::WAITING_KEY) =>
            {
                "taking"
            }
            _ => "gone",
        }
    }

    pub fn observed(&self) -> Observed {
        Observed {
            page: self.page(),
            check: self.observed_check(),
            probe_pending: self.is_probing(),
            network_pending: self.network_pending(),
            keyboard_switching: self.keyboard_status() == KeyboardStatus::Switching,
            attaching: self.is_attaching(),
            dialog: if self.confirm_dialog().is_some() {
                "confirm"
            } else if *self.log_state() != LogSave::Closed {
                "log"
            } else {
                ""
            },
        }
    }

    /// Do what `event` means. The returned [`Step`] is what to carry out next
    /// ([`crate::runner::Services::job`]).
    pub fn handle(&mut self, event: Event) -> Step {
        match event {
            Event::Probe => self.start_probe(),
            Event::Language(index) => {
                if let Some(language) = crate::language::LANGUAGES.get(index) {
                    self.set_language(language.locale);
                }
                Step::Stay
            }
            Event::Layout(index) => {
                let id = self.layouts().all().get(index).map(|l| l.id.clone());
                match id {
                    Some(id) if self.keyboard() != id => self.set_keyboard(&id),
                    _ => Step::Stay,
                }
            }
            Event::RetryLayout => self.retry_keyboard(),
            Event::TryText(text) => {
                self.try_text(&text);
                Step::Stay
            }
            Event::Machine(tag) => {
                self.select_machine(&tag);
                Step::Stay
            }
            Event::Daily(id) => {
                self.set_daily(&id);
                Step::Stay
            }
            Event::DailyName(text) => {
                self.set_daily_name(&text);
                Step::Stay
            }
            Event::DailySize(text) => {
                self.set_daily_size(&text);
                Step::Stay
            }
            Event::Name(text) => {
                self.set_administrator(&text);
                Step::Stay
            }
            Event::Password(secret) => {
                self.set_password(secret);
                Step::Stay
            }
            Event::Confirmation(secret) => {
                self.set_confirmation(secret);
                Step::Stay
            }
            Event::Encrypt(on) => {
                self.set_encrypt(on);
                Step::Stay
            }
            Event::Chip(on) => {
                self.set_chip(on);
                Step::Stay
            }
            Event::Passphrase(secret) => {
                self.set_passphrase(secret);
                Step::Stay
            }
            Event::PassphraseConfirmation(secret) => {
                self.set_passphrase_confirmation(secret);
                Step::Stay
            }
            Event::RecoveryKey(secret) => {
                self.set_recovery_key(secret);
                Step::Stay
            }
            Event::ConfirmRecovery => self.confirm_recovery(),
            Event::ContinueWithoutKey => self.continue_without_recovery_key(),
            Event::Continue => self.advance(),
            Event::Network(ssid) => {
                self.select_network(&ssid);
                Step::Stay
            }
            Event::NetworkPassword(secret) => {
                self.set_network_password(secret);
                Step::Stay
            }
            Event::Connect => self.connect_network(),
            Event::Refresh => self.refresh_network(),
            Event::Disk(id) => {
                self.set_disk(&id);
                Step::Stay
            }
            Event::Mode(name) => {
                match name.as_str() {
                    "whole-disk" => self.set_mode(Mode::WholeDisk),
                    "free-space" => self.set_mode(Mode::FreeSpace),
                    _ => false,
                };
                Step::Stay
            }
            Event::Retry => self.retry(),
            Event::Install => self.install(),
            Event::Confirmed(true) => self.confirm_install(),
            Event::Confirmed(false) => {
                self.cancel_install();
                Step::Stay
            }
            Event::RetryInstall => self.retry_install(),
            Event::Restart => self.restart(),
            Event::PowerOff => self.power_off(),
            Event::ChooseLog => self.choose_log_device(),
            Event::SaveLog(device) => self.save_log(&device),
            Event::CloseLog => {
                self.close_log();
                Step::Stay
            }
            Event::Visible(page) => {
                // The view popped to `page` (Back, Escape): the flow steps
                // back to it, and refuses while the helper is asked (the view
                // is pushed again then).
                while self.page() != page && self.back().is_some() {}
                self.resume_network()
            }
            Event::Jump(page) => {
                self.show(page);
                Step::Stay
            }
            Event::Stats => {
                self.report_stats();
                Step::Stay
            }
        }
    }
}
