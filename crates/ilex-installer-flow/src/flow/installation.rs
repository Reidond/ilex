//! The installation's side of the flow: from the review page's destructive
//! button to the page the installation ends on, and what is offered there.
//!
//! ```text
//! review page   install() ------------> Step::Confirm(dialog)
//!                 cancel_install() / confirm_install()
//!               confirm_install() -----> Step::RunApply { plan, erase_disk, password }
//!               apply_finished(..) ----> page Installing, Step::RunState { delay }
//! installing    state_finished(..) ----> Step::RunState again, until the helper's
//!                                        State has an outcome:
//!               Success --> page Done        (restart, power_off)
//!               Failed  --> page Problem     (retry_install, save_log, restart)
//! start()       the first thing an interface does: Step::RunState. An installation
//!               that is running, being started (`State.starting`: the helper took
//!               `Apply` and is checking) or that ended while the interface was away,
//!               puts the flow on its page again (re-attach); otherwise the first look
//!               at the computer begins. Until `State` has been answered once the first
//!               look waits and Try again asks `State` again; a refusal of `Probe` or
//!               `Plan` that says an installation is active asks it too.
//! ```
//!
//! **An `Apply` that is still checking counts as running.** The helper holds its
//! work lock while it checks the image's signature over the network, looks at
//! the disk and claims it, and its `State` says `starting` for that window while
//! the rest of the record is the previous installation's. The flow shows the
//! installing page for it (with every stage waiting and no log), keeps asking,
//! and a lost answer to `Apply` keeps asking while `State` names its plan.
//!
//! **What the helper recorded is what is shown.** The pages draw from `State`
//! alone (its stage list, progress, last log lines, outcome and summary), so an
//! interface that restarted shows the same page one that never stopped does. A
//! helper that does not answer `State` is waited for a few seconds (systemd
//! starts it again, and the new one reports an installation it did not end as
//! failed in the stage it was in); only a longer silence is a problem page of
//! its own.
//!
//! **Nothing is written before the person has confirmed.** `install()` only
//! asks for the dialog; `confirm_install()` is what hands the helper the password
//! and the disk's id. A refusal of `Apply` (the plan expired, the disk changed,
//! the image's signature) comes back to the review page with its sentence and
//! Try again. If the answer to `Apply` is lost, the helper is asked where things
//! stand and the flow follows what it says.

use super::{Flow, PageId, PlanCheck, ProbeState, Step};
use crate::disks;
use crate::markers;
use crate::messages::{self, ClientError, Message};
use crate::progress::Progress;
use crate::review::{Confirmation, Review, Row};
use ilex_installer_model::finish::{FinishAction, Saved, check_log_device};
use ilex_installer_model::plan::{ErrorCode, Plan};
use ilex_installer_model::probe::Probe;
use ilex_installer_model::state::{Outcome, Stage, StageStatus, State, Summary};
use std::collections::BTreeMap;

/// How often the helper is asked where the installation is.
const POLL_MS: u64 = 1000;
/// After a lost answer to `Apply` the helper is asked sooner.
const SOON_MS: u64 = 400;
/// This many `State` requests in a row without an answer and the helper is
/// taken to be gone (it is started again within a second or two when it
/// crashed, and a new one reports what it finds).
const LOST_AFTER: u32 = 8;
/// How many of the last lines the closing pages show.
const LOG_LINES: usize = 12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Nothing has been confirmed: the choices can change.
    #[default]
    Idle,
    /// The erase dialog is open.
    Confirming,
    /// `Apply` was asked and the helper has not said that it began.
    Starting,
    /// The helper is installing.
    Running,
    Succeeded,
    Failed,
    /// The helper stopped answering `State`.
    Lost,
}

/// A button a page draws: whether it is shown at all, and whether it can be
/// pressed now. An interface applies both and decides neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Control {
    pub visible: bool,
    pub enabled: bool,
}

impl Control {
    pub const HIDDEN: Self = Self {
        visible: false,
        enabled: false,
    };

    /// Shown, and pressable or not.
    pub fn shown(enabled: bool) -> Self {
        Self {
            visible: true,
            enabled,
        }
    }
}

/// The icon of a problem the review page says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemIcon {
    Error,
    /// No disk can be installed to.
    Disk,
}

/// What the review page's body is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewBody {
    /// The helper is being asked (the check) or has been given the erase (the
    /// start of the installation): a spinner with these words.
    Waiting(Message),
    /// The check did not end in a plan.
    Problem {
        message: Message,
        icon: ProblemIcon,
        /// Not when only a restart helps (a BIOS boot).
        retry: Control,
    },
    Checked(Box<Review>),
}

/// What the review page shows: its body and its destructive button.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewPage {
    pub body: ReviewBody,
    /// Visible only with a checked plan. Not disabled while its own dialog is
    /// open: the focus stays put.
    pub button: Control,
}

