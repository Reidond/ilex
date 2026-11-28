//! The installer's flow: which page comes after which, what each page needs
//! before Continue works, and what happens when it is pressed. No toolkit and
//! no I/O: [`Flow`] is a state machine an interface drives and renders, and
//! every rule is here, so that GTK and Kirigami cannot disagree about one.
//!
//! ```text
//! interface                       Flow                          helper / compositor
//! ---------                       ----                          -------------------
//! starts                    -->   Flow::new(layouts)[.with_markers(..)], flow.start()
//! every method that can ask       returns a Step the interface carries out:
//!   for work returns a Step:
//!   Step::RunState{generation,..} -> (after the delay) Backend::state on a worker, then
//!                                    flow.state_finished(generation, result)
//!   Step::RunProbe{generation}    -> Backend::probe on a worker, then
//!                                    flow.probe_finished(generation, result)
//!   Step::RunPlan{generation,..}  -> Backend::plan on a worker, then
//!                                    flow.plan_finished(generation, result)
//!   Step::SwitchKeyboard{..}      -> SessionKeyboard::switch on a worker, then
//!                                    flow.keyboard_switched(generation, result)
//!   Step::Confirm(dialog)         -> show the erase dialog; its answer goes to
//!                                    flow.confirm_install() or flow.cancel_install()
//!   Step::RunApply{..}            -> Backend::start_installation on a worker, then
//!                                    flow.apply_finished(generation, result)
//!   Step::RunTakeKey{..}          -> Backend::take_recovery_key on a worker, then
//!                                    flow.take_key_finished(generation, result)
//!   Step::RunAcknowledge{..}      -> Backend::acknowledge_recovery_key on a worker, then
//!                                    flow.confirm_finished(generation, result)
//!   Step::RunLogProbe{..}         -> Backend::probe for the log dialog, then
//!                                    flow.log_probe_finished(generation, result)
//!   Step::RunFinish{..}           -> Backend::finish on a worker, then
//!                                    flow.finish_finished(generation, result)
//!   Step::Goto(page)              -> show the page (it is flow.page() already)
//!   Step::Stay                    -> nothing
//! person types / picks      -->   set_language / set_keyboard / select_machine /
//!                                 set_disk / set_administrator / set_password / ...
//! Continue enabled?         <--   flow.can_continue()
//! Continue pressed          -->   flow.advance()
//! Back pressed              -->   flow.back()
//! Erase and install         -->   flow.install(), then confirm_install()
//! Try again (review page)   -->   flow.retry()
//! Try again (problem page)  -->   flow.retry_install()
//! Try again (a failed first   -->   flow.start_probe(): asks State again until the helper
//!   question to the helper)         has answered it once, then looks at the computer
//! Restart / Power off       -->   flow.restart() / flow.power_off()
//! Save the log              -->   flow.choose_log_device(), then flow.save_log(id)
//! after every call          -->   flow.drain_markers(): the lines to print
//! ```
//!
//! The same sequence drives GTK (relm4 messages and commands) and Kirigami
//! (cxx-qt signals): the flow never calls the helper or the compositor and
//! never blocks.
//!
//! **Answers carry the generation of the request they answer.** A request is
//! numbered when the flow issues it, and the flow remembers the one request of
//! each kind that is in flight: an answer for any other number (an earlier
//! request's, a repeat, one that nothing asked for) is ignored (it returns
//! `Step::Stay` and changes nothing). Two checks, two probes or two layout
//! switches can never leave the flow with the answer to the older one, and a
//! probe is never asked while a plan is checked or another probe is
//! outstanding. While the helper is asked for the plan the choices that go into
//! it cannot be changed (the setters refuse), so what the answer describes is
//! what the flow still holds. A later change that applies a plan by its id
//! relies on this: the id it erases with is the latest answer's.
//!
//! **The flow also decides what is printed and when.** Every serial-console
//! line ([`crate::markers`]) is queued by the flow at the moment its cause is
//! accepted (a page shown, the machine page again when a probe answers, an
//! accepted language, layout or configuration, a request's answer or the
//! reason it was ignored) and handed over by [`Flow::drain_markers`]: an
//! interface prints what it is given and decides nothing. The test field's
//! text is queued only when the flow's own gate is open
//! ([`Flow::echo_try`]).
//!
//! Pages of this version: `welcome` (language and keyboard layout), `machine`
//! (which configuration), `disk` (where to install: the whole disk),
//! `encryption` (whether the disk is encrypted, and its passphrase), `account`
//! (the maintenance administrator) and `review`, where the choices checked by
//! `Plan` are shown and the destructive button begins the installation. After
//! it come the pages no one goes back from: `installing` (the stages, from the
//! helper's `State`), `recovery` (the recovery key of an encrypted installation,
//! shown while the image downloads and confirmed with two groups; it takes the
//! place of `installing` for as long as the key is waiting for the person),
//! `done` and `problem`.
//!
//! The installation's own steps (`install`, `confirm_install`, `apply_finished`,
//! `state_finished`, `restart`, `power_off`, `choose_log_device`, `save_log`,
//! `retry_install`) live in [`installation`]; the interface calls them the same
//! way: each returns a [`Step`] to carry out on a worker, and the answer comes
//! back with the request's generation.

use crate::account::{self, AccountChecks};
use crate::disks::{self, DiskChoice, DiskView};
use crate::encryption::{self, EncryptionChecks};
use crate::keyboard::{self, Layout, Layouts};
use crate::language::{self, Language};
use crate::machine::{MachineChoice, MachineView};
use crate::markers;
use crate::messages::{self, ClientError, Message};
use crate::recovery;
use crate::review::Confirmation;
use crate::secret::Secret;
use ilex_installer_model::finish::FinishAction;
use ilex_installer_model::plan::{ErrorCode, Plan, PlanRequest};
use ilex_installer_model::probe::{Disk, Probe};
use serde_json::json;
use std::collections::BTreeSet;

mod installation;

