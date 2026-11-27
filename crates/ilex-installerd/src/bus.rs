//! `org.ilex.Installer1` on the system bus. The bus policy decides who may call
//! it (`ilex-installer` and root); every method takes JSON text and returns
//! JSON text, and none takes a path, an image reference, a kernel argument or a
//! command. A refusal's error message is the `PlanError` document.

use crate::{helper::Helper, progress::Event};
use anyhow::Result;
use ilex_installer_model::plan::{ErrorCode, PlanError};
use std::sync::{Arc, mpsc};
use zbus::{OwnedFd, connection, interface};

pub const NAME: &str = "org.ilex.Installer1";
pub const PATH: &str = "/org/ilex/Installer1";

#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "org.ilex.Installer1.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    /// The request is not valid JSON of this schema, or a field is malformed.
    InvalidRequest(String),
    /// A valid request the helper will not carry out.
    Refused(String),
    /// A service the plan needs (the registry) did not answer.
    Unavailable(String),
    Failed(String),
}

impl From<PlanError> for Error {
    fn from(error: PlanError) -> Self {
        let document = serde_json::to_string(&error).unwrap_or_else(|_| error.message.clone());
        match error.code {
            ErrorCode::InvalidRequest | ErrorCode::InvalidField => Self::InvalidRequest(document),
            ErrorCode::Registry | ErrorCode::ImageNotFound => Self::Unavailable(document),
            ErrorCode::Repart | ErrorCode::Internal => Self::Failed(document),
            _ => Self::Refused(document),
        }
    }
}

fn failed(error: anyhow::Error) -> Error {
    Error::from(PlanError::new(
        ErrorCode::Internal,
        None,
        format!("{error:#}"),
    ))
}

struct Installer {
    helper: Arc<Helper>,
}

#[interface(name = "org.ilex.Installer1")]
impl Installer {
    /// This computer, its disks and what the installer offers, as JSON.
    async fn probe(&self) -> Result<String, Error> {
        let helper = Arc::clone(&self.helper);
        let probe = blocking::unblock(move || helper.probe()).await?;
        serde_json::to_string(&probe).map_err(|e| failed(e.into()))
    }

    /// Check a request and return the plan; nothing is written.
    async fn plan(&self, request: String) -> Result<String, Error> {
        let helper = Arc::clone(&self.helper);
        let plan = blocking::unblock(move || helper.plan(&request)).await?;
        serde_json::to_string(&plan).map_err(|e| failed(e.into()))
    }

    /// Hand over a secret for a plan as a sealed memfd (`admin-password`; the
    /// disk's passphrase comes with encryption). The helper reads the
    /// descriptor once into locked memory.
    async fn set_secret(&self, plan: String, kind: String, fd: OwnedFd) -> Result<(), Error> {
        let helper = Arc::clone(&self.helper);
        let fd: std::os::fd::OwnedFd = fd.into();
        blocking::unblock(move || helper.set_secret(&plan, &kind, fd))
            .await
            .map_err(Error::from)
    }

    /// Install what the plan describes. The caller repeats the disk's id (whole
    /// disk mode erases it). Returns once the installation has started, as the
    /// state; every refusal comes before anything is written.
    async fn apply(&self, plan: String, erase_disk: String) -> Result<String, Error> {
        let helper = Arc::clone(&self.helper);
        let state = blocking::unblock(move || helper.apply(&plan, &erase_disk)).await?;
        serde_json::to_string(&state).map_err(|e| failed(e.into()))
    }

    /// Where an installation is; a restarted interface re-attaches with this.
    async fn state(&self) -> Result<String, Error> {
        serde_json::to_string(&self.helper.state()).map_err(|e| failed(e.into()))
    }

    /// End the session: `restart`, `poweroff` or `save-log:<device id>` (a
    /// tarball of the installation's logs onto a removable disk that is neither
    /// the installation's target nor the medium the installer started from).
    /// Refused while an installation runs. Returns JSON.
    async fn finish(&self, action: String) -> Result<String, Error> {
        let helper = Arc::clone(&self.helper);
        let answer = blocking::unblock(move || helper.finish(&action)).await?;
        Ok(answer)
    }

    /// A stage began or moved: the stage's name, how far it is (0 to 1) and a
    /// sentence for the person.
    #[zbus(signal)]
    async fn progress(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        stage: &str,
        fraction: f64,
        message: &str,
    ) -> zbus::Result<()>;

    /// The installation ended: its outcome as JSON.
    #[zbus(signal)]
    async fn finished(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        outcome: &str,
    ) -> zbus::Result<()>;
}

/// Own the name and serve until the process is stopped.
pub fn serve(helper: Arc<Helper>) -> Result<()> {
    zbus::block_on(async move {
        let connection = connection::Builder::system()
            .name(NAME)
            .serve_at(
                PATH,
                Installer {
                    helper: Arc::clone(&helper),
                },
            )
            .build()
            .await?;
        // The installation runs on a thread of its own and sends its events
        // here; a thread of its own turns them into signals.
        let (sender, receiver) = mpsc::channel::<Event>();
        helper.tracker.set_sender(sender);
        let emitter = connection.clone();
        std::thread::Builder::new()
            .name("ilex-signals".into())
            .spawn(move || {
                for event in receiver {
                    let result = zbus::block_on(async {
                        match event {
                            Event::Progress {
                                stage,
                                fraction,
                                message,
                            } => {
                                emitter
                                    .emit_signal(
                                        None::<&str>,
                                        PATH,
                                        NAME,
                                        "Progress",
                                        &(stage.name(), fraction, message.as_str()),
                                    )
                                    .await
                            }
                            Event::Finished(outcome) => {
                                let json = serde_json::to_string(&outcome).unwrap_or_default();
                                emitter
                                    .emit_signal(
                                        None::<&str>,
                                        PATH,
                                        NAME,
                                        "Finished",
                                        &(json.as_str(),),
                                    )
                                    .await
                            }
                        }
                    });
                    if let Err(e) = result {
                        eprintln!("ilex-installerd: could not send a signal: {e}");
                    }
                }
            })?;
        std::future::pending::<()>().await;
        Ok(())
    })
}
