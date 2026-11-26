//! The root helper as an interface sees it: [`Backend`], with the real
//! implementation over D-Bus ([`DbusBackend`], `org.ilex.Installer1` on the
//! system bus, which the bus policy allows only `ilex-installer` and root) and
//! the simulated one ([`crate::simulate::Simulated`]).
//!
//! The calls block: they are made from a worker thread, and each interface
//! brings its own result back to its main loop (relm4's commands for GTK, a
//! queued call into the Qt thread for Kirigami). Nothing here is `async`, so
//! neither toolkit's runtime is imposed on the other.
//!
//! `Probe`, `Plan` and `State` are implemented. `SetSecret`, `Apply` and the
//! progress signals belong to the helper's installation (a later change) and
//! are typed stubs that answer [`ClientError::NotYet`]: the interface pages
//! that need them are written against these signatures.

use crate::messages::ClientError;
use crate::secret::Secret;
use ilex_installer_model::plan::{Plan, PlanRequest};
use ilex_installer_model::probe::Probe;
use ilex_installer_model::state::{Outcome, Stage, State};

/// What a secret is for: the helper reads each from a file descriptor, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    DiskPassphrase,
    AdminPassword,
}

/// A report from a running installation: the `Progress` signal.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgressEvent {
    pub stage: Stage,
    pub fraction: f64,
    pub message: String,
}

/// The end of an installation: the `Finished` signal.
#[derive(Clone, Debug, PartialEq)]
pub struct Finished {
    pub outcome: Outcome,
}

pub trait Backend: Send + Sync {
    /// This computer, its disks and what the installer offers.
    fn probe(&self) -> Result<Probe, ClientError>;

    /// Check a request and return the plan. Nothing is written.
    fn plan(&self, request: &PlanRequest) -> Result<Plan, ClientError>;

    /// Where an installation is.
    fn state(&self) -> Result<State, ClientError>;

    /// Give the helper a secret for a plan (a later change).
    fn set_secret(
        &self,
        _plan: &str,
        _kind: SecretKind,
        _secret: &Secret,
    ) -> Result<(), ClientError> {
        Err(ClientError::NotYet("SetSecret"))
    }

    /// Start the installation of a plan; in whole-disk mode `erase_disk`
    /// repeats the disk's id (a later change).
    fn apply(&self, _plan: &str, _erase_disk: Option<&str>) -> Result<(), ClientError> {
        Err(ClientError::NotYet("Apply"))
    }