pub use installation::{
    AskedGroup, Control, DoneView, InstallingView, LogChoice, LogSave, Phase, ProblemIcon,
    ProblemKind, ProblemView, RecoveryView, ReviewBody, ReviewPage,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PageId {
    Welcome,
    Machine,
    Disk,
    Encryption,
    Account,
    Review,
    Installing,
    Recovery,
    Done,
    Problem,
}

impl PageId {
    /// The pages a person walks with Continue and Back.
    pub const ORDER: [PageId; 6] = [
        Self::Welcome,
        Self::Machine,
        Self::Disk,
        Self::Encryption,
        Self::Account,
        Self::Review,
    ];

    /// Every page, in the order a navigation stack builds them: the walk, then
    /// the pages an installation ends on (nobody goes back from those).
    pub const ALL: [PageId; 10] = [
        Self::Welcome,
        Self::Machine,
        Self::Disk,
        Self::Encryption,
        Self::Account,
        Self::Review,
        Self::Installing,
        Self::Recovery,
        Self::Done,
        Self::Problem,
    ];

    /// Installing, recovery, done and problem: Back and Continue do not exist
    /// there.
    pub fn is_closing(self) -> bool {
        matches!(
            self,
            Self::Installing | Self::Recovery | Self::Done | Self::Problem
        )
    }

    /// The page's place among [`PageId::ALL`].
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    /// The id printed on the serial console when the page is shown
    /// (`ILEX-INSTALLER page=<id>`) and used for widget names.
    pub fn id(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Machine => "machine",
            Self::Disk => "disk",
            Self::Encryption => "encryption",
            Self::Account => "account",
            Self::Review => "review",
            Self::Installing => "installing",
            Self::Recovery => "recovery",
            Self::Done => "done",
            Self::Problem => "problem",
        }
    }

    /// The page's heading and navigation title.
    pub fn title(self) -> &'static str {
        match self {
            Self::Welcome => "Welcome",
            Self::Machine => "This computer",
            Self::Disk => "Disk",
            Self::Encryption => "Encryption",
            Self::Account => "Administrator",
            Self::Review => "Review",
            Self::Installing => "Installing",
            Self::Recovery => "Recovery key",
            Self::Done => "Done",
            Self::Problem => "Problem",
        }
    }

    /// The pages a navigation stack holds when this one is showing: the walk up
    /// to it, or (nobody goes back from the pages an installation ends on, and
    /// the flow moves between those on its own: Installing, Problem, Done in
    /// any order) just the page itself. An interface makes its stack this one:
    /// push or pop when it is a step from the stack it has, replace it
    /// otherwise.
    pub fn stack(self) -> Vec<PageId> {
        if self.is_closing() {
            vec![self]
        } else {
            let at = Self::ORDER.iter().position(|p| *p == self).unwrap_or(0);
            Self::ORDER[..=at].to_vec()
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.id() == id)
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
    /// The helper is being asked (a fresh look at the computer first, on a
    /// retry). The choices cannot change and the person cannot leave.
    Running,
    Checked(Box<Plan>),
    /// No disk of this computer can be installed to: one sentence per disk.
    NoDisk(Vec<String>),
    Failed(Message),
}

/// Whether the session's keyboard layout is the one the person chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyboardStatus {
    /// The compositor confirmed the layout that was asked for.
    Applied,
    /// A switch is under way, or one is waiting for it: Continue is off.
    Switching,
    /// The compositor did not take it: shown on the page, with a way to try
    /// again; Continue stays off until a layout is in effect.
    Failed(Message),
}

/// What the interface does next. Every method that can need the helper or the
/// compositor returns one; the interface carries it out (on a worker, never
/// blocking its main loop) and gives the answer back with the generation.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Show this page (it is [`Flow::page`] already). An interface that
    /// renders from the flow's state has nothing else to do.
    Goto(PageId),
    /// Call `Backend::probe` and give the answer to [`Flow::probe_finished`].
    RunProbe { generation: u64 },
    /// Call `Backend::plan` with this request and give the answer to
    /// [`Flow::plan_finished`].
    RunPlan {
        generation: u64,
        request: Box<PlanRequest>,
    },
    /// Make the session type in this layout (`SessionKeyboard::switch`) and
    /// give the answer to [`Flow::keyboard_switched`]. Only one switch is
    /// ever outstanding.
    SwitchKeyboard { generation: u64, layout: Layout },
    /// Show this dialog and give the person's answer to
    /// [`Flow::confirm_install`] (yes) or [`Flow::cancel_install`] (no).
    Confirm(Confirmation),
    /// Hand the administrator's password (and the disk's passphrase, when the
    /// plan encrypts) to the helper and begin the installation of `plan`
    /// (`Backend::start_installation`); give the answer to
    /// [`Flow::apply_finished`].
    RunApply {
        generation: u64,
        plan: String,
        erase_disk: String,
        password: Secret,
        /// The disk's passphrase when the plan encrypts (`SetSecret` for it
        /// comes first).
        passphrase: Option<Secret>,
    },
    /// Ask the helper where the installation is (`Backend::state`) after
    /// `delay_ms`, and give the answer to [`Flow::state_finished`]. At most one
    /// is outstanding.
    RunState { generation: u64, delay_ms: u64 },
    /// Fetch the installation's recovery key from the helper
    /// (`Backend::take_recovery_key`; it hands the key over once) and give the
    /// answer to [`Flow::take_key_finished`].
    RunTakeKey { generation: u64, plan: String },
    /// Give the helper the two groups the person typed
    /// (`Backend::acknowledge_recovery_key`; `answer` is the model's answer
    /// text) and give the answer to [`Flow::confirm_finished`].
    RunAcknowledge {
        generation: u64,
        plan: String,
        answer: Secret,
    },
    /// Call `Backend::probe` for the log dialog (which sticks there are) and
    /// give the answer to [`Flow::log_probe_finished`].
    RunLogProbe { generation: u64 },
    /// Call `Backend::finish` and give the answer to [`Flow::finish_finished`].
    RunFinish {
        generation: u64,
        action: FinishAction,
    },
    /// Nothing changes (Continue was not allowed, the answer was stale, or
    /// the answer was a refusal shown on the current page).
    Stay,
}

/// The layout switch of the session: what the person asked for, what the
/// compositor confirmed, and the one switch in flight.
struct KeyboardSwitch {
    requested: String,
    /// `None` after a failed switch: what the session types is then unknown.
    applied: Option<String>,
    generation: u64,
    in_flight: Option<(u64, String)>,
    failure: Option<Message>,
}

/// How the interface wants the flow's marker lines (see [`Flow::with_markers`]).
#[derive(Clone, Debug)]
pub struct MarkerConfig {
    /// The edition identifier every page marker carries (`gnome`).
    pub edition: String,
    /// The helper is simulated: no person types, so the layout test field's
    /// text may be printed.
    pub simulated: bool,
    /// The fixed public test credential is present (see
    /// [`markers::echo_allowed`]).
    pub echo_credential: bool,
}

impl MarkerConfig {
    /// The real session's: the test credential is read from the environment.
    pub fn session(edition: impl Into<String>, simulated: bool) -> Self {
        Self {
            edition: edition.into(),
            simulated,
            echo_credential: markers::echo_allowed(),
        }
    }
}

/// The queue of marker lines, and what decides which are queued.
struct Markers {
    edition: String,
    /// The layout test field's text may be printed.
    echo: bool,
    /// The window is on screen: page markers are queued from now on.
    shown: bool,
    outbox: Vec<String>,
}