/// A stick the log may be written to, or why it cannot be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogChoice {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    /// Why the log cannot be written there; `None` when it can.
    pub problem: Option<Message>,
}

/// The save-the-log dialog's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LogSave {
    /// Not open.
    #[default]
    Closed,
    /// Asking the helper which sticks there are.
    Looking,
    /// The removable disks the probe found (none when nothing is plugged in).
    Choosing(Vec<LogChoice>),
    /// Writing to this device.
    Saving {
        device: String,
    },
    Saved(Saved),
    Failed(Message),
}

#[derive(Default)]
pub(super) struct Installation {
    pub(super) phase: Phase,
    /// The first `State` request, made at start-up (or again, see
    /// [`Flow::attach_again`]), is outstanding.
    attaching: bool,
    /// The toolkit's words for the first page's marker, kept until the helper
    /// has said which page that is.
    pub(super) pending_extra: Option<String>,
    /// `RunApply`'s request in flight (generation) and the plan it applies.
    apply_generation: u64,
    applying: Option<(u64, String)>,
    /// The answer to `Apply` never came: why, for the sentence that says the
    /// installation did not start.
    unanswered: Option<ClientError>,
    state_generation: u64,
    state_in_flight: Option<u64>,
    failures: u32,
    state: Option<State>,
    progress: Option<Progress>,
    summary: Option<Summary>,
    /// The plan the helper says it is checking (`State.starting`): the record
    /// in `state` is not this installation's yet, and is not shown.
    starting: Option<String>,
    /// What the problem page says when the helper has not answered for long.
    lost: Option<Message>,
    /// What the installing page says above its list while the helper is asked
    /// again.
    silent: Option<Message>,
    /// The interface found the installation under way (or ended) when it
    /// started.
    reattached: bool,
    /// Each stage's status as last announced, so that only a change is.
    stages_seen: BTreeMap<Stage, StageStatus>,
    finish_generation: u64,
    finish_in_flight: Option<(u64, FinishAction)>,
    /// Restart or power off was accepted: the machine is going down.
    finishing: Option<&'static str>,
    finish_error: Option<Message>,
    log: LogSave,
    log_probe_generation: u64,
    log_probe_in_flight: Option<u64>,
}

impl Installation {
    pub(super) fn is_attaching(&self) -> bool {
        self.attaching
    }
}

/// What the installing page shows.
#[derive(Clone, Debug, PartialEq)]
pub struct InstallingView {
    pub heading: String,
    pub rows: Vec<crate::progress::Row>,
    /// 0.0 to 1.0, weighted by how long each stage takes.
    pub overall: f64,
    /// The helper's last sentence (`Downloading and installing ilex`).
    pub message: String,
    /// The last lines of what the tools printed.
    pub log: Vec<String>,
    /// The helper has not answered for a few requests: said above the list.
    pub waiting: Option<Message>,
}

/// What the done page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DoneView {
    pub heading: String,
    pub detail: String,
    /// Edition, configuration, version.
    pub rows: Vec<Row>,
    /// Restart or Power off was accepted.
    pub finishing: Option<&'static str>,
    pub error: Option<Message>,
    pub can_finish: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProblemKind {
    /// The helper recorded that this stage failed.
    Failed { stage: Stage },
    /// The helper is not answering.
    Lost,
}

/// What the problem page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemView {
    pub kind: ProblemKind,
    pub message: Message,
    /// The last lines of what the stage's tools printed.
    pub log: Vec<String>,
    /// Try again: shown on both kinds (after silence it asks the helper again).
    pub retry: Control,
    /// Save the log: for a failure only (a helper that is not answering cannot
    /// write one).
    pub save_log: Control,
    pub restart: Control,
    pub finishing: Option<&'static str>,
    pub error: Option<Message>,
}

impl Flow {
    // --- start-up and re-attaching ------------------------------------------------

    /// The first thing an interface does. Asks the helper where an installation
    /// is, so that an interface that was restarted during one (a compositor
    /// crash, a killed session) comes back on the right page; the answer then
    /// either puts the flow on that page ([`Flow::state_finished`]) or begins the
    /// first look at the computer.
    pub fn start(&mut self) -> Step {
        if self.is_checking()
            || self.probe_in_flight.is_some()
            || self.inst.attaching
            || self.inst.state_in_flight.is_some()
        {
            return Step::Stay;
        }
        self.inst.attaching = true;
        self.begin_state(0)
    }

    /// Ask where an installation is, though the flow has begun: a refusal said
    /// one is running (or the helper's own record has changed under it). The
    /// answer puts the flow on the page it belongs to, or begins the first
    /// look at the computer again.
    pub(super) fn attach_again(&mut self) -> Step {
        if self.inst.attaching || self.inst.state_in_flight.is_some() {
            return Step::Stay;
        }
        self.inst.attaching = true;
        self.begin_state(0)
    }

