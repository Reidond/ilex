//! The recovery key between the installation and the interface.
//!
//! `ilex-enroll` makes the key as the installation's encrypt stage ends (a
//! systemd recovery key in a second keyslot of the new volume). The installation
//! thread [`Holder::offer`]s it here and goes on: the image takes ten to twenty
//! minutes, and the person reads, writes down and confirms the key meanwhile. The
//! interface calls `TakeRecoveryKey` (the key comes back once, as a sealed memfd,
//! and is never shown again) and then `AcknowledgeRecoveryKey` with the whole key
//! typed back from the paper. As phase 0's local completion does, the helper
//! compares all eight groups with the key it holds **and** opens the key's own
//! keyslot with what was typed (`authenticate_slot`, through the verifier the
//! installation gave with the key) before it calls the key acknowledged: a key
//! that was mis-copied, or one the volume does not accept, is never recorded as
//! confirmed. The installation thread waits for that in its recovery stage,
//! before the security chip is enrolled: a key nobody confirmed does not unlock
//! the disk in place of the passphrase.
//!
//! A person who cannot confirm (the window that took the key went away, the key
//! could not be fetched, the volume refuses it) is not stuck: `ContinueWithout
//! RecoveryKey` ends the wait as a passphrase-only installation.
//!
//! The key lives only in locked memory of its own ([`Secret`]) and in the memfd
//! the interface was given. It is in no state file, no plan, no log and no
//! argument; `State()` carries only [`KeyStatus`].

use crate::{
    helper::Helper,
    secret::{self, Secret},
};
use ilex_installer_model::{
    plan::{ErrorCode, PlanError, SECRET_MAX_BYTES},
    recovery::{self, KeyStatus},
};
use std::{
    os::fd::OwnedFd,
    sync::{Condvar, Mutex},
    time::Duration,
};

/// Opens the key's own keyslot with what was typed (the installation's volume
/// and slot are behind it): the phase-0 check that the key really unlocks.
pub type Verifier = Box<dyn Fn(&Secret) -> anyhow::Result<()> + Send + Sync>;

struct Held {
    plan: String,
    /// Gone once the key is acknowledged or declined: nothing needs it after that.
    key: Option<Secret>,
    status: KeyStatus,
    verify: Verifier,
}

/// How the installation's wait for the person ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    /// The whole key was typed back and its keyslot opened with it.
    Acknowledged,
    /// The person chose to go on without a recovery key.
    Declined,
    /// The installation is over (the holder was cleared) first.
    Abandoned,
}

