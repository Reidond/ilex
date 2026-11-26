//! The installer's flow: which page comes after which, what each page needs
//! before Continue works, and what happens when it is pressed. No toolkit and
//! no I/O: [`Flow`] is a state machine an interface drives and renders.
//!
//! ```text
//! interface                     Flow                         helper (Backend)
//! ---------                     ----                         ----------------
//! starts, spawns probe    -->   Flow::new(layouts)
//! probe answered          -->   flow.set_probe(result)    <--  Backend::probe
//! person types / picks    -->   flow.set_language(..) / set_keyboard(..) /
//!                               select_machine(..) / set_administrator(..) ...
//! Continue enabled?       <--   flow.can_continue()
//! Continue pressed        -->   match flow.advance() {
//!                                   Step::Goto(page)   => show the page
//!                                   Step::RunPlan(req) => spawn Backend::plan(req),
//!                                                         then flow.plan_finished(result)
//!                                   Step::Stay         => nothing
//!                               }
//! Back pressed            -->   flow.back()
//! ```
//!
//! The same sequence drives GTK (relm4 messages and commands) and Kirigami
//! (cxx-qt signals): the flow never calls the helper and never blocks.
//!
//! Pages of this version: `welcome` (language and keyboard layout), `machine`
//! (which configuration), `account` (the maintenance administrator) and
//! `ready`, where the choices checked by `Plan` are shown and this version
//! ends. The disk, encryption, review, progress, done and problem pages follow
//! in later changes; they extend [`PageId`] and the `advance` table.

use crate::account::{self, AccountChecks};
use crate::keyboard::{self, Layout, Layouts};
use crate::language::{self, Language};
use crate::machine::{MachineChoice, MachineView};
use crate::messages::{ClientError, Message};
use crate::secret::Secret;
use ilex_installer_model::plan::{ErrorCode, MIN_DISK_BYTES, Plan, PlanRequest};
use ilex_installer_model::probe::{Disk, Probe};
use serde_json::json;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageId {
    Welcome,
    Machine,
    Account,
    Ready,
}

impl PageId {
    pub const ORDER: [PageId; 4] = [Self::Welcome, Self::Machine, Self::Account, Self::Ready];

    /// The id printed on the serial console when the page is shown
    /// (`ILEX-INSTALLER page=<id>`) and used for widget names.
    pub fn id(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Machine => "machine",
            Self::Account => "account",
            Self::Ready => "ready",
        }
    }

    /// The page's heading and navigation title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Machine => "This computer",
            Self::Account => "Administrator",
            Self::Ready => "Ready to continue",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ORDER.into_iter().find(|p| p.id() == id)
    }

    pub fn next(self) -> Option<Self> {
        let at = Self::ORDER.iter().position(|p| *p == self)?;
        Self::ORDER.get(at + 1).copied()
    }

    pub fn previous(self) -> Option<Self> {
        let at = Self::ORDER.iter().position(|p| *p == self)?;
        at.checked_sub(1).map(|i| Self::ORDER[i])
    }
}

/// What the helper has said about this computer so far.
#[derive(Clone, Debug)]
pub enum ProbeState {
    Pending,
    Ready(Box<Probe>),
    Failed(Message),
}

/// Where the check of the choices with the helper's `Plan` stands.
#[derive(Clone, Debug)]
pub enum PlanCheck {
    NotRun,
    Running,
    Checked(Box<Plan>),
    /// No disk of this computer can be installed to: one sentence per disk.
    NoDisk(Vec<String>),
    Failed(Message),
}

/// What the interface does after Continue.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Show this page.
    Goto(PageId),
    /// Call `Backend::plan` with this request (on a worker) and give the
    /// answer to [`Flow::plan_finished`].
    RunPlan(Box<PlanRequest>),
    /// Nothing changes (Continue was not allowed, or the answer was a refusal
    /// shown on the current page).
    Stay,
}

pub struct Flow {
    layouts: Layouts,
    page: PageId,
    probe: ProbeState,
    view: Option<MachineView>,
    language: String,
    keyboard: String,
    machine: Option<String>,
    administrator: String,
    password: Secret,
    confirmation: Secret,
    /// The helper's refusal of the administrator's name, shown beside it until
    /// the name changes.
    name_refusal: Option<Message>,
    check: PlanCheck,
}

impl Flow {
    pub fn new(layouts: Layouts) -> Self {
        let keyboard = layouts.default_id().to_owned();
        Self {
            layouts,
            page: PageId::Welcome,
            probe: ProbeState::Pending,
            view: None,
            language: language::DEFAULT.to_owned(),
            keyboard,
            machine: None,
            administrator: String::new(),
            password: Secret::default(),
            confirmation: Secret::default(),
            name_refusal: None,
            check: PlanCheck::NotRun,
        }
    }