    fn begin_state(&mut self, delay_ms: u64) -> Step {
        self.inst.state_generation += 1;
        self.inst.state_in_flight = Some(self.inst.state_generation);
        Step::RunState {
            generation: self.inst.state_generation,
            delay_ms,
        }
    }

    /// No first look at the computer is outstanding (the self-check's way to
    /// say that [`Flow::start`] did not begin one while it waits for the state).
    pub fn start_probe_is_idle(&self) -> bool {
        self.probe_in_flight.is_none()
    }

    pub fn phase(&self) -> Phase {
        self.inst.phase
    }

    /// The first question to the helper (where an installation is) has not been
    /// answered: the interface shows nothing of the pages yet.
    pub fn is_attaching(&self) -> bool {
        self.inst.attaching
    }

    /// Is the answer to `RunState` the one in flight?
    pub fn state_is_current(&self, generation: u64) -> bool {
        self.inst.state_in_flight == Some(generation)
    }

    /// The helper's answer to the [`Step::RunState`] in flight. One for any
    /// other request changes nothing.
    pub fn state_finished(&mut self, generation: u64, result: Result<State, ClientError>) -> Step {
        if self.inst.state_in_flight != Some(generation) {
            self.say(markers::stale("state"));
            return Step::Stay;
        }
        self.inst.state_in_flight = None;
        if result.is_ok() {
            self.attach_failed = false;
        }
        if self.inst.attaching {
            self.inst.attaching = false;
            return self.attached(result);
        }
        match self.inst.phase {
            Phase::Starting => self.after_lost_answer(result),
            Phase::Running | Phase::Lost => self.followed(result),
            _ => Step::Stay,
        }
    }

    /// The start-up request's answer.
    fn attached(&mut self, result: Result<State, ClientError>) -> Step {
        match result {
            // Under way, or `Apply` is checking: the installing page.
            Ok(state) if state.busy() => {
                self.say(markers::attach("installing"));
                self.inst.reattached = true;
                self.inst.phase = Phase::Running;
                self.inst.failures = 0;
                self.take_busy(state);
                self.go(PageId::Installing);
                self.begin_state(POLL_MS)
            }
            Ok(state) if state.ended() => {
                let word = match state.outcome {
                    Some(Outcome::Success) => "done",
                    _ => "failed",
                };
                self.say(markers::attach(word));
                self.inst.reattached = true;
                self.take_state(state);
                self.conclude_installation()
            }
            Ok(_) => {
                self.say(markers::attach("idle"));
                self.announce_page(None);
                self.begin_probe()
            }
            Err(error) => {
                // The helper may not be up yet, or is restarting: nothing is
                // known of an installation, so the first look at the computer
                // waits. The welcome page says so, and Try again asks `State`
                // again (a probe that succeeded now could hide an installation
                // whose outcome nobody has read).
                self.say(markers::attach("unavailable"));
                self.attach_failed = true;
                self.probe = ProbeState::Failed(error.message());
                self.view = None;
                self.disk_view = None;
                self.announce_page(None);
                Step::Stay
            }
        }
    }

    /// The helper says an installation is active (`active`, or `starting`
    /// while its `Apply` checks): the stage list and the log of an installation
    /// that is running, and for the checking window the stages all waiting (the
    /// record the state still holds is the previous installation's).
    fn take_busy(&mut self, state: State) {
        if state.active {
            self.inst.starting = None;
            self.take_state(state);
        } else if let Some(plan) = state.starting.clone() {
            self.inst.starting = Some(plan);
            self.inst.state = None;
            self.inst.progress = Some(Progress::starting());
        }
    }

    /// The answer to a `State` request while the installation runs (or the
    /// helper is not answering).
    fn followed(&mut self, result: Result<State, ClientError>) -> Step {
        match result {
            Ok(state) => {
                self.inst.failures = 0;
                if state.busy() {
                    let was_lost = self.inst.phase == Phase::Lost;
                    self.inst.phase = Phase::Running;
                    self.take_busy(state);
                    if was_lost {
                        self.inst.lost = None;
                        self.go(PageId::Installing);
                    }
                    self.begin_state(POLL_MS)
                } else if state.ended() {
                    self.take_state(state);
                    self.conclude_installation()
                } else {
                    // Neither running nor ended: the helper has forgotten the
                    // installation (it was replaced by one that started fresh).
                    let error = ClientError::Other(
                        "the helper reports no installation, though one was running".into(),
                    );
                    self.lose(&error)
                }
            }
            Err(error) => {
                self.inst.failures += 1;
                self.say(markers::lost(self.inst.failures));
                if self.inst.failures >= LOST_AFTER {
                    self.lose(&error)
                } else {
                    // A helper that crashed is started again within a second
                    // or two: keep asking, and say so on the page.
                    self.inst.silent = Some(messages::helper_silent(&error));
                    self.begin_state(POLL_MS)
                }
            }
        }
    }