#[derive(Default)]
pub struct Holder {
    held: Mutex<Option<Held>>,
    changed: Condvar,
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Holder {
    /// The installation of `plan` has a recovery key, waiting to be taken.
    pub fn offer(&self, plan: &str, key: Secret, verify: Verifier) {
        *locked(&self.held) = Some(Held {
            plan: plan.to_owned(),
            key: Some(key),
            status: KeyStatus::Waiting,
            verify,
        });
        self.changed.notify_all();
    }

    /// The installation is over (or never had a key): forget it, wiping it.
    pub fn clear(&self) {
        *locked(&self.held) = None;
        self.changed.notify_all();
    }

    pub fn status(&self) -> Option<KeyStatus> {
        locked(&self.held).as_ref().map(|h| h.status)
    }

    /// Hand the key over, once, as a sealed memfd.
    pub(crate) fn take(&self, plan: &str) -> Result<OwnedFd, PlanError> {
        let mut held = locked(&self.held);
        let Some(h) = held.as_mut().filter(|h| h.plan == plan) else {
            return Err(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to take",
            ));
        };
        if h.status == KeyStatus::Declined {
            return Err(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "the installation goes on without a recovery key",
            ));
        }
        if h.status != KeyStatus::Waiting {
            return Err(PlanError::new(
                ErrorCode::RecoveryKeyTaken,
                Some("plan"),
                "the recovery key was handed over once and is not shown again",
            ));
        }
        let key = h.key.as_ref().ok_or_else(|| {
            PlanError::new(ErrorCode::NoRecoveryKey, Some("plan"), "the key is gone")
        })?;
        // Made before the status changes: a descriptor that cannot be made
        // leaves the key to be taken again.
        let fd = secret::sealed_memfd(key.expose()).map_err(|e| {
            PlanError::new(
                ErrorCode::Internal,
                None,
                format!("the key's descriptor: {e:#}"),
            )
        })?;
        h.status = KeyStatus::Shown;
        Ok(fd)
    }

    /// Compare the typed key with the held one, all eight groups, and open the
    /// key's keyslot with what was typed. A wrong answer changes nothing: the
    /// person tries again.
    pub(crate) fn acknowledge(&self, plan: &str, answer: &Secret) -> Result<(), PlanError> {
        let mismatch = |why: &str| PlanError::new(ErrorCode::RecoveryKeyMismatch, None, why);
        let mut held = locked(&self.held);
        let Some(h) = held.as_mut().filter(|h| h.plan == plan) else {
            return Err(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to confirm",
            ));
        };
        match h.status {
            KeyStatus::Waiting => {
                return Err(mismatch("the recovery key has not been shown yet"));
            }
            KeyStatus::Acknowledged => return Ok(()),
            KeyStatus::Declined => {
                return Err(PlanError::new(
                    ErrorCode::NoRecoveryKey,
                    Some("plan"),
                    "the installation goes on without a recovery key",
                ));
            }
            KeyStatus::Shown => {}
        }
        let text = std::str::from_utf8(answer.expose())
            .map_err(|_| mismatch("what was typed is not text"))?;
        let typed = zeroize::Zeroizing::new(
            recovery::canonical(text)
                .ok_or_else(|| mismatch("what was typed is not a whole recovery key"))?,
        );
        let key = h
            .key
            .as_ref()
            .and_then(|k| std::str::from_utf8(k.expose()).ok())
            .ok_or_else(|| mismatch("the key is gone"))?;
        if !recovery::matches(key, &typed) {
            return Err(mismatch("that is not the recovery key"));
        }
        // As phase 0's `display_recovery` does after comparing: what was typed
        // must open the key's own keyslot. A failure here is the volume's, not a
        // typing mistake, and the person may go on without the key instead.
        let typed = Secret::from_bytes(typed.as_bytes()).map_err(|e| {
            PlanError::new(ErrorCode::Internal, None, format!("the typed key: {e:#}"))
        })?;
        (h.verify)(&typed).map_err(|e| {
            PlanError::new(
                ErrorCode::Internal,
                None,
                format!("the recovery key does not open its keyslot: {e:#}"),
            )
        })?;
        h.status = KeyStatus::Acknowledged;
        h.key = None;
        self.changed.notify_all();
        Ok(())
    }

    /// The person goes on without a recovery key: allowed while the key is
    /// unconfirmed (never taken, taken, or refused by the volume). The key is
    /// wiped; the installation ends its recovery stage as passphrase-only.
    pub(crate) fn decline(&self, plan: &str) -> Result<(), PlanError> {
        let mut held = locked(&self.held);
        let Some(h) = held.as_mut().filter(|h| h.plan == plan) else {
            return Err(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to skip",
            ));
        };
        match h.status {
            KeyStatus::Acknowledged => {
                return Err(PlanError::new(
                    ErrorCode::RecoveryKeyConfirmed,
                    Some("plan"),
                    "the recovery key was confirmed: the installation keeps it",
                ));
            }
            KeyStatus::Declined => return Ok(()),
            KeyStatus::Waiting | KeyStatus::Shown => {}
        }
        h.status = KeyStatus::Declined;
        h.key = None;
        self.changed.notify_all();
        Ok(())
    }

    /// Wait until the key is acknowledged or declined, or the holder is cleared
    /// (the installation is over). `tick` is called about every second, so that
    /// the caller can say it is still waiting.
    pub fn wait_acknowledged(&self, tick: &mut dyn FnMut()) -> Wait {
        let mut held = locked(&self.held);
        loop {
            match held.as_ref() {
                None => return Wait::Abandoned,
                Some(h) if h.status == KeyStatus::Acknowledged => return Wait::Acknowledged,
                Some(h) if h.status == KeyStatus::Declined => return Wait::Declined,
                Some(_) => {}
            }
            let (next, _) = self
                .changed
                .wait_timeout(held, Duration::from_secs(1))
                .unwrap_or_else(|e| e.into_inner());
            held = next;
            drop(held);
            tick();
            held = locked(&self.held);
        }
    }
}

impl Helper {
    /// `TakeRecoveryKey(plan)`: the key, once, as a sealed memfd.
    pub fn take_recovery_key(&self, plan: &str) -> Result<OwnedFd, PlanError> {
        let fd = self.recovery.take(plan)?;
        self.tracker.set_security(|security| {
            security.recovery_key = Some(KeyStatus::Shown);
        });
        Ok(fd)
    }

    /// `AcknowledgeRecoveryKey(plan, key)`: the whole key typed back, as a sealed
    /// memfd of its text (hyphens, spaces and capitals are forgiven).
    pub fn acknowledge_recovery_key(&self, plan: &str, fd: OwnedFd) -> Result<(), PlanError> {
        let answer = Secret::from_memfd(fd, SECRET_MAX_BYTES)
            .map_err(|e| PlanError::new(ErrorCode::InvalidSecret, Some("fd"), format!("{e:#}")))?;
        self.recovery.acknowledge(plan, &answer)?;
        self.tracker.set_security(|security| {
            security.recovery_key = Some(KeyStatus::Acknowledged);
        });
        Ok(())
    }

    /// `ContinueWithoutRecoveryKey(plan)`: the person gives up on confirming the
    /// key. The installation finishes passphrase-only (`recovery-key-failed`).
    pub fn continue_without_recovery_key(&self, plan: &str) -> Result<(), PlanError> {
        self.recovery.decline(plan)?;
        self.tracker.set_security(|security| {
            security.recovery_key = Some(KeyStatus::Declined);
        });
        Ok(())
    }
}