    // --- the helper's answers ------------------------------------------------

    /// Take the probe's answer (or its failure). The machine page's choice
    /// starts on what the probe preselected.
    pub fn set_probe(&mut self, result: Result<Probe, ClientError>) {
        match result {
            Ok(probe) => {
                let view = MachineView::from_probe(&probe);
                self.machine = Some(view.preselected.clone());
                self.view = Some(view);
                self.probe = ProbeState::Ready(Box::new(probe));
            }
            Err(error) => {
                self.view = None;
                self.machine = None;
                self.probe = ProbeState::Failed(error.message());
            }
        }
    }

    /// Ask again (after a failure): back to waiting for the answer.
    pub fn probe_restarted(&mut self) {
        self.probe = ProbeState::Pending;
    }

    pub fn probe_state(&self) -> &ProbeState {
        &self.probe
    }

    pub fn probe(&self) -> Option<&Probe> {
        match &self.probe {
            ProbeState::Ready(probe) => Some(probe),
            _ => None,
        }
    }

    /// The edition's name for headings, or `ilex` until the probe answers.
    pub fn edition_name(&self) -> &str {
        self.probe().map_or("ilex", |p| p.edition.name.as_str())
    }

    /// A problem that stops the installation before it starts: the probe
    /// failed, or this computer is not in UEFI mode.
    pub fn blocking_problem(&self) -> Option<Message> {
        match &self.probe {
            ProbeState::Failed(message) => Some(message.clone()),
            ProbeState::Ready(probe) if !probe.firmware.uefi => Some(
                crate::messages::plan_error(&ilex_installer_model::plan::PlanError::new(
                    ErrorCode::UnsupportedFirmware,
                    None,
                    "",
                )),
            ),
            _ => None,
        }
    }

    // --- pages ---------------------------------------------------------------

    pub fn page(&self) -> PageId {
        self.page
    }

    /// Go to a page directly (the interface's `--self-check` shots).
    pub fn show(&mut self, page: PageId) {
        self.page = page;
    }

    pub fn can_go_back(&self) -> bool {
        self.page.previous().is_some() && !matches!(self.check, PlanCheck::Running)
    }

    /// Step back one page; `Some(page)` when the page changed.
    pub fn back(&mut self) -> Option<PageId> {
        if !self.can_go_back() {
            return None;
        }
        let previous = self.page.previous()?;
        self.page = previous;
        Some(previous)
    }

    pub fn can_continue(&self) -> bool {
        if self.blocking_problem().is_some() {
            return false;
        }
        match self.page {
            PageId::Welcome => {
                language::find(&self.language).is_some() && self.layouts.get(&self.keyboard).is_some()
            }
            PageId::Machine => {
                self.probe().is_some()
                    && self
                        .machine
                        .as_deref()
                        .zip(self.view.as_ref())
                        .is_some_and(|(tag, view)| view.choice(tag).is_some())
            }
            PageId::Account => {
                self.account_checks().all_valid() && !matches!(self.check, PlanCheck::Running)
            }
            PageId::Ready => false,
        }
    }

    /// Continue was pressed.
    pub fn advance(&mut self) -> Step {
        if !self.can_continue() {
            return Step::Stay;
        }
        match self.page {
            PageId::Welcome | PageId::Machine => {
                let next = self.page.next().unwrap_or(self.page);
                self.page = next;
                Step::Goto(next)
            }
            PageId::Account => self.run_plan(),
            PageId::Ready => Step::Stay,
        }
    }

    /// Check the choices with the helper again (the Ready page's "Try again").
    pub fn retry_plan(&mut self) -> Step {
        match self.page {
            PageId::Ready | PageId::Account => self.run_plan(),
            _ => Step::Stay,
        }
    }

    fn run_plan(&mut self) -> Step {
        self.name_refusal = None;
        let Some(probe) = self.probe() else {
            return Step::Stay;
        };
        let disks = usable_disks(probe);
        if disks.is_empty() {
            self.check = PlanCheck::NoDisk(disk_report(probe));
            self.page = PageId::Ready;
            return Step::Goto(PageId::Ready);
        }
        match self.plan_request() {
            Ok(request) => {
                self.check = PlanCheck::Running;
                Step::RunPlan(Box::new(request))
            }
            Err(message) => {
                self.check = PlanCheck::Failed(message);
                self.page = PageId::Ready;
                Step::Goto(PageId::Ready)
            }
        }
    }