    /// The helper has not answered long enough: the problem page for it. It is
    /// still asked, more slowly, so that it moves on by itself when the helper
    /// is back.
    fn lose(&mut self, error: &ClientError) -> Step {
        self.inst.phase = Phase::Lost;
        self.inst.lost = Some(messages::helper_lost(error));
        self.go(PageId::Problem);
        self.begin_state(POLL_MS * 2)
    }

    /// Take the helper's state: rows, the last lines, what each stage did, and
    /// the summary of what is being installed.
    fn take_state(&mut self, state: State) {
        self.inst.starting = None;
        if let Some(summary) = &state.summary {
            self.inst.summary = Some(summary.clone());
        }
        let progress = if state.stages.is_empty() {
            let mut progress = self
                .inst
                .progress
                .clone()
                .unwrap_or_else(|| Progress::from_state(&state));
            progress.apply(&state);
            progress
        } else {
            Progress::from_state(&state)
        };
        self.inst.progress = Some(progress);
        for record in &state.stages {
            if self.inst.stages_seen.get(&record.stage) != Some(&record.status) {
                self.inst.stages_seen.insert(record.stage, record.status);
                let status = match record.status {
                    StageStatus::Pending | StageStatus::Skipped => None,
                    StageStatus::Running => Some("running"),
                    StageStatus::Done => Some("done"),
                    StageStatus::Failed => Some("failed"),
                };
                if let Some(status) = status {
                    self.say(markers::stage(record.stage, status));
                }
            }
        }
        self.inst.state = Some(state);
    }

    /// The state has an outcome: the page it ends on.
    fn conclude_installation(&mut self) -> Step {
        let outcome = self.inst.state.as_ref().and_then(|s| s.outcome.clone());
        match outcome {
            Some(Outcome::Success) => {
                self.say(markers::outcome(&Outcome::Success));
                self.inst.phase = Phase::Succeeded;
                // Nothing needs the password again (Try again is for a failure):
                // it is wiped now, not when the process ends, and an interface
                // clears the fields it holds it in.
                self.wipe_secrets();
                self.go(PageId::Done)
            }
            Some(failed @ Outcome::Failed { .. }) => {
                self.say(markers::outcome(&failed));
                self.inst.phase = Phase::Failed;
                self.go(PageId::Problem)
            }
            None => Step::Stay,
        }
    }

    // --- the review page's button ---------------------------------------------------

    /// What the review page is made of, once the choices are checked.
    pub fn review(&self) -> Option<Review> {
        let super::PlanCheck::Checked(plan) = &self.check else {
            return None;
        };
        Some(Review::new(plan, self.review_choices(plan)))
    }

    /// What the review page shows: the check under way or the installation
    /// being started (a spinner and its words), the problem the check ended
    /// in, or the plan with its destructive button.
    pub fn review_page(&self) -> ReviewPage {
        let waiting = |message: Message| ReviewPage {
            body: ReviewBody::Waiting(message),
            button: Control::HIDDEN,
        };
        if self.is_starting() {
            return waiting(Message::new(
                messages::STARTING_TITLE,
                messages::STARTING_DETAIL,
            ));
        }
        match &self.check {
            PlanCheck::NotRun | PlanCheck::Running => waiting(Message::new(
                messages::CHECKING_TITLE,
                messages::CHECKING_DETAIL,
            )),
            PlanCheck::Checked(_) => match self.review() {
                Some(review) => ReviewPage {
                    body: ReviewBody::Checked(Box::new(review)),
                    // Not insensitive while its dialog is open: the focus stays
                    // put there.
                    button: Control::shown(
                        self.can_install() || self.inst.phase == Phase::Confirming,
                    ),
                },
                None => waiting(Message::new(
                    messages::CHECKING_TITLE,
                    messages::CHECKING_DETAIL,
                )),
            },
            PlanCheck::NoDisk(_) | PlanCheck::Failed(_) => ReviewPage {
                body: ReviewBody::Problem {
                    message: self.check_problem().unwrap_or_else(|| {
                        Message::title("The installer could not check the choices")
                    }),
                    icon: if matches!(self.check, PlanCheck::NoDisk(_)) {
                        ProblemIcon::Disk
                    } else {
                        ProblemIcon::Error
                    },
                    retry: if self.can_retry() {
                        Control::shown(true)
                    } else {
                        Control::HIDDEN
                    },
                },
                button: Control::HIDDEN,
            },
        }
    }

    fn review_choices(&self, plan: &Plan) -> Vec<Row> {
        vec![
            Row {
                title: "Edition",
                value: self.edition_name().to_owned(),
            },
            Row {
                title: "Configuration",
                value: self
                    .machine_choice()
                    .map_or(plan.composition.tag.clone(), |c| c.title.clone()),
            },
            Row {
                title: "Language",
                value: crate::language::find(self.language())
                    .map_or_else(|| self.language().to_owned(), |l| l.label()),
            },
            Row {
                title: "Keyboard layout",
                value: self
                    .keyboard_layout()
                    .map_or(self.keyboard(), |l| l.name.as_str())
                    .to_owned(),
            },
            Row {
                title: "Administrator",
                value: self.administrator().to_owned(),
            },
        ]
    }

