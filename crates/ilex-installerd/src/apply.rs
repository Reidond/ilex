//! `SetSecret` and `Apply`: everything that happens before the first byte is
//! written to a disk. `Apply` starts the installation only when, at that moment
//! and with the disk claimed, the disk, the image's signature and the layout
//! are exactly what the plan showed; otherwise it refuses, and nothing was
//! written.
//!
//! The order matters and each step is a reason the next may be believed:
//!
//! 1. the plan is one this helper made, is fresh, and the caller repeated the
//!    disk's id (the whole-disk confirmation);
//! 2. the image's signature is accepted by the installer's policy ([`policy`]);
//! 3. a fresh probe still shows the disk the plan was made for: the same id,
//!    serial, model, size and partitions, not the live medium, not in use;
//! 4. the helper claims the whole disk exclusively (`O_EXCL`), which the kernel
//!    only grants when nothing holds the disk or one of its partitions;
//! 5. with the claim held, the disk is looked at once more (the trace, the
//!    partitions) and `systemd-repart`'s dry run is compared with the plan;
//! 6. the stage is recorded, and only then does the installation thread start,
//!    holding the claim until the moment before `systemd-repart` runs.

use crate::{
    disks::{self, Hold},
    install, policy, repart,
    secret::{self, Secret},
    sys,
};
use anyhow::Context;
use ilex_installer_model::{
    plan::{
        ErrorCode, Mode, Plan, PlanError, SECRET_MAX_BYTES, SecretKind,
    },
    probe::Disk,
    state::State,
};
use std::{os::fd::OwnedFd, sync::Arc, thread};

use crate::helper::{Helper, Recorded};

fn locked<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// What differs between the disk a plan was made for and the one found now,
/// as a sentence, or `None` when it is the same disk with the same contents.
/// The identity is the id, model, serial, size and bus, then the partition
/// table, then what the plan listed as erased. What the probe reads from
/// inside (an EFI partition's loaders, which a held disk cannot mount) is not
/// compared: the partitions it is in are.
pub fn disk_difference(then: &Disk, now: &Disk) -> Option<String> {
    let differs = |what: &str| Some(format!("its {what} is not what the plan showed"));
    if then.id != now.id {
        return differs("id");
    }
    if then.path != now.path {
        return differs("kernel name");
    }
    if then.model != now.model {
        return differs("model");
    }
    if then.serial != now.serial {
        return differs("serial number");
    }
    if then.size_bytes != now.size_bytes || then.sector_size != now.sector_size {
        return differs("size");
    }
    if then.bus != now.bus || then.removable != now.removable {
        return differs("bus or removability");
    }
    if then.partition_table != now.partition_table {
        return differs("partition table type");
    }
    if then.partitions != now.partitions {
        return differs("partitions");
    }
    None
}

/// `7 seconds`, `15 minutes`: whichever reads better.
fn duration(seconds: u64) -> String {
    if seconds < 120 {
        format!("{seconds} seconds")
    } else {
        format!("{} minutes", seconds / 60)
    }
}