    /// The helper's answer to the request [`Step::RunPlan`] carried. A refusal
    /// of the administrator's name stays on the account page, beside the name;
    /// anything else is reported on the Ready page.
    pub fn plan_finished(&mut self, result: Result<Plan, ClientError>) -> Step {
        match result {
            Ok(plan) => {
                self.check = PlanCheck::Checked(Box::new(plan));
                self.page = PageId::Ready;
                Step::Goto(PageId::Ready)
            }
            Err(ClientError::Refused(refusal))
                if refusal.field.as_deref() == Some("administrator")
                    || refusal.code == ErrorCode::ReservedName =>
            {
                self.check = PlanCheck::NotRun;
                self.name_refusal = Some(crate::messages::plan_error(&refusal));
                self.page = PageId::Account;
                Step::Stay
            }
            Err(error) => {
                self.check = PlanCheck::Failed(error.message());
                self.page = PageId::Ready;
                Step::Goto(PageId::Ready)
            }
        }
    }

    pub fn plan_check(&self) -> &PlanCheck {
        &self.check
    }

    /// The request for `Plan`: the choices so far on the first disk the
    /// installer could use (the disk page of a later change replaces that
    /// provisional choice). Unset fields keep the model's defaults.
    pub fn plan_request(&self) -> Result<PlanRequest, Message> {
        let probe = self
            .probe()
            .ok_or_else(|| Message::title("The installer has not looked at this computer yet"))?;
        let tag = self
            .machine_tag()
            .ok_or_else(|| Message::title("No configuration is chosen"))?;
        let disk = usable_disks(probe)
            .into_iter()
            .next()
            .ok_or_else(|| Message::title("No disk can be installed to"))?;
        let document = json!({
            "composition": tag,
            "disk": disk.id,
            "mode": "whole-disk",
            "keyboard": self.keyboard,
            "language": self.language,
            "administrator": self.administrator,
        });
        PlanRequest::parse(&document.to_string()).map_err(|e| crate::messages::plan_error(&e))
    }

    // --- welcome -------------------------------------------------------------

    pub fn languages(&self) -> &'static [Language] {
        language::LANGUAGES
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    pub fn set_language(&mut self, locale: &str) -> bool {
        if language::find(locale).is_some() {
            self.language = locale.to_owned();
            true
        } else {
            false
        }
    }

    pub fn layouts(&self) -> &Layouts {
        &self.layouts
    }

    pub fn keyboard(&self) -> &str {
        &self.keyboard
    }

    pub fn keyboard_layout(&self) -> Option<&Layout> {
        self.layouts.get(&self.keyboard)
    }

    /// Choose a layout by console keymap name. The interface then switches the
    /// session to it through [`keyboard::SessionKeyboard`].
    pub fn set_keyboard(&mut self, id: &str) -> bool {
        if self.layouts.get(id).is_some() {
            self.keyboard = id.to_owned();
            true
        } else {
            false
        }
    }

    // --- machine -------------------------------------------------------------

    pub fn machine_view(&self) -> Option<&MachineView> {
        self.view.as_ref()
    }

    /// The tag chosen: the person's, else what the probe preselected.
    pub fn machine_tag(&self) -> Option<&str> {
        self.machine.as_deref()
    }

    pub fn machine_choice(&self) -> Option<&MachineChoice> {
        self.view.as_ref()?.choice(self.machine.as_deref()?)
    }

    pub fn select_machine(&mut self, tag: &str) -> bool {
        if self.view.as_ref().is_some_and(|v| v.choice(tag).is_some()) {
            self.machine = Some(tag.to_owned());
            self.check = PlanCheck::NotRun;
            true
        } else {
            false
        }
    }

    /// The line the selection trials read: `ILEX-COMPOSITION <tag> (<reason>)
    /// installs <image>`. It is the model's own when the choice is what the
    /// helper selected, and says "chosen in the installer" when the person
    /// picked another.
    pub fn composition_line(&self) -> Option<String> {
        let probe = self.probe()?;
        let tag = self.machine_tag()?;
        match &probe.selection {
            Some(selected) if selected.tag == tag => Some(selected.serial_line()),
            _ => {
                let reason = if probe.detection.tag == tag && probe.selection.is_none() {
                    probe.detection.reason.as_str()
                } else {
                    "chosen in the installer"
                };
                Some(format!(
                    "ILEX-COMPOSITION {tag} ({reason}) installs {}",
                    probe.catalogue.image_reference(tag)
                ))
            }
        }
    }

    // --- account -------------------------------------------------------------

    pub fn administrator(&self) -> &str {
        &self.administrator
    }