    /// May the destructive button be pressed? The choices are checked, nothing
    /// is under way and the computer is one that can be installed to.
    pub fn can_install(&self) -> bool {
        self.page == PageId::Review
            && self.inst.phase == Phase::Idle
            && self.blocking_problem().is_none()
            && matches!(self.check, super::PlanCheck::Checked(_))
    }

    /// The destructive button was pressed: the erase dialog to show. Nothing is
    /// asked of the helper until [`Flow::confirm_install`].
    pub fn install(&mut self) -> Step {
        if !self.can_install() {
            return Step::Stay;
        }
        let Some(review) = self.review() else {
            return Step::Stay;
        };
        self.inst.phase = Phase::Confirming;
        self.say(markers::confirm("shown"));
        Step::Confirm(review.confirmation())
    }

    /// The dialog that is open, if one is.
    pub fn confirm_dialog(&self) -> Option<Confirmation> {
        (self.inst.phase == Phase::Confirming)
            .then(|| self.review().map(|r| r.confirmation()))
            .flatten()
    }

    /// The person dismissed the dialog: nothing happened.
    pub fn cancel_install(&mut self) {
        if self.inst.phase == Phase::Confirming {
            self.inst.phase = Phase::Idle;
            self.say(markers::confirm("cancelled"));
        }
    }

    /// The person confirmed: the helper is given the password and the disk's id.
    pub fn confirm_install(&mut self) -> Step {
        if self.inst.phase != Phase::Confirming {
            return Step::Stay;
        }
        let super::PlanCheck::Checked(plan) = &self.check else {
            self.inst.phase = Phase::Idle;
            return Step::Stay;
        };
        let (plan_id, disk) = (plan.id.clone(), plan.disk.id.clone());
        self.inst.phase = Phase::Starting;
        self.inst.unanswered = None;
        self.inst.apply_generation += 1;
        self.inst.applying = Some((self.inst.apply_generation, plan_id.clone()));
        self.say(markers::confirm("accepted"));
        Step::RunApply {
            generation: self.inst.apply_generation,
            plan: plan_id,
            erase_disk: disk,
            password: self.password().clone(),
        }
    }

    /// Is the installation being started (the helper has been asked and has
    /// not answered yet)?
    pub fn is_starting(&self) -> bool {
        self.inst.phase == Phase::Starting
    }

    /// The helper's answer to [`Step::RunApply`] (`SetSecret` then `Apply`).
    pub fn apply_finished(&mut self, generation: u64, result: Result<State, ClientError>) -> Step {
        let current = self.inst.applying.as_ref().map(|(g, _)| *g) == Some(generation)
            && self.inst.phase == Phase::Starting;
        if !current {
            self.say(markers::stale("install"));
            return Step::Stay;
        }
        match result {
            Ok(state) => {
                self.say(markers::install("started"));
                self.inst.applying = None;
                self.inst.phase = Phase::Running;
                self.inst.failures = 0;
                self.take_state(state);
                self.go(PageId::Installing);
                self.begin_state(POLL_MS)
            }
            Err(ClientError::Refused(error)) if error.code == ErrorCode::InstallationActive => {
                // The helper is installing already (an earlier `Apply` of this
                // interface whose answer was lost, or another): where it is
                // is for `State` to say.
                self.say(markers::install_refused(&error));
                self.inst.applying = None;
                self.inst.phase = Phase::Idle;
                self.attach_again()
            }
            Err(ClientError::Refused(error)) => {
                // Nothing was written. The plan may be what is out of date
                // (expired, the disk changed): Try again looks again first.
                self.say(markers::install_refused(&error));
                self.inst.applying = None;
                self.inst.phase = Phase::Idle;
                self.stale_probe = true;
                self.check = super::PlanCheck::Failed(messages::plan_error(&error));
                self.go(PageId::Review)
            }
            Err(error) => {
                // The answer was lost, or `SetSecret` failed on the way: the
                // helper may have begun. Ask it before saying anything.
                self.say(markers::install("unknown"));
                self.inst.unanswered = Some(error);
                self.inst.failures = 0;
                self.begin_state(SOON_MS)
            }
        }
    }

