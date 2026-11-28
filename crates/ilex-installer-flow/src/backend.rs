//! The root helper as an interface sees it: [`Backend`], with the real
//! implementation over D-Bus ([`DbusBackend`], `org.ilex.Installer1` on the
//! system bus, which the bus policy allows only `ilex-installer` and root) and
//! the simulated one ([`crate::simulate::Simulated`]).
//!
//! The live system's network (`network`, `join_wifi`) is here too, though it is
//! not the helper's: the interface asks NetworkManager itself (see
//! [`crate::network`]), and the simulation answers for it.
//!
//! The calls block: they are made from a worker thread, and each interface
//! brings its own result back to its main loop (relm4's commands for GTK, a
//! queued call into the Qt thread for Kirigami). Nothing here is `async`, so
//! neither toolkit's runtime is imposed on the other.
//!
//! Every method of the helper is here: `Probe`, `Plan`, `SetSecret`, `Apply`,
//! `State` and `Finish`. The interface learns how an installation goes by
//! asking `State` about once a second (the same call a restarted interface
//! re-attaches with, and the only one that carries the last lines of what the
//! tools printed); the helper's `Progress` and `Finished` signals are for
//! other listeners and are not subscribed to.

use crate::messages::ClientError;
use crate::network::{NetworkState, Security};
use crate::secret::Secret;
use ilex_installer_model::finish::{FinishAction, Saved};
use ilex_installer_model::plan::{Plan, PlanRequest, SecretKind};
use ilex_installer_model::probe::Probe;
use ilex_installer_model::state::State;

pub trait Backend: Send + Sync {
    /// The live system's network: whether the registry can be reached, the
    /// wired adapter and the Wi-Fi networks in range. `rescan` scans again now.
    fn network(&self, rescan: bool) -> Result<NetworkState, ClientError>;

    /// Join a Wi-Fi network (`password` for a secured one; it goes to
    /// NetworkManager on a standard input, never to a command line or a log).
    fn join_wifi(
        &self,
        ssid: &str,
        security: Security,
        password: Option<&Secret>,
    ) -> Result<(), ClientError>;

    /// This computer, its disks and what the installer offers.
    fn probe(&self) -> Result<Probe, ClientError>;

    /// Check a request and return the plan. Nothing is written.
    fn plan(&self, request: &PlanRequest) -> Result<Plan, ClientError>;

    /// Where an installation is.
    fn state(&self) -> Result<State, ClientError>;

    /// Give the helper a secret for a plan. It crosses as a sealed memfd, never
    /// as text.
    fn set_secret(&self, plan: &str, kind: SecretKind, secret: &Secret) -> Result<(), ClientError>;

    /// Start the installation of a plan; `erase_disk` repeats the disk's id (it
    /// is erased in whole-disk mode, written to beside the other systems). The helper has checked everything again and
    /// begun when this answers: the state returned is the installation's first.
    fn apply(&self, plan: &str, erase_disk: &str) -> Result<State, ClientError>;

    /// Restart, power off, or write the log to a stick (the [`Saved`] record).
    fn finish(&self, action: &FinishAction) -> Result<Option<Saved>, ClientError>;

    /// The installation's recovery key, once: the helper hands it over a single
    /// time (a sealed memfd) and refuses every later request.
    fn take_recovery_key(&self, plan: &str) -> Result<Secret, ClientError>;

    /// The whole key typed back (as the paper has it). The helper compares all
    /// eight groups with the key it holds and opens the key's keyslot with it.
    fn acknowledge_recovery_key(&self, plan: &str, answer: &Secret) -> Result<(), ClientError>;

    /// The person goes on without a recovery key: the installation ends as
    /// passphrase-only and the unconfirmed keyslot is recorded for a later
    /// `ilex-enroll complete --reissue-recovery`.
    fn continue_without_recovery_key(&self, plan: &str) -> Result<(), ClientError>;

    /// What the review page's destructive button does: the disk's passphrase
    /// (when the plan encrypts) and the administrator's password, then the
    /// installation.
    fn start_installation(
        &self,
        plan: &str,
        erase_disk: &str,
        password: &Secret,
        passphrase: Option<&Secret>,
    ) -> Result<State, ClientError> {
        if let Some(passphrase) = passphrase {
            self.set_secret(plan, SecretKind::DiskPassphrase, passphrase)?;
        }
        self.set_secret(plan, SecretKind::AdminPassword, password)?;
        self.apply(plan, erase_disk)
    }
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

        /// One method of the helper: takes `body` (nothing, a JSON text, two
        /// texts), returns a JSON text.
        fn call<B>(&self, method: &str, body: &B) -> Result<String, ClientError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
        {
            self.call_returning(method, body)
        }

        /// A method that returns nothing (`SetSecret`).
        fn call_unit<B>(&self, method: &str, body: &B) -> Result<(), ClientError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
        {
            self.call_returning(method, body)
        }