impl Helper {
    /// `SetSecret(plan, kind, fd)`: read a sealed memfd once into locked
    /// memory and keep it for the plan. A second call for the same plan and
    /// kind replaces the first. Refused without saying anything about the
    /// content.
    pub fn set_secret(&self, plan_id: &str, kind: &str, fd: OwnedFd) -> Result<(), PlanError> {
        let invalid = |message: String| PlanError::new(ErrorCode::InvalidSecret, Some("fd"), message);
        let kind = SecretKind::parse(kind).ok_or_else(|| {
            PlanError::new(
                ErrorCode::InvalidSecret,
                Some("kind"),
                "the kind is admin-password or disk-passphrase",
            )
        })?;
        let plans = locked(&self.plans);
        let recorded = plans.iter().find(|r| r.plan.id == plan_id).ok_or_else(|| {
            PlanError::new(
                ErrorCode::UnknownPlan,
                Some("plan"),
                "the helper has no such plan (it keeps the latest eight and forgets them when it restarts)",
            )
        })?;
        match kind {
            SecretKind::AdminPassword => {}
            SecretKind::DiskPassphrase => {
                return Err(if recorded.plan.request.encrypt {
                    PlanError::new(
                        ErrorCode::UnsupportedMode,
                        Some("encrypt"),
                        "encrypted installations are not available yet",
                    )
                } else {
                    invalid("this plan is not encrypted: it has no passphrase".into())
                });
            }
        }
        // The descriptor is read and closed whatever follows.
        let secret = Secret::from_memfd(fd, SECRET_MAX_BYTES).map_err(|e| invalid(format!("{e:#}")))?;
        secret::admin_password(&secret).map_err(|e| invalid(format!("{e:#}")))?;
        let id = recorded.plan.id.clone();
        drop(plans);
        locked(&self.secrets).insert((id, kind), secret);
        Ok(())
    }

    /// `Apply(plan, erase_disk)`: check everything once more, claim the disk
    /// and start the installation. Returns the state as the installation
    /// begins; `State()` and the `Progress` and `Finished` signals follow it.
    pub fn apply(self: &Arc<Self>, plan_id: &str, erase_disk: &str) -> Result<State, PlanError> {
        let internal =
            |e: anyhow::Error| PlanError::new(ErrorCode::Internal, None, format!("{e:#}"));
        let _busy = locked(&self.work);
        if self.tracker.active() {
            return Err(PlanError::new(
                ErrorCode::InstallationActive,
                None,
                "an installation is already running",
            ));
        }
        // 1. The plan.
        let Recorded { plan, disk: planned } = locked(&self.plans)
            .iter()
            .find(|r| r.plan.id == plan_id)
            .cloned()
            .ok_or_else(|| {
                PlanError::new(
                    ErrorCode::UnknownPlan,
                    Some("plan"),
                    "the helper has no such plan (it keeps the latest eight and forgets them when it restarts): plan again",
                )
            })?;
        let age = sys::unix_time().saturating_sub(plan.created);
        if age > plan.valid_seconds {
            return Err(PlanError::new(
                ErrorCode::PlanExpired,
                Some("plan"),
                format!(
                    "the plan is {} old and is valid for {}: plan again",
                    duration(age),
                    duration(plan.valid_seconds)
                ),
            ));
        }
        match plan.request.mode {
            Mode::WholeDisk => {
                if erase_disk != plan.disk.id {
                    return Err(PlanError::new(
                        ErrorCode::InvalidField,
                        Some("erase_disk"),
                        "whole-disk mode erases the disk: repeat its id to confirm",
                    ));
                }
            }
            Mode::FreeSpace => {
                return Err(PlanError::new(
                    ErrorCode::UnsupportedMode,
                    Some("mode"),
                    "installing into free space is not available yet",
                ));
            }
        }
        if plan.request.encrypt {
            return Err(PlanError::new(
                ErrorCode::UnsupportedMode,
                Some("encrypt"),
                "encrypted installations are not available yet",
            ));
        }
        if !locked(&self.secrets).contains_key(&(plan.id.clone(), SecretKind::AdminPassword)) {
            return Err(PlanError::new(
                ErrorCode::MissingSecret,
                Some("admin-password"),
                "the administrator's password has not been set (SetSecret)",
            ));
        }

        // 2. The signature, before anything is written.
        policy::verify(&plan.image, &self.config.run_dir)?;

        // 3. The disk as it is now.
        let probe = self.probe_locked().map_err(internal)?;
        if !probe.firmware.uefi {
            return Err(PlanError::new(
                ErrorCode::UnsupportedFirmware,
                None,
                "this computer started in BIOS (legacy) mode; ilex installs on UEFI computers only",
            ));
        }
        let now = self.disk_for_apply(&probe.disks, &plan, &planned)?;

        // 4. The kernel's claim on the whole disk, held from here until the
        //    moment before the partition table is written.
        let hold = disks::hold(now).map_err(|why| {
            PlanError::new(
                ErrorCode::InUse,
                Some("disk"),
                format!("{} cannot be claimed: {why}", plan.disk.id),
            )
        })?;

        // 5. With the claim held: the disk and the layout once more.
        self.look_again(&hold, &plan, &planned)?;

        // 6. Start. The secrets move into the installation; what is left over
        //    is wiped.
        let admin = locked(&self.secrets)
            .remove(&(plan.id.clone(), SecretKind::AdminPassword))
            .ok_or_else(|| {
                PlanError::new(
                    ErrorCode::MissingSecret,
                    Some("admin-password"),
                    "the administrator's password has not been set (SetSecret)",
                )
            })?;
        locked(&self.secrets).retain(|(id, _), _| *id != plan.id);
        self.tracker
            .begin(&plan.id, &install::stages())
            .map_err(internal)?;
        let helper = Arc::clone(self);
        let started = thread::Builder::new()
            .name("ilex-apply".into())
            .spawn({
                let plan = plan.clone();
                move || install::run(helper, plan, hold, admin)
            })
            .context("starting the installation thread")
            .map_err(internal);
        if let Err(error) = started {
            self.tracker.fail(
                ilex_installer_model::state::Stage::Partition,
                &error.message,
            );
            return Err(error);
        }
        Ok(self.state())
    }