    /// The `State` answer after a lost answer to `Apply`: what the helper says
    /// is what happened.
    fn after_lost_answer(&mut self, result: Result<State, ClientError>) -> Step {
        let applying = self.inst.applying.as_ref().map(|(_, plan)| plan.clone());
        match result {
            // The helper took the `Apply` and is still checking (the image's
            // signature is checked over the network): it has neither begun nor
            // refused, so nothing is said yet.
            Ok(state) if state.starting.is_some() => {
                self.inst.failures = 0;
                self.begin_state(POLL_MS)
            }
            Ok(state) if state.applied == applying && (state.active || state.ended()) => {
                self.say(markers::install("started"));
                self.inst.applying = None;
                self.inst.phase = Phase::Running;
                if state.active {
                    // The installing page, with what the helper says.
                    self.take_state(state.clone());
                    self.go(PageId::Installing);
                }
                self.followed(Ok(state))
            }
            Ok(_) => {
                let cause = self
                    .inst
                    .unanswered
                    .take()
                    .unwrap_or_else(|| ClientError::Other("the helper did not answer".into()));
                self.say(markers::install("not-started"));
                self.inst.applying = None;
                self.inst.phase = Phase::Idle;
                self.stale_probe = true;
                self.check = super::PlanCheck::Failed(messages::not_started(&cause));
                self.go(PageId::Review)
            }
            Err(error) => {
                self.inst.failures += 1;
                if self.inst.failures >= LOST_AFTER {
                    self.inst.applying = None;
                    self.lose(&error)
                } else {
                    self.begin_state(POLL_MS)
                }
            }
        }
    }

    // --- what the closing pages show -------------------------------------------------

    /// What the installing page shows; `None` before the first state.
    pub fn installing_view(&self) -> Option<InstallingView> {
        let progress = self.inst.progress.as_ref()?;
        let state = self.inst.state.as_ref();
        let edition = self
            .inst
            .summary
            .as_ref()
            .map_or_else(|| self.edition_name().to_owned(), |s| s.edition.clone());
        Some(InstallingView {
            heading: format!("Installing {edition}"),
            rows: progress.rows.clone(),
            overall: progress.overall(),
            message: if self.inst.starting.is_some() {
                messages::STARTING_TITLE.to_owned()
            } else {
                state.map(|s| s.message.clone()).unwrap_or_default()
            },
            log: state.map(|s| tail(&s.log, LOG_LINES)).unwrap_or_default(),
            waiting: (self.inst.failures > 0 && self.inst.phase == Phase::Running)
                .then(|| self.inst.silent.clone())
                .flatten(),
        })
    }

    /// What the done page shows.
    pub fn done_view(&self) -> Option<DoneView> {
        if self.inst.phase != Phase::Succeeded {
            return None;
        }
        let summary = self.inst.summary.as_ref();
        let edition = summary.map_or_else(|| self.edition_name().to_owned(), |s| s.edition.clone());
        let mut rows = vec![Row {
            title: "Edition",
            value: edition.clone(),
        }];
        if let Some(summary) = summary {
            rows.push(Row {
                title: "Configuration",
                value: summary.composition.clone(),
            });
            rows.push(Row {
                title: "Version",
                value: summary
                    .version
                    .clone()
                    .unwrap_or_else(|| "not reported".to_owned()),
            });
            rows.push(Row {
                title: "Installed on",
                value: summary.disk_label.clone(),
            });
        }
        Some(DoneView {
            heading: format!("{edition} is installed"),
            detail: "Remove the installation medium, then restart. The computer starts the new system; sign in with the administrator account you made, and create your own account there when it asks.".to_owned(),
            rows,
            finishing: self.inst.finishing,
            error: self.inst.finish_error.clone(),
            can_finish: self.can_finish(),
        })
    }

    /// What the problem page shows.
    pub fn problem_view(&self) -> Option<ProblemView> {
        let state = self.inst.state.as_ref();
        let (kind, message) = match self.inst.phase {
            Phase::Failed => {
                let Some(Outcome::Failed { stage, reason }) = state.and_then(|s| s.outcome.clone())
                else {
                    return None;
                };
                let disk = self.inst.summary.as_ref().map(|s| s.disk_label.as_str());
                (
                    ProblemKind::Failed { stage },
                    messages::install_failed(
                        stage,
                        &reason,
                        state.is_some_and(|s| s.disk_changed),
                        disk,
                    ),
                )
            }
            Phase::Lost => (
                ProblemKind::Lost,
                self.inst.lost.clone().unwrap_or_else(|| {
                    Message::title("The installer lost contact with its helper")
                }),
            ),
            _ => return None,
        };
        Some(ProblemView {
            kind,
            message,
            log: state.map(|s| tail(&s.log, LOG_LINES)).unwrap_or_default(),
            retry: Control::shown(self.can_retry_install()),
            save_log: Control {
                visible: matches!(kind, ProblemKind::Failed { .. }),
                enabled: self.can_save_log(),
            },
            restart: Control::shown(self.can_finish()),
            finishing: self.inst.finishing,
            error: self.inst.finish_error.clone(),
        })
    }