        fn call_returning<B, R>(&self, method: &str, body: &B) -> Result<R, ClientError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
            R: for<'de> serde::Deserialize<'de> + zbus::zvariant::Type,
        {
            let connection = self.connection()?;
            let reply = zbus::block_on(async {
                let proxy = Proxy::new(&connection, NAME, PATH, NAME)
                    .await
                    .map_err(|e| ClientError::HelperUnavailable(e.to_string()))?;
                proxy.call::<_, _, R>(method, body).await.map_err(map_error)
            });
            if matches!(reply, Err(ClientError::HelperUnavailable(_))) {
                // The connection may be what broke: connect afresh next time.
                *self.connection.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
            reply
        }

        fn document<T: serde::de::DeserializeOwned, B>(
            &self,
            method: &str,
            body: &B,
        ) -> Result<T, ClientError>
        where
            B: serde::Serialize + zbus::zvariant::DynamicType,
        {
            let text = self.call(method, body)?;
            serde_json::from_str(&text)
                .map_err(|e| ClientError::BadAnswer(format!("{method}: {e}")))
        }
    }

    impl Backend for DbusBackend {
        fn network(&self, rescan: bool) -> Result<NetworkState, ClientError> {
            Ok(crate::network::look(rescan))
        }

        fn join_wifi(
            &self,
            ssid: &str,
            security: Security,
            password: Option<&Secret>,
        ) -> Result<(), ClientError> {
            crate::network::join(ssid, security, password).map_err(|why| {
                // Nothing of the password: the exit status only.
                eprintln!("ilex-installer: joining {ssid}: {why}");
                ClientError::Other(why)
            })
        }

        fn probe(&self) -> Result<Probe, ClientError> {
            self.document("Probe", &())
        }

        fn plan(&self, request: &PlanRequest) -> Result<Plan, ClientError> {
            let text = serde_json::to_string(request)
                .map_err(|e| ClientError::Other(format!("the request: {e}")))?;
            self.document("Plan", &(text,))
        }

        fn state(&self) -> Result<State, ClientError> {
            self.document("State", &())
        }

        fn set_secret(
            &self,
            plan: &str,
            kind: SecretKind,
            secret: &Secret,
        ) -> Result<(), ClientError> {
            // A sealed memfd holding the secret, handed over as a file
            // descriptor: the bus never carries it as text.
            let fd = sealed_memfd(secret.expose().as_bytes())
                .map_err(|e| ClientError::Other(format!("the secret's descriptor: {e}")))?;
            let fd = zbus::OwnedFd::from(fd);
            self.call_unit("SetSecret", &(plan, kind.name(), fd))
        }

        fn apply(&self, plan: &str, erase_disk: &str) -> Result<State, ClientError> {
            self.document("Apply", &(plan, erase_disk))
        }

        fn take_recovery_key(&self, plan: &str) -> Result<Secret, ClientError> {
            let fd: zbus::OwnedFd = self.call_returning("TakeRecoveryKey", &(plan,))?;
            read_key(fd.into())
                .map_err(|e| ClientError::Other(format!("the recovery key's descriptor: {e}")))
        }

        fn acknowledge_recovery_key(&self, plan: &str, answer: &Secret) -> Result<(), ClientError> {
            let fd = sealed_memfd(answer.expose().as_bytes())
                .map_err(|e| ClientError::Other(format!("the groups' descriptor: {e}")))?;
            let fd = zbus::OwnedFd::from(fd);
            self.call_unit("AcknowledgeRecoveryKey", &(plan, fd))
        }

        fn continue_without_recovery_key(&self, plan: &str) -> Result<(), ClientError> {
            self.call_unit("ContinueWithoutRecoveryKey", &(plan,))
        }

        fn finish(&self, action: &FinishAction) -> Result<Option<Saved>, ClientError> {
            let text = self.call("Finish", &(action.text(),))?;
            match action {
                FinishAction::SaveLog { .. } => serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|e| ClientError::BadAnswer(format!("Finish: {e}"))),
                _ => Ok(None),
            }
        }
    }

    /// What the helper's `TakeRecoveryKey` returned: a memfd, read from its start
    /// once and closed. A key is 71 characters; anything much longer is not one.
    fn read_key(fd: std::os::fd::OwnedFd) -> std::io::Result<Secret> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::from(fd);
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(1024).read_to_end(&mut bytes)?;
        let secret = std::str::from_utf8(&bytes)
            .map(Secret::new)
            .map_err(|_| std::io::Error::other("the key is not text"));
        zeroize::Zeroize::zeroize(&mut bytes);
        secret
    }

    /// A memfd holding `content`, sealed against writing and resizing: what the
    /// helper requires of a secret's descriptor. Close-on-exec, so that no
    /// child of this process inherits it.
    fn sealed_memfd(content: &[u8]) -> std::io::Result<std::os::fd::OwnedFd> {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: a plain syscall with a constant name and flags.
        let raw = unsafe {
            libc::memfd_create(
                c"ilex-secret".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `raw` is a descriptor this call just made.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let mut file = std::fs::File::from(owned.try_clone()?);
        file.write_all(content)?;
        // SAFETY: sealing a descriptor this function owns.
        let sealed = unsafe {
            libc::fcntl(
                owned.as_raw_fd(),
                libc::F_ADD_SEALS,
                libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
            )
        };
        if sealed != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(owned)
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