pub struct Flow {
    layouts: Layouts,
    page: PageId,
    probe: ProbeState,
    probe_generation: u64,
    /// The probe request whose answer is awaited: the only one accepted.
    probe_in_flight: Option<u64>,
    markers: Option<Markers>,
    /// A check of the choices waits for the probe in flight (a retry looks at
    /// the computer again before it asks for the plan).
    resume_check: bool,
    /// The last check failed in a way a fresh look at the computer can mend
    /// (a disk gone, in use, a helper that restarted): the next one starts
    /// with a probe.
    stale_probe: bool,
    view: Option<MachineView>,
    language: String,
    keyboard: KeyboardSwitch,
    machine: Option<String>,
    /// The person picked the configuration (a new probe keeps it while it is
    /// still offered; otherwise it follows the probe's preselection).
    machine_chosen: bool,
    /// The disk to install to (its id), and whether the person chose it (a new
    /// probe keeps the choice while the disk is still offered; otherwise the
    /// first disk that can be installed to is chosen).
    disk: Option<String>,
    disk_chosen: bool,
    disk_view: Option<DiskView>,
    /// `State` could not be asked at start-up (the helper was not up, or is
    /// restarting) and has not been answered since. Nothing is known of an
    /// installation that may be under way: the first look at the computer
    /// waits, and Try again asks `State` again.
    attach_failed: bool,
    /// The welcome page says that an installation did not start: this interface
    /// was restarted while the helper's `Apply` checked, and the helper then
    /// did not begin (it refused, or it stopped). Said until Continue.
    start_notice: Option<Message>,
    /// The person's disk is gone (unplugged, in use now) and the flow went to
    /// the disk page: said there until a disk is chosen.
    disk_notice: Option<Message>,
    /// How many times the flow wiped the password it holds (an interface
    /// clears its own fields when this changes).
    secrets_wiped: u64,
    /// The installation, from the review page's button to its end.
    inst: installation::Installation,
    /// Whether the disk is encrypted (on until the person turns it off), and
    /// the passphrase that unlocks it, typed twice.
    encrypt: bool,
    /// Unlock with the security chip when the computer can (on until the person
    /// turns it off; there is nothing to turn off on a computer that cannot).
    chip: bool,
    passphrase: Secret,
    passphrase_confirmation: Secret,
    administrator: String,
    password: Secret,
    confirmation: Secret,
    /// The helper's refusal of the administrator's name, shown beside it until
    /// the name changes.
    name_refusal: Option<Message>,
    check: PlanCheck,
    plan_generation: u64,
}

impl Flow {
    pub fn new(layouts: Layouts) -> Self {
        // The live session starts on the default layout.
        let initial = layouts.default_id().to_owned();
        Self {
            layouts,
            page: PageId::Welcome,
            probe: ProbeState::Pending,
            probe_generation: 0,
            probe_in_flight: None,
            markers: None,
            resume_check: false,
            stale_probe: false,
            view: None,
            language: language::DEFAULT.to_owned(),
            keyboard: KeyboardSwitch {
                requested: initial.clone(),
                applied: Some(initial),
                generation: 0,
                in_flight: None,
                failure: None,
            },
            machine: None,
            machine_chosen: false,
            disk: None,
            disk_chosen: false,
            disk_view: None,
            attach_failed: false,
            start_notice: None,
            disk_notice: None,
            secrets_wiped: 0,
            inst: installation::Installation::default(),
            encrypt: true,
            chip: true,
            passphrase: Secret::default(),
            passphrase_confirmation: Secret::default(),
            administrator: String::new(),
            password: Secret::default(),
            confirmation: Secret::default(),
            name_refusal: None,
            check: PlanCheck::NotRun,
            plan_generation: 0,
        }
    }

    // --- markers ---------------------------------------------------------------

    /// Have the flow queue the serial-console lines of [`crate::markers`] for
    /// the interface to print (see [`Flow::drain_markers`]). Without this the
    /// flow queues nothing (the renderer and the self-check).
    pub fn with_markers(mut self, config: MarkerConfig) -> Self {
        self.markers = Some(Markers {
            edition: config.edition,
            echo: config.simulated || config.echo_credential,
            shown: false,
            outbox: Vec::new(),
        });
        self
    }

    /// The lines queued since the last call, in the order their causes were
    /// accepted. Call it after every flow method and print each line.
    pub fn drain_markers(&mut self) -> Vec<String> {
        self.markers
            .as_mut()
            .map(|m| std::mem::take(&mut m.outbox))
            .unwrap_or_default()
    }

    /// The window is on screen: queue the first page's marker, with the
    /// toolkit's own words (`renderer=… scale=…`) and the uptime after them.
    /// Page markers are queued from now on; calling it again does nothing.
    pub fn window_shown(&mut self, toolkit: &str) {
        let Some(queue) = self.markers.as_mut() else {
            return;
        };
        if queue.shown {
            return;
        }
        queue.shown = true;
        let extra = format!("{toolkit} uptime={}", markers::uptime());
        if self.inst.is_attaching() {
            // The first page is the one the helper's answer puts the flow on:
            // it is announced then, with these words.
            self.inst.pending_extra = Some(extra);
            return;
        }
        self.announce_page(Some(&extra));
    }

    /// May the interface read the layout test field at all? Only when the text
    /// may be printed: people type their intended passwords there, so on a
    /// normal boot the field is not read and nothing of it is queued.
    pub fn echo_try(&self) -> bool {
        self.markers.as_ref().is_some_and(|m| m.echo)
    }

    /// The layout test field's text changed. It is queued (as `event=try`)
    /// only when [`Flow::echo_try`] is true; otherwise it is dropped.
    pub fn try_text(&mut self, text: &Secret) {
        let line = self
            .markers
            .as_ref()
            .and_then(|m| markers::tried(&self.keyboard.requested, text, m.echo));
        if let Some(line) = line {
            self.say(line);
        }
    }

    /// The session has settled: queue the memory and uptime report.
    pub fn report_stats(&mut self) {
        if self.markers.is_some() {
            self.say(markers::stats());
        }
    }

    fn say(&mut self, line: String) {
        if let Some(queue) = self.markers.as_mut() {
            queue.outbox.push(line);
        }
    }

    /// The page's marker (with `extra` words after it), and on the machine
    /// page the composition line and the notice that follow it. Nothing before
    /// the window is shown.
    fn announce_page(&mut self, extra: Option<&str>) {
        let Some(queue) = self.markers.as_ref().filter(|m| m.shown) else {
            return;
        };
        let pending = self.inst.pending_extra.take();
        let extra = extra.map(str::to_owned).or(pending);
        let mut line = markers::page(&queue.edition, self.page, &self.marker_fields());
        if let Some(extra) = extra {
            line.push(' ');
            line.push_str(&extra);
        }
        let mut lines = vec![line];
        if self.page == PageId::Machine {
            lines.extend(self.composition_line());
            if let Some(view) = &self.view {
                lines.push(markers::notice(&view.notice()));
            }
        }
        for line in lines {
            self.say(line);
        }
    }