    /// Find the disk of the plan in a probe and require it to be the same disk
    /// and free for use.
    fn disk_for_apply<'a>(
        &self,
        disks: &'a [Disk],
        plan: &Plan,
        planned: &Disk,
    ) -> Result<&'a Disk, PlanError> {
        let changed = |why: String| {
            PlanError::new(
                ErrorCode::DiskChanged,
                Some("disk"),
                format!("{} is not the disk that was planned: {why}; plan again", plan.disk.id),
            )
        };
        let Some(now) = disks.iter().find(|d| d.id == plan.disk.id) else {
            return Err(changed("it is not there any more".into()));
        };
        if now.live_medium {
            return Err(PlanError::new(
                ErrorCode::LiveMedium,
                Some("disk"),
                format!("{} is the medium this installer started from", now.id),
            ));
        }
        if !now.in_use.is_empty() {
            return Err(PlanError::new(
                ErrorCode::InUse,
                Some("disk"),
                format!(
                    "{} is in use by the running system: {}",
                    now.id,
                    now.in_use.join("; ")
                ),
            ));
        }
        if let Some(why) = disk_difference(planned, now) {
            return Err(changed(why));
        }
        Ok(now)
    }

    /// The last look, with the disk claimed: probe again (the held disk is not
    /// claimed a second time and its partitions are not mounted for reading),
    /// require the same disk, and require `systemd-repart`'s dry run to be the
    /// plan's partitions byte for byte.
    fn look_again(&self, hold: &Hold, plan: &Plan, planned: &Disk) -> Result<(), PlanError> {
        let internal =
            |e: anyhow::Error| PlanError::new(ErrorCode::Internal, None, format!("{e:#}"));
        let disks = disks::scan_held(&self.config.run_dir, hold).map_err(internal)?;
        let now = self.disk_for_apply(&disks, plan, planned)?;
        let layout = repart::layout(&self.config.definitions, plan.request.encrypt).map_err(internal)?;
        let made = repart::dry_run(&layout, &now.path, &plan.seed)
            .map_err(|e| PlanError::new(ErrorCode::Repart, None, format!("{e:#}")))?;
        if made != plan.partitions {
            return Err(PlanError::new(
                ErrorCode::DiskChanged,
                Some("disk"),
                format!(
                    "systemd-repart would now make other partitions on {} than the plan showed; plan again",
                    plan.disk.id
                ),
            ));
        }
        Ok(())
    }
}
