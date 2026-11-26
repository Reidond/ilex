//! `State()`: where an installation is, for a restarted interface to re-attach.
//! The helper keeps the same document at `/run/ilex-installer/state.json`.

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
        }
    }
}