    // --- the helper's answers ------------------------------------------------

    /// Ask the helper about this computer: at the start, and again after a
    /// failure (the page's Try again). Refused while a plan is being checked
    /// (a retry's own look is part of the check and is already outstanding) and
    /// while another probe is: one request at a time, so an answer can only
    /// ever be for the one in flight.
    pub fn start_probe(&mut self) -> Step {
        if self.inst.is_attaching() {
            // `State` is being asked: the look at the computer follows its
            // answer, never beside it.
            return Step::Stay;
        }
        if self.attach_failed {
            // The helper never said where an installation is (it was not up,
            // or answered an error): that comes first, or a helper that has
            // since restarted would let a probe succeed over an installation
            // whose outcome nobody read.
            return self.start();
        }
        if self.is_checking() || self.probe_in_flight.is_some() {
            return Step::Stay;
        }
        self.begin_probe()
    }

    fn begin_probe(&mut self) -> Step {
        self.probe_generation += 1;
        self.probe_in_flight = Some(self.probe_generation);
        self.probe = ProbeState::Pending;
        Step::RunProbe {
            generation: self.probe_generation,
        }
    }

    /// The helper's answer (or its failure) to the [`Step::RunProbe`] in
    /// flight; an answer to any other number, or when none was asked for,
    /// changes nothing. The machine page's choice starts on what the probe
    /// preselected; a later look keeps the person's own choice while it is
    /// still offered (and a choice that is reset invalidates the earlier
    /// check). When the look was the first half of a retry, the check of the
    /// choices follows. The machine page is announced again when it is the one
    /// shown: what it says depends on this answer.
    pub fn probe_finished(&mut self, generation: u64, result: Result<Probe, ClientError>) -> Step {
        if self.probe_in_flight != Some(generation) {
            self.say(markers::stale("probe"));
            return Step::Stay;
        }
        self.probe_in_flight = None;
        self.say(markers::probe(&result));
        let step = self.take_probe(result);
        // (A step that moved the flow announced its page already.)
        if matches!(self.page, PageId::Machine | PageId::Disk)
            && !self.is_attaching()
            && !matches!(step, Step::Goto(_))
        {
            self.announce_page(None);
        }
        step
    }

    fn take_probe(&mut self, result: Result<Probe, ClientError>) -> Step {
        match result {
            Ok(probe) => {
                let view = MachineView::from_probe(&probe);
                let keep = self.machine_chosen
                    && self
                        .machine
                        .as_deref()
                        .is_some_and(|tag| view.choice(tag).is_some());
                let before = self.machine.clone();
                if !keep {
                    self.machine = Some(view.preselected.clone());
                    self.machine_chosen = false;
                }
                let reset = self.machine != before;
                // The disk: the person's choice while it is still offered,
                // else the first disk that can be installed to. A chosen disk
                // that is gone is never replaced silently: the disk page says
                // so (below).
                let disks = DiskView::from_probe(&probe);
                let keep_disk = self.disk_chosen
                    && self
                        .disk
                        .as_deref()
                        .and_then(|id| disks.choice(id))
                        .is_some_and(DiskChoice::is_offered);
                let disk_before = self.disk.clone();
                let gone = (self.disk_chosen && !keep_disk)
                    .then(|| {
                        let id = disk_before.as_deref()?;
                        Some((
                            id.to_owned(),
                            self.disk_view
                                .as_ref()
                                .and_then(|v| v.choice(id))
                                .map_or_else(|| id.to_owned(), |c| c.title.clone()),
                        ))
                    })
                    .flatten();
                if !keep_disk {
                    self.disk = disks.offered().next().map(|c| c.id.clone());
                    self.disk_chosen = false;
                }
                let disk_reset = self.disk != disk_before;
                self.disk_view = Some(disks);
                self.view = Some(view);
                self.probe = ProbeState::Ready(Box::new(probe));
                if disk_reset {
                    self.invalidate_check();
                }
                if reset {
                    // What an earlier check was made of has changed.
                    self.invalidate_check();
                    if before.is_some()
                        && self.page != PageId::Machine
                        && let Some(line) = self.composition_line()
                    {
                        // The machine page says it itself when it is shown.
                        self.say(line);
                    }
                }
                self.stale_probe = false;
                if let Some((id, title)) = gone {
                    self.say(markers::disk_gone(&id));
                    self.disk_notice = Some(messages::disk_gone(&title));
                    if self.page.index() > PageId::Disk.index() {
                        // Back to the disk page, with what happened: the check
                        // is over, whatever the helper would have said of the
                        // disk it found there instead.
                        self.resume_check = false;
                        self.check = PlanCheck::NotRun;
                        return self.go(PageId::Disk);
                    }
                }
                if self.resume_check {
                    self.resume_check = false;
                    return self.run_plan();
                }
                Step::Stay
            }
            // The helper is installing: the first question to ask is where.
            Err(ClientError::Refused(ref refusal))
                if refusal.code == ErrorCode::InstallationActive =>
            {
                self.resume_check = false;
                if self.is_checking() {
                    self.check = PlanCheck::NotRun;
                }
                self.attach_again()
            }
            Err(error) => {
                self.view = None;
                self.disk_view = None;
                self.probe = ProbeState::Failed(error.message());
                if self.resume_check {
                    self.resume_check = false;
                    return self.conclude(PlanCheck::Failed(error.message()));
                }
                Step::Stay
            }
        }
    }

    /// Is a look at the computer outstanding?
    pub fn is_probing(&self) -> bool {
        self.probe_in_flight.is_some()
    }

