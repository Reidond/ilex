//! Carrying out what the flow asks for, off the interface's main loop. A
//! [`Step`] that needs the helper or the compositor becomes a [`Job`]: a
//! closure to run on a worker thread of the interface's choosing, which
//! returns an [`Answer`]; the interface gives that back to the main loop and
//! to [`Flow::answer`], which hands the result to the right method with its
//! generation (a late answer for a request that was replaced is ignored by the
//! flow). Nothing here knows a toolkit, a runtime or a thread pool.

use std::sync::Arc;
use std::time::Duration;

use ilex_installer_model::finish::Saved;
use ilex_installer_model::plan::Plan;
use ilex_installer_model::probe::Probe;
use ilex_installer_model::state::State;

use crate::backend::Backend;
use crate::flow::{Flow, NetworkAction, Step};
use crate::network::NetworkState;
use crate::keyboard::SessionKeyboard;
use crate::messages::ClientError;
use crate::secret::Secret;

/// What a worker brings back. (A plan is large and an answer is moved once, so
/// the size difference between the variants costs nothing worth a box.)
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Answer {
    Probed {
        generation: u64,
        result: Result<Probe, ClientError>,
    },
    Planned {
        generation: u64,
        result: Result<Plan, ClientError>,
    },
    Switched {
        generation: u64,
        result: Result<(), String>,
    },
    Applied {
        generation: u64,
        result: Result<State, ClientError>,
    },
    Stated {
        generation: u64,
        result: Result<State, ClientError>,
    },
    LogProbed {
        generation: u64,
        result: Result<Probe, ClientError>,
    },
    /// The helper answered `TakeRecoveryKey`: the key is a `Secret`.
    KeyTaken {
        generation: u64,
        result: Result<Secret, ClientError>,
    },
    /// The helper compared the key (and opened its keyslot with it).
    KeyConfirmed {
        generation: u64,
        result: Result<(), ClientError>,
    },
    /// The helper answered `ContinueWithoutRecoveryKey`.
    KeySkipped {
        generation: u64,
        result: Result<(), ClientError>,
    },
    Finished {
        generation: u64,
        result: Result<Option<Saved>, ClientError>,
    },
    /// A look at the network.
    Networked {
        generation: u64,
        result: Result<NetworkState, ClientError>,
    },
    /// A Wi-Fi network joined (or not).
    Joined {
        generation: u64,
        result: Result<(), ClientError>,
    },
}

/// A blocking piece of work: run it on a worker, never on the main loop.
pub type Job = Box<dyn FnOnce() -> Answer + Send>;

/// What a job calls.
#[derive(Clone)]
pub struct Services {
    pub backend: Arc<dyn Backend>,
    pub keyboard: Arc<dyn SessionKeyboard + Send + Sync>,
    /// How long the helper is left between two questions about the
    /// installation, at most, instead of what the flow asks (the renderer, so
    /// that a simulated installation does not take its seconds).
    pub poll_ms: Option<u64>,
}

impl Services {
    /// The work `step` asks for, or `None` when it asks for none (the
    /// interface follows the flow's page and dialog by itself).
    pub fn job(&self, step: Step) -> Option<Job> {
        let backend = Arc::clone(&self.backend);
        Some(match step {
            Step::RunProbe { generation } => Box::new(move || Answer::Probed {
                generation,
                result: backend.probe(),
            }),
            Step::RunPlan {
                generation,
                request,
            } => Box::new(move || Answer::Planned {
                generation,
                result: backend.plan(&request),
            }),
            Step::SwitchKeyboard { generation, layout } => {
                // The session types in the new layout from now on; the call
                // blocks on D-Bus. The flow keeps one switch outstanding, so
                // the compositor ends on the last layout asked for.
                let keyboard = Arc::clone(&self.keyboard);
                Box::new(move || {
                    let result = keyboard.switch(&layout).map_err(|e| format!("{e:#}"));
                    if let Err(why) = &result {
                        eprintln!(
                            "ilex-installer: switching the session's layout to {}: {why}",
                            layout.id
                        );
                    }
                    Answer::Switched { generation, result }
                })
            }
            Step::RunApply {
                generation,
                plan,
                erase_disk,
                password,
                passphrase,
            } => Box::new(move || Answer::Applied {
                generation,
                result: backend.start_installation(
                    &plan,
                    &erase_disk,
                    &password,
                    passphrase.as_ref(),
                ),
            }),
            Step::RunState {
                generation,
                delay_ms,
            } => {
                let delay = self.poll_ms.map_or(delay_ms, |fast| delay_ms.min(fast));
                Box::new(move || {
                    if delay > 0 {
                        std::thread::sleep(Duration::from_millis(delay));
                    }
                    Answer::Stated {
                        generation,
                        result: backend.state(),
                    }
                })
            }
            Step::RunTakeKey { generation, plan } => Box::new(move || Answer::KeyTaken {
                generation,
                result: backend.take_recovery_key(&plan),
            }),
            Step::RunAcknowledge {
                generation,
                plan,
                answer,
            } => Box::new(move || Answer::KeyConfirmed {
                generation,
                result: backend.acknowledge_recovery_key(&plan, &answer),
            }),
            Step::RunContinueWithout { generation, plan } => Box::new(move || Answer::KeySkipped {
                generation,
                result: backend.continue_without_recovery_key(&plan),
            }),
            Step::RunLogProbe { generation } => Box::new(move || Answer::LogProbed {
                generation,
                result: backend.probe(),
            }),
            Step::RunFinish { generation, action } => Box::new(move || Answer::Finished {
                generation,
                result: backend.finish(&action),
            }),
            Step::RunNetwork {
                generation,
                delay_ms,
                action,
            } => {
                let delay = self.poll_ms.map_or(delay_ms, |fast| delay_ms.min(fast));
                Box::new(move || {
                    if delay > 0 {
                        std::thread::sleep(Duration::from_millis(delay));
                    }
                    match action {
                        NetworkAction::Look { rescan } => Answer::Networked {
                            generation,
                            result: backend.network(rescan),
                        },
                        NetworkAction::Join {
                            ssid,
                            security,
                            password,
                        } => Answer::Joined {
                            generation,
                            result: backend.join_wifi(&ssid, security, password.as_ref()),
                        },
                    }
                })
            }
            // The widgets follow the flow's page and its dialog.
            Step::Goto(_) | Step::Confirm(_) | Step::Stay => return None,
        })
    }
}

impl Flow {
    /// Give a worker's answer to the method it belongs to.
    pub fn answer(&mut self, answer: Answer) -> Step {
        match answer {
            Answer::Probed { generation, result } => self.probe_finished(generation, result),
            Answer::Planned { generation, result } => self.plan_finished(generation, result),
            Answer::Switched { generation, result } => self.keyboard_switched(generation, result),
            Answer::Applied { generation, result } => self.apply_finished(generation, result),
            Answer::Stated { generation, result } => self.state_finished(generation, result),
            Answer::LogProbed { generation, result } => {
                self.log_probe_finished(generation, result)
            }
            Answer::KeyTaken { generation, result } => self.take_key_finished(generation, result),
            Answer::KeyConfirmed { generation, result } => {
                self.confirm_finished(generation, result)
            }
            Answer::KeySkipped { generation, result } => self.skip_finished(generation, result),
            Answer::Finished { generation, result } => self.finish_finished(generation, result),
            Answer::Networked { generation, result } => self.network_finished(generation, result),
            Answer::Joined { generation, result } => self.join_finished(generation, result),
        }
    }
}
