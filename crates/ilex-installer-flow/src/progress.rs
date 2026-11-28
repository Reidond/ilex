//! The installation's progress as an interface shows it: the stages the plan
//! will go through, which one is running, how far it is, and how it ended.
//! The helper reports a [`State`] (`State()` and the `Progress` signal); this
//! turns it into rows. Which stages exist depends on the plan: an unencrypted
//! installation has no encryption stage (and none has a recovery key or TPM
//! stage yet).

use ilex_installer_model::plan::Plan;
use ilex_installer_model::state::{Outcome, Stage, StageStatus, State};
use serde::Serialize;

/// Every stage, in the order the helper runs them.
pub const ORDER: [Stage; 9] = [
    Stage::Partition,
    Stage::Encrypt,
    Stage::Mount,
    Stage::Image,
    Stage::Configure,
    Stage::Account,
    Stage::Recovery,
    Stage::Tpm,
    Stage::Finalise,
];

/// What the person reads for a stage, in the progress list.
pub fn label(stage: Stage) -> &'static str {
    match stage {
        Stage::Idle => "Waiting to start",
        Stage::Partition => "Preparing the disk",
        Stage::Encrypt => "Encrypting the disk",
        Stage::Mount => "Mounting the new system",
        Stage::Image => "Downloading and installing ilex",
        Stage::Configure => "Configuring the system",
        Stage::Account => "Creating the administrator",
        Stage::Recovery => "Confirming the recovery key",
        Stage::Tpm => "Setting up automatic unlock",
        Stage::Finalise => "Finishing",
        Stage::Done => "Done",
    }
}

/// How much of the whole each stage stands for in the overall bar: the image
/// is by far the longest (a download of gigabytes).
fn weight(stage: Stage) -> f64 {
    match stage {
        Stage::Partition => 3.0,
        Stage::Encrypt => 3.0,
        Stage::Mount => 1.0,
        Stage::Image => 70.0,
        Stage::Configure => 5.0,
        Stage::Account => 3.0,
        Stage::Recovery => 2.0,
        Stage::Tpm => 3.0,
        Stage::Finalise => 6.0,
        Stage::Idle | Stage::Done => 0.0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Pending,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Row {
    pub stage: Stage,
    pub label: &'static str,
    pub status: Status,
    /// 0.0 to 1.0 within the stage; 1.0 once it is done.
    pub fraction: f64,
}

/// The progress page's model.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    pub rows: Vec<Row>,
    /// The helper's last line (`downloading layer 12 of 61`).
    pub message: String,
    pub outcome: Option<Outcome>,
}

impl Progress {
    /// The stages a plan goes through.
    pub fn for_plan(plan: &Plan) -> Self {
        let encrypted = plan.encryption.enabled;
        let rows = ORDER
            .iter()
            .copied()
            .filter(|stage| match stage {
                Stage::Encrypt => encrypted,
                Stage::Recovery => encrypted && plan.encryption.recovery_key,
                Stage::Tpm => {
                    encrypted && plan.encryption.recovery_key && plan.encryption.tpm.will_enroll
                }
                _ => true,
            })
            .map(|stage| Row {
                stage,
                label: label(stage),
                status: Status::Pending,
                fraction: 0.0,
            })
            .collect();
        Self {
            rows,
            message: String::new(),
            outcome: None,
        }
    }

    /// The stages of an installation that is being started and has no record
    /// yet (the helper is checking the disk and the image): all of them wait.
    /// The helper only installs without encryption so far; a later change
    /// takes the stages from what the helper reports.
    pub fn starting() -> Self {
        Self {
            rows: Stage::UNENCRYPTED
                .iter()
                .map(|&stage| Row {
                    stage,
                    label: label(stage),
                    status: Status::Pending,
                    fraction: 0.0,
                })
                .collect(),
            message: String::new(),
            outcome: None,
        }
    }

    /// The rows of an installation the interface has not seen the plan of (it
    /// restarted): the stages the helper recorded, with their outcomes.
    pub fn from_state(state: &State) -> Self {
        let mut progress = Self {
            rows: state
                .stages
                .iter()
                .filter(|record| record.status != StageStatus::Skipped)
                .map(|record| Row {
                    stage: record.stage,
                    label: label(record.stage),
                    status: Status::Pending,
                    fraction: 0.0,
                })
                .collect(),
            message: String::new(),
            outcome: None,
        };
        progress.apply(state);
        progress
    }

    /// Take the helper's state: stages before the current one are done, the
    /// current one runs with the state's fraction (or has failed), later ones
    /// wait. A state in `Done` finishes every row.
    pub fn apply(&mut self, state: &State) {
        self.message.clone_from(&state.message);
        self.outcome = state.outcome.clone();
        let current = ORDER.iter().position(|s| *s == state.stage);
        let failed = match &state.outcome {
            Some(Outcome::Failed { stage, .. }) => Some(*stage),
            _ => None,
        };
        let succeeded =
            matches!(state.outcome, Some(Outcome::Success)) || state.stage == Stage::Done;
        for row in &mut self.rows {
            let at = ORDER.iter().position(|s| *s == row.stage);
            let recorded = state
                .stages
                .iter()
                .find(|record| record.stage == row.stage)
                .map(|record| record.status);
            (row.status, row.fraction) = if succeeded {
                (Status::Done, 1.0)
            } else if failed == Some(row.stage) {
                (Status::Failed, state.progress.clamp(0.0, 1.0))
            } else if let Some(recorded) = recorded {
                // The helper's own record of the stage, when it keeps one.
                match recorded {
                    StageStatus::Done | StageStatus::Skipped => (Status::Done, 1.0),
                    StageStatus::Running => (Status::Running, state.progress.clamp(0.0, 1.0)),
                    StageStatus::Failed => (Status::Failed, state.progress.clamp(0.0, 1.0)),
                    StageStatus::Pending => (Status::Pending, 0.0),
                }
            } else {
                match (at, current) {
                    (Some(at), Some(current)) if at < current => (Status::Done, 1.0),
                    (Some(at), Some(current)) if at == current => {
                        (Status::Running, state.progress.clamp(0.0, 1.0))
                    }
                    _ => (Status::Pending, 0.0),
                }
            };
        }
    }

    /// The whole installation, 0.0 to 1.0, weighted by how long each stage
    /// takes.
    pub fn overall(&self) -> f64 {
        let total: f64 = self.rows.iter().map(|r| weight(r.stage)).sum();
        if total == 0.0 {
            return 0.0;
        }
        let done: f64 = self.rows.iter().map(|r| weight(r.stage) * r.fraction).sum();
        (done / total).clamp(0.0, 1.0)
    }

    pub fn running(&self) -> Option<&Row> {
        self.rows.iter().find(|r| r.status == Status::Running)
    }

    pub fn failed(&self) -> Option<&Row> {
        self.rows.iter().find(|r| r.status == Status::Failed)
    }
}
