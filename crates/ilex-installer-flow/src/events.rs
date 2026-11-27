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
    Name(String),
    Password(Secret),
    Confirmation(Secret),
    Continue,
    Disk(String),
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
            "name" => Self::Name(argument.to_owned()),
            "password" => Self::Password(Secret::new(argument)),
            "confirmation" => Self::Confirmation(Secret::new(argument)),
            "continue" => Self::Continue,
            "disk" => Self::Disk(argument.to_owned()),
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
    /// `Flow::check_word`: `not-run`, `running`, `ok`, `no-disk`, `failed`.
    pub check: &'static str,
    pub probe_pending: bool,
    pub keyboard_switching: bool,
    /// The first question to the helper (where an installation is) has not
    /// been answered.
    pub attaching: bool,
    /// The dialog the flow says is open: `confirm`, `log` or `` for none.
    pub dialog: &'static str,
}

impl Observed {
    /// Nothing is under way: no probe, no check, no layout switch.
    pub fn settled(&self) -> bool {
        self.check != "running"
            && !self.probe_pending
            && !self.keyboard_switching
            && !self.attaching
    }

    /// What nothing has been asked yet looks like.
    pub fn initial() -> Self {
        Self {
            page: PageId::Welcome,
            check: "not-run",
            probe_pending: true,
            keyboard_switching: false,
            attaching: true,
            dialog: "",
        }
    }
}

impl Flow {
    pub fn observed(&self) -> Observed {
        Observed {
            page: self.page(),
            check: self.check_word(),
            probe_pending: self.is_probing(),
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
            Event::Continue => self.advance(),
            Event::Disk(id) => {
                self.set_disk(&id);
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
                Step::Stay
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