    /// Is this the answer to the probe request in flight?
    pub fn probe_is_current(&self, generation: u64) -> bool {
        self.probe_in_flight == Some(generation)
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
    /// failed, or this computer is not in UEFI mode. Every page shows it,
    /// wherever the person is when the answer arrives.
    pub fn blocking_problem(&self) -> Option<Message> {
        match &self.probe {
            ProbeState::Failed(message) => Some(message.clone()),
            ProbeState::Ready(probe) if !probe.firmware.uefi => Some(messages::plan_error(
                &ilex_installer_model::plan::PlanError::new(
                    ErrorCode::UnsupportedFirmware,
                    None,
                    "",
                ),
            )),
            _ => None,
        }
    }

    /// May the page offer to ask the helper again? Only a failed probe can be
    /// mended by looking again; a BIOS boot needs a restart.
    pub fn can_retry_probe(&self) -> bool {
        matches!(self.probe, ProbeState::Failed(_)) && !self.is_checking()
    }

    // --- pages ---------------------------------------------------------------

    pub fn page(&self) -> PageId {
        self.page
    }

    /// Go to a page directly. For the self-check and the renderer only: a
    /// person moves with [`Flow::advance`] and [`Flow::back`].
    pub fn show(&mut self, page: PageId) {
        self.page = page;
    }

    fn go(&mut self, page: PageId) -> Step {
        if self.page == page {
            Step::Stay
        } else {
            self.page = page;
            self.announce_page(None);
            Step::Goto(page)
        }
    }

    /// Back is off on the first page, while the helper is being asked, and
    /// from the moment the erase is confirmed.
    pub fn can_go_back(&self) -> bool {
        self.page.previous().is_some() && !self.is_frozen()
    }

    /// Step back one page; `Some(page)` when the page changed.
    pub fn back(&mut self) -> Option<PageId> {
        if !self.can_go_back() {
            return None;
        }
        let previous = self.page.previous()?;
        self.page = previous;
        self.announce_page(None);
        Some(previous)
    }

    pub fn can_continue(&self) -> bool {
        if self.blocking_problem().is_some() || self.inst.is_attaching() {
            return false;
        }
        match self.page {
            PageId::Welcome => {
                language::find(&self.language).is_some()
                    && self.layouts.get(&self.keyboard.requested).is_some()
                    && self.keyboard_status() == KeyboardStatus::Applied
            }
            PageId::Machine => {
                self.probe().is_some()
                    && self
                        .machine
                        .as_deref()
                        .zip(self.view.as_ref())
                        .is_some_and(|(tag, view)| view.choice(tag).is_some())
            }
            PageId::Disk => {
                self.probe().is_some()
                    && self
                        .disk
                        .as_deref()
                        .zip(self.disk_view.as_ref())
                        .and_then(|(id, view)| view.choice(id))
                        .is_some_and(DiskChoice::is_offered)
            }
            // Off, there is nothing to check; on, the passphrase and its
            // confirmation.
            PageId::Encryption => !self.encrypt || self.encryption_checks().all_valid(),
            // The check needs the computer's own answer: names are judged
            // against its profiles and the plan is asked for its disks. After
            // Back and a new look, Continue waits for it.
            PageId::Account => {
                self.probe().is_some() && self.account_checks().all_valid() && !self.is_checking()
            }
            // The review page has its own button (`install`), not Continue.
            PageId::Review
            | PageId::Installing
            | PageId::Recovery
            | PageId::Done
            | PageId::Problem => false,
        }
    }

    /// The label of the page's Continue button.
    pub fn continue_label(&self) -> &'static str {
        if self.is_checking() {
            messages::CHECKING_LABEL
        } else {
            messages::CONTINUE_LABEL
        }
    }

    /// Continue was pressed.
    pub fn advance(&mut self) -> Step {
        if !self.can_continue() {
            return Step::Stay;
        }
        match self.page {
            PageId::Welcome | PageId::Machine | PageId::Disk | PageId::Encryption => {
                if self.page == PageId::Welcome {
                    self.start_notice = None;
                }
                if self.page == PageId::Disk {
                    // Continuing is accepting the disk that is selected,
                    // whether the person picked it or it was preselected: a
                    // later look at the computer must not swap it silently.
                    self.disk_chosen = self.disk.is_some();
                    self.disk_notice = None;
                }
                let next = self.page.next().unwrap_or(self.page);
                self.go(next)
            }
            PageId::Account => {
                if self.stale_probe {
                    self.look_again_then_check()
                } else {
                    self.run_plan()
                }
            }
            PageId::Review
            | PageId::Installing
            | PageId::Recovery
            | PageId::Done
            | PageId::Problem => Step::Stay,
        }
    }

    /// The review page's Try again: look at this computer again, then check
    /// the choices. The earlier look is what a refusal such as "that disk is no
    /// longer there" was about, so repeating the check alone could never mend
    /// it.
    pub fn retry(&mut self) -> Step {
        if !self.can_retry() {
            return Step::Stay;
        }
        if self.attach_failed {
            // The helper could not say where an installation is (see
            // `start_probe`): that is asked first, and the check follows its
            // answer, as the look at the computer does.
            self.check = PlanCheck::Running;
            self.resume_check = true;
            return self.attach_again();
        }
        self.look_again_then_check()
    }

    /// May the closing page offer Try again? Not while the helper is asked, and
    /// not when only a restart can help (a BIOS boot stays a BIOS boot).
    pub fn can_retry(&self) -> bool {
        self.page == PageId::Review
            && !self.is_frozen()
            && !matches!(&self.probe, ProbeState::Ready(probe) if !probe.firmware.uefi)
    }

    fn look_again_then_check(&mut self) -> Step {
        self.check = PlanCheck::Running;
        self.resume_check = true;
        self.begin_probe()
    }

    fn run_plan(&mut self) -> Step {
        self.name_refusal = None;
        if let Some(problem) = self.blocking_problem() {
            return self.conclude(PlanCheck::Failed(problem));
        }
        let Some(probe) = self.probe() else {
            return self.conclude(PlanCheck::Failed(Message::title(
                "The installer has not looked at this computer yet",
            )));
        };
        if usable_disks(probe).is_empty() {
            let reasons = disk_report(probe);
            self.stale_probe = true;
            return self.conclude(PlanCheck::NoDisk(reasons));
        }
        match self.plan_request() {
            Ok(request) => {
                self.plan_generation += 1;
                self.check = PlanCheck::Running;
                Step::RunPlan {
                    generation: self.plan_generation,
                    request: Box::new(request),
                }
            }
            Err(message) => self.conclude(PlanCheck::Failed(message)),
        }
    }

    /// End the check with this result, on the closing page.
    fn conclude(&mut self, check: PlanCheck) -> Step {
        self.check = check;
        self.go(PageId::Review)
    }

    /// The helper's answer to the latest [`Step::RunPlan`]. A refusal of the
    /// administrator's name goes back to the account page, beside the name;
    /// anything else is reported on the closing page. An answer to an earlier
    /// request, or one that arrives when no check is running, changes
    /// nothing. The step is `Goto` whenever the page changed.
    pub fn plan_finished(&mut self, generation: u64, result: Result<Plan, ClientError>) -> Step {
        if !self.plan_is_current(generation) {
            self.say(markers::stale("plan"));
            return Step::Stay;
        }
        self.say(markers::plan(&result));
        match result {
            Ok(plan) => {
                self.stale_probe = false;
                self.conclude(PlanCheck::Checked(Box::new(plan)))
            }
            // An installation began without this interface noticing (it was
            // restarted, or its answer was lost): ask where it is.
            Err(ClientError::Refused(refusal)) if refusal.code == ErrorCode::InstallationActive => {
                self.check = PlanCheck::NotRun;
                self.attach_again()
            }
            Err(ClientError::Refused(refusal))
                if refusal.field.as_deref() == Some("administrator")
                    || refusal.code == ErrorCode::ReservedName =>
            {
                self.check = PlanCheck::NotRun;
                self.name_refusal = Some(messages::plan_error(&refusal));
                self.go(PageId::Account)
            }
            Err(error) => {
                // The look at the computer may be what is out of date.
                self.stale_probe = true;
                self.conclude(PlanCheck::Failed(error.message()))
            }
        }
    }