    /// The words the closing pages' markers carry.
    pub(super) fn installation_fields(&self) -> String {
        let state = self.inst.state.as_ref();
        match self.page {
            PageId::Installing => {
                let stage = state.map_or("starting", |s| s.stage.name());
                let percent = self
                    .inst
                    .progress
                    .as_ref()
                    .map_or(0, |p| (p.overall() * 100.0).floor() as u32);
                format!(
                    "stage={stage} progress={percent} reattached={}",
                    self.inst.reattached
                )
            }
            PageId::Done => format!(
                "composition={} reattached={}",
                self.inst
                    .summary
                    .as_ref()
                    .map_or("unknown", |s| s.composition.as_str()),
                self.inst.reattached
            ),
            PageId::Problem => match self.problem_view() {
                Some(view) => format!(
                    "kind={} stage={} retry={} reattached={}",
                    match view.kind {
                        ProblemKind::Failed { .. } => "failed",
                        ProblemKind::Lost => "lost",
                    },
                    match view.kind {
                        ProblemKind::Failed { stage } => stage.name(),
                        ProblemKind::Lost => "unknown",
                    },
                    view.retry.enabled,
                    self.inst.reattached
                ),
                None => "kind=unknown".to_owned(),
            },
            _ => String::new(),
        }
    }

    // --- Try again --------------------------------------------------------------------

    /// May the problem page offer Try again? When the installation failed: its
    /// stages can all be run again, from the start, so long as the person
    /// confirms the erase again. When the helper is silent: asking it again.
    pub fn can_retry_install(&self) -> bool {
        matches!(self.inst.phase, Phase::Failed | Phase::Lost)
            && self.page == PageId::Problem
            && self.inst.finishing.is_none()
            && self.inst.finish_in_flight.is_none()
            && !matches!(self.inst.log, LogSave::Saving { .. })
    }

    /// The problem page's Try again. After a failure: look at the computer
    /// again, check the same choices and show the review page, where the erase
    /// is confirmed again. An interface that restarted has not the choices (the
    /// password is gone): it starts again from the welcome page. After silence:
    /// ask the helper again.
    pub fn retry_install(&mut self) -> Step {
        if !self.can_retry_install() {
            return Step::Stay;
        }
        if self.inst.phase == Phase::Lost {
            // The request that waits its turn is replaced by one that goes at
            // once: its answer, when it comes, is stale.
            self.inst.failures = 0;
            return self.begin_state(0);
        }
        let have_choices = !self.administrator.is_empty() && !self.password.is_empty();
        self.inst = Installation::default();
        if have_choices {
            self.check = super::PlanCheck::Running;
            self.resume_check = true;
            self.page = PageId::Review;
            self.announce_page(None);
            self.begin_probe()
        } else {
            self.administrator.clear();
            self.wipe_secrets();
            self.name_refusal = None;
            self.machine_chosen = false;
            self.disk_chosen = false;
            self.check = super::PlanCheck::NotRun;
            self.stale_probe = false;
            self.resume_check = false;
            self.page = PageId::Welcome;
            self.announce_page(None);
            self.begin_probe()
        }
    }

    // --- restart, power off ----------------------------------------------------------

    /// May Restart and Power off be offered? On the done page, and on the
    /// problem page, when nothing else is under way.
    pub fn can_finish(&self) -> bool {
        matches!(self.page, PageId::Done | PageId::Problem)
            && matches!(
                self.inst.phase,
                Phase::Succeeded | Phase::Failed | Phase::Lost
            )
            && self.inst.finishing.is_none()
            && self.inst.finish_in_flight.is_none()
            && !matches!(self.inst.log, LogSave::Saving { .. })
    }

    pub fn restart(&mut self) -> Step {
        self.request_finish(FinishAction::Restart)
    }

    pub fn power_off(&mut self) -> Step {
        self.request_finish(FinishAction::PowerOff)
    }

    fn request_finish(&mut self, action: FinishAction) -> Step {
        let is_log = matches!(action, FinishAction::SaveLog { .. });
        if self.inst.finish_in_flight.is_some() || (!is_log && !self.can_finish()) {
            return Step::Stay;
        }
        self.inst.finish_error = None;
        self.inst.finish_generation += 1;
        self.inst.finish_in_flight = Some((self.inst.finish_generation, action.clone()));
        Step::RunFinish {
            generation: self.inst.finish_generation,
            action,
        }
    }

    /// Is this the answer to the `Finish` request in flight?
    pub fn finish_is_current(&self, generation: u64) -> bool {
        self.inst
            .finish_in_flight
            .as_ref()
            .is_some_and(|(g, _)| *g == generation)
    }

