//! `State()`: where an installation is, for a restarted interface to re-attach.
//! The helper keeps the same document at `/run/ilex-installer/state.json`.

use crate::plan::PlanError;
use serde::{Deserialize, Serialize};

pub const STATE_SCHEMA: u32 = 1;

/// How many recent lines of what the installation's tools printed `State`
/// carries (the full output is in the stage logs).
pub const LOG_TAIL_LINES: usize = 40;

/// The stages of an installation, in order, after `Idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Idle,
    Partition,
    Encrypt,
    Mount,
    Image,
    Configure,
    Account,
    Recovery,
    Tpm,
    Finalise,
    Done,
}

impl Stage {
    /// The name the logs, the state file and the stage log files use.
    pub fn name(self) -> &'static str {
        match self {
            Stage::Idle => "idle",
            Stage::Partition => "partition",
            Stage::Encrypt => "encrypt",
            Stage::Mount => "mount",
            Stage::Image => "image",
            Stage::Configure => "configure",
            Stage::Account => "account",
            Stage::Recovery => "recovery",
            Stage::Tpm => "tpm",
            Stage::Finalise => "finalise",
            Stage::Done => "done",
        }
    }

    /// The stages an unencrypted whole-disk installation goes through, in order.
    pub const UNENCRYPTED: [Stage; 6] = [
        Stage::Partition,
        Stage::Mount,
        Stage::Image,
        Stage::Configure,
        Stage::Account,
        Stage::Finalise,
    ];
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Outcome {
    Success,
    Failed { stage: Stage, reason: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    /// The stage does not apply to this installation (no encryption).
    Skipped,
    Failed,
}

/// One line of the progress list: a stage, when it began and ended, and how.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageRecord {
    pub stage: Stage,
    pub status: StageStatus,
    pub started: Option<u64>,
    pub finished: Option<u64>,
}

/// What the running or last installation installs, taken from its plan when
/// `Apply` accepted it: what an interface that restarted says on its closing
/// pages without having seen the plan, and which disk is the installation's
/// (the one a log is never written to).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// The edition's name for headings (`ilex Glabra`).
    pub edition: String,
    /// The composition's tag (`generic.gnome`).
    pub composition: String,
    /// The image's version label, when the registry gave one.
    pub version: Option<String>,
    /// The manifest digest that was installed.
    pub digest: String,
    /// The target disk's stable id.
    pub disk: String,
    /// How a person names it: model and serial number.
    pub disk_label: String,
}

/// An `Apply` the helper took the work lock for and then refused (the plan
/// expired, the disk changed, the signature, the disk in use, an internal
/// error): nothing was written. The helper keeps the latest one so that an
/// interface that was restarted while it checked can say why its installation
/// did not start, with the plan's id to tell it from an older one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refusal {
    /// The plan that was applied.
    pub plan: String,
    /// What the helper answered.
    pub error: PlanError,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub schema: u32,
    pub stage: Stage,
    /// The latest plan the helper made, if any.
    pub plan: Option<String>,
    /// 0.0 to 1.0 within the current stage.
    pub progress: f64,
    pub message: String,
    pub outcome: Option<Outcome>,
    pub updated: u64,
    /// An installation is running now (it has an `Apply` and no outcome yet).
    #[serde(default)]
    pub active: bool,
    /// The plan the running or last installation was applied from.
    #[serde(default)]
    pub applied: Option<String>,
    /// The stages of the running or last installation, with their outcomes:
    /// the list a progress page shows.
    #[serde(default)]
    pub stages: Vec<StageRecord>,
    /// The helper has written to the disk since the last `Apply` began: a
    /// failure from here on leaves it changed.
    #[serde(default)]
    pub disk_changed: bool,
    /// The latest lines of what the tools of the current or last stage printed
    /// (never a secret: none is passed where it could be printed).
    #[serde(default)]
    pub log: Vec<String>,
    /// When `Apply` began.
    #[serde(default)]
    pub started: Option<u64>,
    /// What the running or last installation installs, and where.
    #[serde(default)]
    pub summary: Option<Summary>,
    /// The plan an `Apply` is checking right now: the helper took the work
    /// lock for it and has not yet begun, or refused. For that window (the
    /// image's signature is checked over the network, the disk is probed and
    /// claimed) every other field still describes the *previous*
    /// installation, so an interface that asks must treat this as an
    /// installation under way, not read the rest. It is set when `Apply`
    /// takes the lock and cleared when it refuses or begins; the file a
    /// crashed helper leaves it in is read as not set.
    #[serde(default)]
    pub starting: Option<String>,
    /// The latest `Apply` that ended in a refusal, over whatever else the
    /// record holds (it is cleared when an `Apply` takes the lock again). An
    /// interface that waited for the plan in `starting` and finds it here, or
    /// finds a record that is not that plan's, knows nothing was written.
    #[serde(default)]
    pub refusal: Option<Refusal>,
}

impl State {
    pub fn idle(updated: u64) -> Self {
        Self {
            schema: STATE_SCHEMA,
            stage: Stage::Idle,
            plan: None,
            progress: 0.0,
            message: String::new(),
            outcome: None,
            updated,
            active: false,
            applied: None,
            stages: Vec::new(),
            disk_changed: false,
            log: Vec::new(),
            started: None,
            summary: None,
            starting: None,
            refusal: None,
        }
    }

    /// An installation is under way or being started: what an interface that
    /// restarted shows the installing page for, and keeps asking about.
    pub fn busy(&self) -> bool {
        self.active || self.starting.is_some()
    }

    /// The installation this state describes has ended (one that was applied
    /// and has an outcome).
    pub fn ended(&self) -> bool {
        self.applied.is_some() && self.outcome.is_some()
    }
}