    /// Is this the answer to the latest check, which is still waiting for it?
    pub fn plan_is_current(&self, generation: u64) -> bool {
        generation == self.plan_generation && self.is_checking() && !self.resume_check
    }

    pub fn plan_check(&self) -> &PlanCheck {
        &self.check
    }

    /// Is the helper being asked about the choices? Nothing the plan is made
    /// of can change meanwhile, and the person cannot go back.
    pub fn is_checking(&self) -> bool {
        matches!(self.check, PlanCheck::Running)
    }

    /// Nothing the plan is made of may change: the helper is being asked about
    /// it, the installation has been confirmed, or `State` is being asked where
    /// an installation is (its answer may move the flow to the installing page,
    /// so no look at the computer or check may start beside it).
    pub fn is_frozen(&self) -> bool {
        self.is_checking()
            || self.inst.phase != installation::Phase::Idle
            || self.inst.is_attaching()
    }

    /// What the review page says when the check did not end in a plan.
    pub fn check_problem(&self) -> Option<Message> {
        match &self.check {
            PlanCheck::NoDisk(reasons) => Some(messages::no_disk(reasons)),
            PlanCheck::Failed(message) => Some(message.clone()),
            _ => None,
        }
    }

    /// The word the closing page's serial marker carries (`check=…`).
    pub fn check_word(&self) -> &'static str {
        match &self.check {
            PlanCheck::NotRun => "not-run",
            PlanCheck::Running => "running",
            PlanCheck::Checked(_) => "ok",
            PlanCheck::NoDisk(_) => "no-disk",
            PlanCheck::Failed(_) => "failed",
        }
    }

    /// A change to what the plan is made of: an earlier result no longer
    /// describes the choices. (A running check is left to finish: the
    /// setters refuse while one runs.)
    fn invalidate_check(&mut self) {
        if !self.is_checking() {
            self.check = PlanCheck::NotRun;
        }
    }

    /// The request for `Plan`: the choices so far, on the disk chosen on the
    /// disk page, whole-disk, encrypted unless the person turned that off.
    /// Unset fields keep the model's defaults. (The passphrase is not in it:
    /// it goes to the helper as a secret.)
    pub fn plan_request(&self) -> Result<PlanRequest, Message> {
        let probe = self
            .probe()
            .ok_or_else(|| Message::title("The installer has not looked at this computer yet"))?;
        let tag = self
            .machine_tag()
            .ok_or_else(|| Message::title("No configuration is chosen"))?;
        // The disk the person chose, which must still be one that can be
        // installed to.
        let disk = self
            .disk
            .as_deref()
            .and_then(|id| usable_disks(probe).into_iter().find(|d| d.id == id))
            .ok_or_else(|| Message::title("No disk can be installed to"))?;
        let mut document = json!({
            "composition": tag,
            "disk": disk.id,
            "mode": "whole-disk",
            "encrypt": self.encrypt,
            "keyboard": self.keyboard.requested,
            "language": self.language,
            "administrator": self.administrator,
        });
        if self.encrypt {
            // An interface shows the recovery key (the answers file cannot), so
            // an encrypted installation of this interface has one; the security
            // chip is enrolled on a computer that can, unless turned off.
            document["recovery_key"] = json!(true);
            document["tpm"] = json!(if self.chip { "auto" } else { "off" });
        }
        PlanRequest::parse(&document.to_string()).map_err(|e| messages::plan_error(&e))
    }

    // --- disk ----------------------------------------------------------------

    /// What the disk page lists, once the computer has been looked at.
    pub fn disk_view(&self) -> Option<&DiskView> {
        self.disk_view.as_ref()
    }

    /// The disk chosen (its id): the person's, else the first that can be
    /// installed to.
    pub fn disk(&self) -> Option<&str> {
        self.disk.as_deref()
    }

    pub fn disk_choice(&self) -> Option<&DiskChoice> {
        self.disk_view.as_ref()?.choice(self.disk.as_deref()?)
    }

    /// `false` for a disk that is not offered, and while the helper is being
    /// asked. Choosing the disk that is chosen already changes nothing.
    pub fn set_disk(&mut self, id: &str) -> bool {
        if self.is_frozen()
            || !self
                .disk_view
                .as_ref()
                .and_then(|v| v.choice(id))
                .is_some_and(DiskChoice::is_offered)
        {
            return false;
        }
        if self.disk.as_deref() != Some(id) {
            // (A toolkit that echoes the selection back is not choosing: the
            // notice stays until a disk is picked or Continue is pressed.)
            self.disk_notice = None;
            self.invalidate_check();
            self.disk = Some(id.to_owned());
            self.disk_chosen = true;
            self.say(markers::disk(id));
        }
        true
    }

    /// What the welcome page says after an installation that this interface was
    /// waiting to see begin did not (nothing was written): said until
    /// Continue.
    pub fn start_notice(&self) -> Option<Message> {
        self.start_notice.clone()
    }

    /// Why the disk page is showing again: the disk the person chose is gone
    /// (said until a disk is chosen).
    pub fn disk_notice(&self) -> Option<Message> {
        self.disk_notice.clone()
    }

    /// What the disk page says when no disk can be installed to: every disk's
    /// reason, then what to do.
    pub fn disk_problem(&self) -> Option<Message> {
        let probe = self.probe()?;
        usable_disks(probe)
            .is_empty()
            .then(|| messages::no_disk(&disk_report(probe)))
    }

    /// May the disk page offer to look again? Nothing can be installed to and
    /// the helper is not being asked: a disk plugged in since appears.
    pub fn can_look_again(&self) -> bool {
        self.page == PageId::Disk
            && !self.is_frozen()
            && self.probe_in_flight.is_none()
            && self
                .disk_view
                .as_ref()
                .is_some_and(|v| v.offered().next().is_none())
    }

    // --- welcome -------------------------------------------------------------

    pub fn languages(&self) -> &'static [Language] {
        language::LANGUAGES
    }

    pub fn language(&self) -> &str {
        &self.language
    }

    /// `false` for a locale the catalogue does not offer, and while the helper
    /// is being asked.
    pub fn set_language(&mut self, locale: &str) -> bool {
        if self.is_frozen() || language::find(locale).is_none() {
            return false;
        }
        if self.language != locale {
            self.language = locale.to_owned();
            self.invalidate_check();
            self.say(markers::language(locale));
        }
        true
    }

    pub fn layouts(&self) -> &Layouts {
        &self.layouts
    }

    /// The layout the person asked for (the session may still be on another:
    /// see [`Flow::keyboard_status`]).
    pub fn keyboard(&self) -> &str {
        &self.keyboard.requested
    }

    pub fn keyboard_layout(&self) -> Option<&Layout> {
        self.layouts.get(&self.keyboard.requested)
    }

    pub fn keyboard_status(&self) -> KeyboardStatus {
        let k = &self.keyboard;
        if let Some(failure) = &k.failure {
            KeyboardStatus::Failed(failure.clone())
        } else if k.in_flight.is_some() || k.applied.as_deref() != Some(k.requested.as_str()) {
            KeyboardStatus::Switching
        } else {
            KeyboardStatus::Applied
        }
    }

    /// Choose a layout by console keymap name. The step asks the interface to
    /// switch the session to it (`Step::SwitchKeyboard`), unless the session
    /// is already there or a switch is under way: that one finishes first and
    /// the latest choice is asked for next, so the compositor ends on what the
    /// person chose last whatever order the answers come in. Continue stays
    /// off until the compositor has confirmed it.
    pub fn set_keyboard(&mut self, id: &str) -> Step {
        if self.is_frozen() || self.layouts.get(id).is_none() {
            return Step::Stay;
        }
        // The layout that was asked for already: a toolkit that echoes its
        // selection back is not choosing. Nothing changes, so a failure that is
        // shown stays until Try again (`retry_keyboard`) and no switch starts.
        if self.keyboard.requested == id {
            return Step::Stay;
        }
        // What was typed in the old layout holds that layout's characters,
        // and the installed computer asks for it in the new one (the
        // passphrase at the start-up prompt, the password at the login
        // screen): a match would no longer unlock the disk. The fields are
        // emptied, so they are typed again in the layout that will be used.
        if !(self.password.is_empty()
            && self.confirmation.is_empty()
            && self.passphrase.is_empty()
            && self.passphrase_confirmation.is_empty())
        {
            self.wipe_secrets();
        }
        self.invalidate_check();
        self.keyboard.requested = id.to_owned();
        self.keyboard.failure = None;
        self.pump_keyboard()
    }

    /// Try the layout again after a failure.
    pub fn retry_keyboard(&mut self) -> Step {
        self.keyboard.failure = None;
        self.pump_keyboard()
    }

    /// The compositor's answer to the latest [`Step::SwitchKeyboard`]; `Err`
    /// carries the reason, for the page.
    pub fn keyboard_switched(&mut self, generation: u64, result: Result<(), String>) -> Step {
        let Some((current, layout)) = self.keyboard.in_flight.clone() else {
            self.say(markers::stale("layout"));
            return Step::Stay;
        };
        if current != generation {
            self.say(markers::stale("layout"));
            return Step::Stay;
        }
        self.keyboard.in_flight = None;
        self.say(markers::layout(&layout, result.is_ok()));
        match result {
            Ok(()) => self.keyboard.applied = Some(layout),
            Err(why) => {
                // What the session types is unknown now. A failure of a
                // layout the person has since replaced is not shown: the
                // newer choice is asked for next.
                self.keyboard.applied = None;
                if layout == self.keyboard.requested {
                    self.keyboard.failure = Some(messages::keyboard_switch_failed(&why));
                }
            }
        }
        self.pump_keyboard()
    }

    /// Is this the answer to the latest layout switch?
    pub fn keyboard_switch_is_current(&self, generation: u64) -> bool {
        self.keyboard
            .in_flight
            .as_ref()
            .is_some_and(|(current, _)| *current == generation)
    }

    /// Start the switch to the requested layout when none is under way, none
    /// has just failed and the compositor is not on it already.
    fn pump_keyboard(&mut self) -> Step {
        let k = &mut self.keyboard;
        if k.in_flight.is_some()
            || k.failure.is_some()
            || k.applied.as_deref() == Some(k.requested.as_str())
        {
            return Step::Stay;
        }
        let Some(layout) = self.layouts.get(&k.requested).cloned() else {
            return Step::Stay;
        };
        k.generation += 1;
        k.in_flight = Some((k.generation, k.requested.clone()));
        Step::SwitchKeyboard {
            generation: k.generation,
            layout,
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

    /// `false` for a tag this edition does not offer, and while the helper is
    /// being asked. Choosing what is chosen already changes nothing (a toolkit
    /// that echoes the selection back is not choosing). A change queues the
    /// `ILEX-COMPOSITION` line of the new choice.
    pub fn select_machine(&mut self, tag: &str) -> bool {
        if self.is_frozen() || !self.view.as_ref().is_some_and(|v| v.choice(tag).is_some()) {
            return false;
        }
        if self.machine.as_deref() != Some(tag) {
            self.invalidate_check();
            self.machine = Some(tag.to_owned());
            self.machine_chosen = true;
            if let Some(line) = self.composition_line() {
                self.say(line);
            }
        }
        true
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

    // --- encryption ----------------------------------------------------------

    /// Is the disk to be encrypted? (On until the person turns it off.)
    pub fn encrypt(&self) -> bool {
        self.encrypt
    }

    /// Refused (`false`) while the helper is being asked. Turning encryption
    /// off keeps what was typed (turning it on again finds it); it is wiped
    /// with the other secrets.
    pub fn set_encrypt(&mut self, on: bool) -> bool {
        if self.is_frozen() {
            return false;
        }
        if self.encrypt != on {
            self.encrypt = on;
            self.invalidate_check();
            self.say(markers::encryption(on));
        }
        true
    }

    /// Whether this computer can unlock the disk with its security chip, as
    /// the helper's probe says (`ilex-enroll`'s verdict), and if not, its
    /// reason code.
    pub fn chip_eligible(&self) -> Option<Result<(), String>> {
        let enrollment = &self.probe()?.enrollment;
        Some(if enrollment["verdict"] == "eligible" {
            Ok(())
        } else {
            Err(enrollment["reason"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned())
        })
    }

    /// The security chip's row on the encryption page.
    pub fn chip_view(&self) -> recovery::ChipView {
        match (self.encrypt, self.chip_eligible()) {
            (false, _) => recovery::ChipView {
                sensitive: false,
                on: false,
                line: recovery::CHIP_NEEDS_ENCRYPTION.to_owned(),
            },
            (true, Some(Ok(()))) => recovery::ChipView {
                sensitive: !self.is_frozen(),
                on: self.chip,
                line: recovery::CHIP_ON_LINE.to_owned(),
            },
            (true, Some(Err(reason))) => recovery::ChipView {
                sensitive: false,
                on: false,
                line: recovery::chip_unavailable_line(&reason),
            },
            (true, None) => recovery::ChipView {
                sensitive: false,
                on: false,
                line: "Looking at this computer…".to_owned(),
            },
        }
    }

    /// Refused (`false`) while the helper is being asked, or where the switch
    /// is not offered.
    pub fn set_chip(&mut self, on: bool) -> bool {
        if self.is_frozen() || !self.chip_view().sensitive {
            return false;
        }
        if self.chip != on {
            self.chip = on;
            self.invalidate_check();
            self.say(markers::chip(on));
        }
        true
    }

    /// Refused (`false`) while the helper is being asked. The secret is moved
    /// in; the one it replaces is wiped.
    pub fn set_passphrase(&mut self, secret: Secret) -> bool {
        if self.is_frozen() {
            return false;
        }
        if self.passphrase != secret {
            self.passphrase = secret;
            self.invalidate_check();
        }
        true
    }

    /// Refused (`false`) while the helper is being asked.
    pub fn set_passphrase_confirmation(&mut self, secret: Secret) -> bool {
        if self.is_frozen() {
            return false;
        }
        if self.passphrase_confirmation != secret {
            self.passphrase_confirmation = secret;
            self.invalidate_check();
        }
        true
    }

    /// The passphrase, for the helper's `SetSecret`. It never goes into a
    /// request, a marker or a log.
    pub fn passphrase(&self) -> &Secret {
        &self.passphrase
    }

    pub fn passphrase_confirmation(&self) -> &Secret {
        &self.passphrase_confirmation
    }

    pub fn encryption_checks(&self) -> EncryptionChecks {
        EncryptionChecks {
            passphrase: encryption::passphrase(&self.passphrase),
            confirmation: encryption::confirmation(&self.passphrase, &self.passphrase_confirmation),
        }
    }

    pub fn passphrase_advice(&self) -> Option<Message> {
        encryption::advice(&self.passphrase)
    }

    /// What the page says about the layout the passphrase is typed in.
    pub fn passphrase_layout_line(&self) -> String {
        encryption::layout_line(self.keyboard_layout())
    }

    // --- account -------------------------------------------------------------

    pub fn administrator(&self) -> &str {
        &self.administrator
    }

    /// Refused (`false`) while the helper is being asked.
    pub fn set_administrator(&mut self, name: &str) -> bool {
        if self.is_frozen() {
            return false;
        }
        if self.administrator != name {
            self.administrator = name.to_owned();
            self.name_refusal = None;
            self.invalidate_check();
        }
        true
    }

    /// Refused (`false`) while the helper is being asked. The secret is moved
    /// in; the one it replaces is wiped.
    pub fn set_password(&mut self, secret: Secret) -> bool {
        if self.is_frozen() {
            return false;
        }
        if self.password != secret {
            self.password = secret;
            self.invalidate_check();
        }
        true
    }

    /// Refused (`false`) while the helper is being asked.
    pub fn set_confirmation(&mut self, secret: Secret) -> bool {
        if self.is_frozen() {
            return false;
        }
        if self.confirmation != secret {
            self.confirmation = secret;
            self.invalidate_check();
        }
        true
    }

    /// The password, for the helper's `SetSecret` (a later change). It never
    /// goes into a request, a marker or a log.
    pub fn password(&self) -> &Secret {
        &self.password
    }

    pub fn confirmation(&self) -> &Secret {
        &self.confirmation
    }

    /// Wipe the flow's own copy of the passwords. An interface clears the
    /// fields it holds them in when [`Flow::secrets_wiped`] changes. Also
    /// when the keyboard layout changes after they were typed.
    fn wipe_secrets(&mut self) {
        self.password = Secret::default();
        self.confirmation = Secret::default();
        self.passphrase = Secret::default();
        self.passphrase_confirmation = Secret::default();
        self.inst.clear_recovery();
        self.secrets_wiped += 1;
    }

    /// How many times the flow has wiped the passwords and the passphrase it
    /// holds (after an installation succeeded, or when the computer is about
    /// to restart).
    pub fn secrets_wiped(&self) -> u64 {
        self.secrets_wiped
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
            PageId::Welcome => format!(
                "language={} keyboard={}{}",
                self.language,
                self.keyboard.requested,
                if self.start_notice.is_some() {
                    " notice=not-started"
                } else {
                    ""
                }
            ),
            PageId::Machine => match (&self.view, self.probe()) {
                (Some(view), Some(probe)) => format!(
                    "selected={} origin={} detected={} offered={} choices={}",
                    self.machine.as_deref().unwrap_or("none"),
                    view.origin.word(),
                    probe.detection.tag,
                    view.choices.len(),
                    view.choices
                        .iter()
                        .map(|c| c.tag.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                _ => "selected=none origin=pending".to_owned(),
            },
            PageId::Disk => match &self.disk_view {
                Some(view) => format!(
                    "disk={} offered={} unavailable={}",
                    markers::one_word(self.disk.as_deref().unwrap_or("none")),
                    view.offered().count(),
                    view.choices.len() - view.offered().count()
                ),
                None => "disk=none offered=pending".to_owned(),
            },
            PageId::Encryption => format!(
                "encryption={} keyboard={} chip={}",
                if self.encrypt { "on" } else { "off" },
                markers::one_word(&self.keyboard.requested),
                match (self.encrypt, self.chip_eligible()) {
                    (true, Some(Ok(()))) if self.chip => "on",
                    (true, Some(Ok(()))) => "off",
                    (true, Some(Err(_))) => "unavailable",
                    _ => "none",
                }
            ),
            PageId::Account => format!("administrator={}", markers::one_word(&self.administrator)),
            PageId::Review => format!("check={}", self.check_word()),
            PageId::Installing | PageId::Recovery | PageId::Done | PageId::Problem => {
                self.installation_fields()
            }
        }
    }

    pub fn default_keyboard_id() -> &'static str {
        keyboard::DEFAULT
    }
}

/// The disks the installer could install to now: not the medium it started
/// from, not in use by the running system and big enough (the helper's own
/// minimum). The disk page lists them for the person.
pub fn usable_disks(probe: &Probe) -> Vec<&Disk> {
    probe
        .disks
        .iter()
        .filter(|d| disks::unavailable_reason(d).is_none())
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
        .filter_map(|d| {
            disks::unavailable_reason(d).map(|why| {
                format!(
                    "{} ({}): {why}.",
                    disks::title_of(d),
                    human_bytes(d.size_bytes)
                )
            })
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