    /// The helper's answer to [`Step::RunFinish`]: `Ok(Some(saved))` for a log.
    pub fn finish_finished(
        &mut self,
        generation: u64,
        result: Result<Option<Saved>, ClientError>,
    ) -> Step {
        let Some((current, action)) = self.inst.finish_in_flight.clone() else {
            self.say(markers::stale("finish"));
            return Step::Stay;
        };
        if current != generation {
            self.say(markers::stale("finish"));
            return Step::Stay;
        }
        self.inst.finish_in_flight = None;
        self.say(markers::finish(
            action.word(),
            &result.as_ref().map(|_| ()).map_err(Clone::clone),
        ));
        match (&action, result) {
            (FinishAction::SaveLog { .. }, Ok(Some(saved))) => {
                self.say(markers::log("saved", 0, 0));
                self.inst.log = LogSave::Saved(saved);
            }
            (FinishAction::SaveLog { .. }, Ok(None)) => {
                self.say(markers::log("failed", 0, 0));
                self.inst.log =
                    LogSave::Failed(Message::title("The helper did not say where the log went"));
            }
            (FinishAction::SaveLog { .. }, Err(error)) => {
                self.say(markers::log("failed", 0, 0));
                self.inst.log = LogSave::Failed(messages::log_failed(&error));
            }
            (action, Ok(_)) => {
                self.inst.finishing = Some(if *action == FinishAction::Restart {
                    messages::RESTARTING
                } else {
                    messages::POWERING_OFF
                });
                // The computer is going down: nothing will ask for the
                // password again (this is also the way out of a problem page).
                self.wipe_secrets();
            }
            (_, Err(error)) => self.inst.finish_error = Some(messages::finish_failed(&error)),
        }
        Step::Stay
    }

    // --- the log -----------------------------------------------------------------------

    /// May the problem page offer to save the log? The helper is there to write
    /// it, and nothing else is happening.
    pub fn can_save_log(&self) -> bool {
        self.page == PageId::Problem
            && matches!(self.inst.phase, Phase::Failed)
            && self.inst.finishing.is_none()
            && self.inst.finish_in_flight.is_none()
            && !matches!(self.inst.log, LogSave::Looking | LogSave::Saving { .. })
    }

    pub fn log_state(&self) -> &LogSave {
        &self.inst.log
    }

    /// The problem page's Save the log: ask the helper which removable disks
    /// there are, for the dialog to list.
    pub fn choose_log_device(&mut self) -> Step {
        if !self.can_save_log() {
            return Step::Stay;
        }
        self.inst.log = LogSave::Looking;
        self.inst.log_probe_generation += 1;
        self.inst.log_probe_in_flight = Some(self.inst.log_probe_generation);
        self.say(markers::log("looking", 0, 0));
        Step::RunLogProbe {
            generation: self.inst.log_probe_generation,
        }
    }

    /// The helper's answer to [`Step::RunLogProbe`].
    pub fn log_probe_finished(
        &mut self,
        generation: u64,
        result: Result<Probe, ClientError>,
    ) -> Step {
        if self.inst.log_probe_in_flight != Some(generation) || self.inst.log != LogSave::Looking {
            self.say(markers::stale("log"));
            return Step::Stay;
        }
        self.inst.log_probe_in_flight = None;
        match result {
            Ok(probe) => {
                let target = self.inst.summary.as_ref().map(|s| s.disk.clone());
                let choices: Vec<LogChoice> = probe
                    .disks
                    .iter()
                    .filter(|d| d.removable && !d.live_medium && Some(&d.id) != target.as_ref())
                    .map(|d| LogChoice {
                        id: d.id.clone(),
                        title: disks::title_of(d),
                        subtitle: format!(
                            "{}{}",
                            super::human_bytes(d.size_bytes),
                            if d.serial.trim().is_empty() {
                                String::new()
                            } else {
                                format!(" · serial {}", d.serial.trim())
                            }
                        ),
                        problem: check_log_device(&probe, &d.id, target.as_deref())
                            .err()
                            .map(|e| messages::plan_error(&e)),
                    })
                    .collect();
                let usable = choices.iter().filter(|c| c.problem.is_none()).count();
                self.say(markers::log("choosing", choices.len(), usable));
                self.inst.log = LogSave::Choosing(choices);
            }
            Err(error) => {
                self.say(markers::log("failed", 0, 0));
                self.inst.log = LogSave::Failed(messages::log_failed(&error));
            }
        }
        Step::Stay
    }

    /// Write the log to this stick (an id the dialog listed as usable).
    pub fn save_log(&mut self, device: &str) -> Step {
        let usable = matches!(&self.inst.log, LogSave::Choosing(choices)
            if choices.iter().any(|c| c.id == device && c.problem.is_none()));
        if !usable {
            return Step::Stay;
        }
        self.inst.log = LogSave::Saving {
            device: device.to_owned(),
        };
        self.say(markers::log("saving", 0, 0));
        self.request_finish(FinishAction::SaveLog {
            device: device.to_owned(),
        })
    }

    /// The dialog was closed (not while the log is being written).
    pub fn close_log(&mut self) {
        if !matches!(self.inst.log, LogSave::Saving { .. }) {
            self.inst.log = LogSave::Closed;
        }
    }
}

/// The last `n` lines.
fn tail(lines: &[String], n: usize) -> Vec<String> {
    lines[lines.len().saturating_sub(n)..].to_vec()
}
