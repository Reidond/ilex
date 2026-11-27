//! Where an installation is: the `State()` record, kept in memory and at
//! `/run/ilex-installer/state.json`, and the `Progress` and `Finished` events
//! a connected interface hears. Each stage is recorded *before* it writes
//! anything, so a helper that dies leaves a record that names the stage it
//! died in; the next helper reads it, says so, and never resumes by itself.

use crate::sys;
use anyhow::{Context, Result};
use ilex_installer_model::state::{
    LOG_TAIL_LINES, Outcome, STATE_SCHEMA, Stage, StageRecord, StageStatus, State, Summary,
};
use std::{
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, mpsc::Sender},
    time::{Duration, Instant},
};

/// What the bus announces.
#[derive(Clone, Debug)]
pub enum Event {
    /// `Progress(stage, fraction, message)`: `fraction` is within the stage.
    Progress {
        stage: Stage,
        fraction: f64,
        message: String,
    },
    /// `Finished(outcome)`: the outcome as JSON.
    Finished(Outcome),
}

/// The smallest interval between two `Progress` events of the same stage.
const EMIT_EVERY: Duration = Duration::from_millis(500);

pub struct Tracker {
    run_dir: PathBuf,
    state: Mutex<State>,
    events: Mutex<Option<Sender<Event>>>,
    last_emit: Mutex<Option<Instant>>,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What a helper that starts finds in the state file left by one that did not
/// end its installation: the installation is over, failed in the stage it was
/// in, and the disk is changed if that stage had begun to write.
pub fn recover(mut state: State, now: u64) -> State {
    if state.active {
        let stage = state.stage;
        state.active = false;
        state.updated = now;
        for record in &mut state.stages {
            if record.status == StageStatus::Running {
                record.status = StageStatus::Failed;
                record.finished = Some(now);
            }
        }
        state.message = format!(
            "The installer's helper stopped during the {} stage; nothing resumes by itself.",
            stage.name()
        );
        state.outcome = Some(Outcome::Failed {
            stage,
            reason: format!(
                "the installer's helper stopped (it crashed or was stopped) during the {} stage{}",
                stage.name(),
                if state.disk_changed {
                    "; the disk was already changed, so install again from the start"
                } else {
                    "; the disk was not changed"
                }
            ),
        });
    }
    state
}

impl Tracker {
    /// Read the state file a helper before this one left, if any.
    pub fn new(run_dir: &Path) -> Self {
        let now = sys::unix_time();
        let state = fs::read(run_dir.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<State>(&bytes).ok())
            .filter(|state| state.schema == STATE_SCHEMA)
            .map_or_else(|| State::idle(now), |state| recover(state, now));
        // What this helper found is what it says from now on, also to the next
        // one (and to a reader of the file): an installation that was not
        // ended is over.
        let _ = write_state(run_dir, &state);
        Self {
            run_dir: run_dir.to_path_buf(),
            state: Mutex::new(state),
            events: Mutex::new(None),
            last_emit: Mutex::new(None),
        }
    }

    pub fn state(&self) -> State {
        locked(&self.state).clone()
    }

    pub fn active(&self) -> bool {
        locked(&self.state).active
    }

    /// The bus listens through this channel.
    pub fn set_sender(&self, sender: Sender<Event>) {
        *locked(&self.events) = Some(sender);
    }

    fn send(&self, event: Event) {
        if let Some(sender) = locked(&self.events).as_ref() {
            let _ = sender.send(event);
        }
    }

    /// Write the record to disk: a stage is not started unless this worked.
    fn persist(&self, state: &State) -> Result<()> {
        write_state(&self.run_dir, state)
    }

    /// Change the state and publish it. A file that cannot be written is
    /// reported by the caller that needs it (`start`); elsewhere the in-memory
    /// state is still right.
    fn update(&self, change: impl FnOnce(&mut State)) -> State {
        let mut state = locked(&self.state);
        change(&mut state);
        state.updated = sys::unix_time();
        let copy = state.clone();
        drop(state);
        let _ = self.persist(&copy);
        copy
    }

    pub fn set_plan(&self, id: &str) {
        self.update(|state| state.plan = Some(id.to_owned()));
    }

