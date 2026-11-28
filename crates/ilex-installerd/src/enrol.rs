//! The recovery key and the security chip, through `ilex-enroll`'s library: the
//! same code the installed system's first-boot repair and the Anaconda
//! completion call, so that one implementation decides what "eligible" is and
//! what a token must bind. This file only gives it what it needs from an
//! installation: the open LUKS volume, the passphrase as a descriptor, and a
//! place for its state record while the new system's `/var` does not exist yet.
//!
//! The state record (`enrollment.json`) is the library's: it binds slot ids and
//! the acknowledgement to the volume's UUID. It is written under a directory of
//! `/run` while the image is installed (`bootc` needs the new file systems empty)
//! and copied to the new system's `/var/lib/ilex/security/` as the last step,
//! where `ilex-security-verify` and `ilexctl security repair` read it.
//!
//! Nothing here logs a secret: the library never puts one in an error, and the
//! key reaches the rest of the helper only as a [`Secret`] of its own.

use crate::secret::Secret;
use anyhow::{Context, Result, ensure};
use ilex_enroll::{Device, Reconciled, SecurityState, secret_fd};
use std::{
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// What an installation needs to open `ilex-enroll`'s session on its volume.
pub struct Prepared {
    pub device: Device,
    unlock: ilex_enroll::Secret,
    /// The root of the scratch state directory: the record is at
    /// `<scratch>/var/lib/ilex/security/enrollment.json`.
    pub scratch: PathBuf,
    /// The security chip may be enrolled (the plan said so).
    pub eligible: bool,
}

impl Prepared {
    /// The volume `uuid`, addressed as the library requires
    /// (`/dev/disk/by-uuid/<LUKS UUID>`), unlocked with the passphrase.
    pub fn new(uuid: &str, passphrase: &Secret, scratch: &Path, eligible: bool) -> Result<Self> {
        let path = Path::new("/dev/disk/by-uuid").join(uuid);
        // udev makes the link as it sees the volume; the partition was made
        // moments ago.
        let started = Instant::now();
        while !path.exists() {
            ensure!(
                started.elapsed() < Duration::from_secs(20),
                "{} did not appear",
                path.display()
            );
            thread::sleep(Duration::from_millis(250));
        }
        let device = Device::open(&path).context("opening the LUKS volume for enrolment")?;
        let unlock = ilex_enroll::Secret::read(secret_fd(passphrase.expose())?)
            .context("keeping the passphrase for enrolment")?;
        Ok(Self {
            device,
            unlock,
            scratch: scratch.to_path_buf(),
            eligible,
        })
    }

    /// A session that reconciles the scratch record with the volume's header.
    pub fn session(&self) -> Result<Reconciled<'_>> {
        let state = SecurityState::open(&self.scratch, &self.device)?;
        Reconciled::open(&self.device, &self.unlock, state, self.eligible)
    }

    pub fn uuid(&self) -> &str {
        &self.device.uuid
    }

    /// Whether the volume has room for what will be added (the recovery key,
    /// and the token when it will be enrolled): checked before anything is.
    pub fn preflight(&self) -> Result<()> {
        let state = SecurityState::open(&self.scratch, &self.device)?;
        self.device.preflight(&state, true, self.eligible)
    }
}
