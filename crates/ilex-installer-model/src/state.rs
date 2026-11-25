//! `State()`: where an installation is, for a restarted interface to re-attach.

use serde::{Deserialize, Serialize};

pub const STATE_SCHEMA: u32 = 1;

/// The stages of an installation, in order, after `Idle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "kebab-case")]
pub enum Outcome {
    Success,
    Failed { stage: Stage, reason: String },
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
        }
    }
}