    /// The next `Progress` or `Finished` report, waiting for it (a later
    /// change). `Ok(None)` means the helper closed the stream.
    fn next_report(&self) -> Result<Option<Report>, ClientError> {
        Err(ClientError::NotYet("Progress"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Report {
    Progress(ProgressEvent),
    Finished(Finished),
}

#[cfg(target_os = "linux")]
pub use dbus::{DbusBackend, classify_bus_error};

#[cfg(target_os = "linux")]
mod dbus {
    use super::*;
    use ilex_installer_model::plan::PlanError;
    use std::sync::Mutex;
    use zbus::{Connection, Proxy};

    pub const NAME: &str = "org.ilex.Installer1";
    pub const PATH: &str = "/org/ilex/Installer1";
    const ERROR_PREFIX: &str = "org.ilex.Installer1.Error.";

    /// The helper over the system bus. Each call blocks the calling thread
    /// (`zbus::block_on`), so make it from a worker, never from a toolkit's
    /// main loop. The connection is made on first use and again after a
    /// failure, so an interface can start before the bus or the helper is up
    /// and show the problem instead of exiting.
    #[derive(Default)]
    pub struct DbusBackend {
        connection: Mutex<Option<Connection>>,
    }

    impl DbusBackend {
        pub fn new() -> Self {
            Self::default()
        }

        fn connection(&self) -> Result<Connection, ClientError> {
            let mut slot = self.connection.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(connection) = &*slot {
                return Ok(connection.clone());
            }
            let connection = zbus::block_on(Connection::system())
                .map_err(|e| ClientError::HelperUnavailable(format!("the system bus: {e}")))?;
            *slot = Some(connection.clone());
            Ok(connection)
        }

        /// One method of the helper: takes nothing or a JSON text, returns a
        /// JSON text.
        fn call(&self, method: &str, request: Option<&str>) -> Result<String, ClientError> {
            let connection = self.connection()?;
            let reply = zbus::block_on(async {
                let proxy = Proxy::new(&connection, NAME, PATH, NAME)
                    .await
                    .map_err(|e| ClientError::HelperUnavailable(e.to_string()))?;
                let reply = match request {
                    Some(request) => proxy.call::<_, _, String>(method, &(request,)).await,
                    None => proxy.call::<_, _, String>(method, &()).await,
                };
                reply.map_err(map_error)
            });
            if matches!(reply, Err(ClientError::HelperUnavailable(_))) {
                // The connection may be what broke: connect afresh next time.
                *self.connection.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            reply
        }

        fn document<T: serde::de::DeserializeOwned>(
            &self,
            method: &str,
            request: Option<&str>,
        ) -> Result<T, ClientError> {
            let text = self.call(method, request)?;
            serde_json::from_str(&text)
                .map_err(|e| ClientError::BadAnswer(format!("{method}: {e}")))
        }
    }

    impl Backend for DbusBackend {
        fn probe(&self) -> Result<Probe, ClientError> {
            self.document("Probe", None)
        }

        fn plan(&self, request: &PlanRequest) -> Result<Plan, ClientError> {
            let text = serde_json::to_string(request)
                .map_err(|e| ClientError::Other(format!("the request: {e}")))?;
            self.document("Plan", Some(&text))
        }

        fn state(&self) -> Result<State, ClientError> {
            self.document("State", None)
        }
    }

    /// The helper's refusals carry a `PlanError` document as the error's
    /// message; the bus's own errors say the helper is not there, does not
    /// accept this user, or that the connection is gone.
    fn map_error(error: zbus::Error) -> ClientError {
        use zbus::DBusError;
        match &error {
            zbus::Error::MethodError(name, description, _) => {
                classify_bus_error(name.as_str(), description.clone().unwrap_or_default())
            }
            zbus::Error::FDO(fdo) => classify_bus_error(
                fdo.name().as_str(),
                fdo.description().unwrap_or_default().to_owned(),
            ),
            zbus::Error::InputOutput(_) => ClientError::HelperUnavailable(error.to_string()),
            _ => ClientError::Other(error.to_string()),
        }
    }

    /// What a D-Bus error name means to the interface. The helper's own
    /// refusals (`org.ilex.Installer1.Error.*`) carry the model's document;
    /// everything that says the helper or the bus is not there (not started,
    /// gone, disconnected, timed out, could not be started on demand by the
    /// bus or systemd) is [`ClientError::HelperUnavailable`], which also makes
    /// the backend connect afresh next time.
    pub fn classify_bus_error(name: &str, description: String) -> ClientError {
        if let Some(code) = name.strip_prefix(ERROR_PREFIX) {
            return match serde_json::from_str::<PlanError>(&description) {
                Ok(refusal) => ClientError::Refused(refusal),
                Err(_) => ClientError::Other(format!("{code}: {description}")),
            };
        }
        match name {
            "org.freedesktop.DBus.Error.AccessDenied" | "org.freedesktop.DBus.Error.AuthFailed" => {
                ClientError::AccessDenied(description)
            }
            "org.freedesktop.DBus.Error.ServiceUnknown"
            | "org.freedesktop.DBus.Error.NameHasNoOwner"
            | "org.freedesktop.DBus.Error.NoReply"
            | "org.freedesktop.DBus.Error.Disconnected"
            | "org.freedesktop.DBus.Error.TimedOut"
            | "org.freedesktop.DBus.Error.Timeout"
            | "org.freedesktop.DBus.Error.NoServer" => {
                ClientError::HelperUnavailable(format!("{name}: {description}"))
            }
            // The bus could not start the helper (`Spawn.*`), or systemd,
            // which the bus asks to, could not (`org.freedesktop.systemd1.*`).
            _ if name.starts_with("org.freedesktop.DBus.Error.Spawn.")
                || name.starts_with("org.freedesktop.systemd1.") =>
            {
                ClientError::HelperUnavailable(format!("{name}: {description}"))
            }
            other => ClientError::Other(format!("{other}: {description}")),
        }
    }
}