    pub fn set_administrator(&mut self, name: &str) {
        if self.administrator != name {
            self.administrator = name.to_owned();
            self.name_refusal = None;
            self.check = PlanCheck::NotRun;
        }
    }

    pub fn set_password(&mut self, text: &str) {
        self.password.set(text);
        self.check = PlanCheck::NotRun;
    }

    pub fn set_confirmation(&mut self, text: &str) {
        self.confirmation.set(text);
        self.check = PlanCheck::NotRun;
    }

    /// The password, for the helper's `SetSecret` (a later change). It never
    /// goes into a request, a marker or a log.
    pub fn password(&self) -> &Secret {
        &self.password
    }

    pub fn account_checks(&self) -> AccountChecks {
        let profiles: BTreeSet<&str> = self
            .probe()
            .map(|p| p.catalogue.profiles.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let mut name = account::name(&self.administrator, &profiles);
        if name.is_valid()
            && let Some(refusal) = &self.name_refusal
        {
            name = account::Check::Invalid(refusal.clone());
        }
        let password = account::password(&self.password);
        let confirmation = if password.is_valid() {
            account::confirmation(&self.password, &self.confirmation)
        } else if self.confirmation.is_empty() {
            account::Check::Empty
        } else {
            // A confirmation of a password that is itself refused cannot be
            // valid: the person sees the password's message first.
            account::confirmation(&self.password, &self.confirmation)
        };
        AccountChecks {
            name,
            password,
            confirmation,
        }
    }

    pub fn password_advice(&self) -> Option<Message> {
        account::password_advice(&self.password)
    }

    // --- markers ---------------------------------------------------------------

    /// The `key=value` words printed after `ILEX-INSTALLER page=<id>` when the
    /// page is shown: what the tests assert on the serial console.
    pub fn marker_fields(&self) -> String {
        match self.page {
            PageId::Welcome => format!("language={} keyboard={}", self.language, self.keyboard),
            PageId::Machine => match (&self.view, self.probe()) {
                (Some(view), Some(probe)) => format!(
                    "selected={} origin={} detected={} offered={}",
                    self.machine.as_deref().unwrap_or("none"),
                    view.origin.word(),
                    probe.detection.tag,
                    view.choices.len()
                ),
                _ => "selected=none origin=pending".to_owned(),
            },
            PageId::Account => format!("administrator={}", self.administrator),
            PageId::Ready => format!(
                "check={}",
                match &self.check {
                    PlanCheck::NotRun => "not-run",
                    PlanCheck::Running => "running",
                    PlanCheck::Checked(_) => "ok",
                    PlanCheck::NoDisk(_) => "no-disk",
                    PlanCheck::Failed(_) => "failed",
                }
            ),
        }
    }

    pub fn default_keyboard_id() -> &'static str {
        keyboard::DEFAULT
    }
}

/// The disks the installer could install to now: not the medium it started
/// from, not in use by the running system and big enough (the helper's own
/// minimum). The disk page of a later change lists them for the person.
pub fn usable_disks(probe: &Probe) -> Vec<&Disk> {
    probe
        .disks
        .iter()
        .filter(|d| !d.live_medium && d.in_use.is_empty() && d.size_bytes >= MIN_DISK_BYTES)
        .collect()
}

/// Why each disk that is not usable is not, one sentence each, for the page
/// that has to say that nothing can be installed to.
pub fn disk_report(probe: &Probe) -> Vec<String> {
    if probe.disks.is_empty() {
        return vec!["The installer found no disk in this computer.".to_owned()];
    }
    probe
        .disks
        .iter()
        .filter(|d| !usable_disks(probe).contains(d))
        .map(|d| {
            let name = if d.model.is_empty() {
                d.id.clone()
            } else {
                d.model.clone()
            };
            let why = if d.live_medium {
                "it is the medium the installer started from".to_owned()
            } else if let Some(use_) = d.in_use.first() {
                format!("it is in use: {use_}")
            } else {
                format!(
                    "it is too small ({}; at least {} are needed)",
                    human_bytes(d.size_bytes),
                    human_bytes(MIN_DISK_BYTES)
                )
            };
            format!("{name} ({}): {why}.", human_bytes(d.size_bytes))
        })
        .collect()
}

/// "1.8 GiB", "512 MiB", as a person counts disks and downloads.
pub fn human_bytes(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    const TIB: f64 = GIB * 1024.0;
    let value = bytes as f64;
    if value >= TIB {
        format!("{:.1} TiB", value / TIB)
    } else if value >= 10.0 * GIB {
        format!("{:.0} GiB", value / GIB)
    } else if value >= GIB {
        format!("{:.1} GiB", value / GIB)
    } else {
        format!("{:.0} MiB", value / MIB)
    }
}