    /// A new installation: every stage of `stages` is pending, the previous
    /// installation's outcome and log are gone. Fails (and changes nothing)
    /// if the record cannot be written: nothing must be written to a disk
    /// without it.
    pub fn begin(&self, plan: &str, stages: &[Stage], summary: Summary) -> Result<()> {
        let mut state = locked(&self.state);
        let mut next = state.clone();
        next.active = true;
        next.applied = Some(plan.to_owned());
        next.stage = Stage::Idle;
        next.progress = 0.0;
        next.message = "Starting the installation".into();
        next.outcome = None;
        next.disk_changed = false;
        next.log.clear();
        next.started = Some(sys::unix_time());
        next.updated = sys::unix_time();
        next.summary = Some(summary);
        next.stages = stages
            .iter()
            .map(|&stage| StageRecord {
                stage,
                status: StageStatus::Pending,
                started: None,
                finished: None,
            })
            .collect();
        self.persist(&next)
            .context("recording that the installation began")?;
        *state = next;
        Ok(())
    }

    /// A stage begins. The record is written first, and a stage whose record
    /// cannot be written does not begin.
    pub fn start(&self, stage: Stage, message: &str) -> Result<()> {
        let mut state = locked(&self.state);
        let mut next = state.clone();
        next.stage = stage;
        next.progress = 0.0;
        next.message = message.to_owned();
        // What the last stage's tools printed is not this stage's.
        next.log.clear();
        next.updated = sys::unix_time();
        for record in &mut next.stages {
            if record.stage == stage {
                record.status = StageStatus::Running;
                record.started = Some(next.updated);
            }
        }
        self.persist(&next)
            .with_context(|| format!("recording the {} stage", stage.name()))?;
        *state = next;
        drop(state);
        *locked(&self.last_emit) = Some(Instant::now());
        self.send(Event::Progress {
            stage,
            fraction: 0.0,
            message: message.to_owned(),
        });
        Ok(())
    }

    /// The disk is about to be, or has been, written to. Recorded before the
    /// write: a failure from here on leaves the disk changed.
    pub fn disk_changed(&self) -> Result<()> {
        let mut state = locked(&self.state);
        let mut next = state.clone();
        next.disk_changed = true;
        next.updated = sys::unix_time();
        self.persist(&next)
            .context("recording that the disk is about to change")?;
        *state = next;
        Ok(())
    }

    /// How far the current stage is, at most every half second.
    pub fn progress(&self, fraction: f64, message: Option<&str>) {
        let fraction = fraction.clamp(0.0, 1.0);
        let stage = {
            let mut last = locked(&self.last_emit);
            if last.is_some_and(|at| at.elapsed() < EMIT_EVERY) {
                return;
            }
            *last = Some(Instant::now());
            locked(&self.state).stage
        };
        // Never backwards within a stage: the bytes in use on a file system
        // can fall (a pull's temporary files), and a bar that goes back is a
        // bar nobody believes.
        let state = self.update(|state| {
            state.progress = fraction.max(state.progress);
            if let Some(message) = message {
                state.message = message.to_owned();
            }
        });
        self.send(Event::Progress {
            stage,
            fraction: state.progress,
            message: state.message,
        });
    }

    /// A line of what the stage's tools printed. Kept in memory for `State()`;
    /// written to the state file with the next update.
    pub fn line(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut state = locked(&self.state);
        state.log.push(text.to_owned());
        let excess = state.log.len().saturating_sub(LOG_TAIL_LINES);
        state.log.drain(..excess);
    }

    pub fn done(&self, stage: Stage) {
        self.update(|state| {
            state.progress = 1.0;
            for record in &mut state.stages {
                if record.stage == stage {
                    record.status = StageStatus::Done;
                    record.finished = Some(sys::unix_time());
                }
            }
        });
    }

    pub fn succeed(&self) {
        self.update(|state| {
            state.stage = Stage::Done;
            state.progress = 1.0;
            state.message = "The installation is complete".into();
            state.active = false;
            state.outcome = Some(Outcome::Success);
        });
        self.send(Event::Finished(Outcome::Success));
    }

    pub fn fail(&self, stage: Stage, reason: &str) {
        let outcome = Outcome::Failed {
            stage,
            reason: reason.to_owned(),
        };
        self.update(|state| {
            state.stage = stage;
            state.message = format!("The installation failed during the {} stage", stage.name());
            state.active = false;
            state.outcome = Some(outcome.clone());
            for record in &mut state.stages {
                if record.stage == stage {
                    record.status = StageStatus::Failed;
                    record.finished = Some(sys::unix_time());
                }
            }
        });
        self.send(Event::Finished(outcome));
    }
}

/// `/run/ilex-installer/state.json`, written to a file of its own and renamed
/// into place, so a reader never sees half of it. Mode 0600: root's.
pub fn write_state(dir: &Path, state: &State) -> Result<()> {
    fs::create_dir_all(dir)?;
    let temporary = dir.join(".state.json.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join("state.json"))?;
    Ok(())
}
