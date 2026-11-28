//! The helper's state and its three read-only operations: probe this computer,
//! plan an installation, report where things stand. Nothing here writes to a
//! disk; the partitions a plan names are repart's dry run of them.

use crate::{disks, progress::Tracker, registry, repart, secret::Secret, sys};
use anyhow::{Context, Result, bail};
use ilex_installer_model::{
    Edition, Identity, Table,
    edition::EditionRecord,
    plan::{
        Erase, ErrorCode, PLAN_SCHEMA, PLAN_VALID_SECONDS, Plan, PlanError, PlanRequest,
        PlannedComposition, PlannedDisk, PlannedEncryption, PlannedTpm, SecretKind, TpmMode,
        check_request,
    },
    probe::{Disk, EditionInfo, HelperInfo, Probe},
    select,
    state::State,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

/// Where the helper reads its inputs. Production uses the defaults; the
/// self-check points them at fixtures.
#[derive(Clone, Debug)]
pub struct Config {
    pub table: PathBuf,
    pub edition_file: PathBuf,
    pub edition: Option<String>,
    pub definitions: PathBuf,
    pub run_dir: PathBuf,
    pub dmi_dir: PathBuf,
    /// `/sys/firmware/efi`: present when the computer booted in UEFI mode.
    pub efi_dir: PathBuf,
    pub cmdline: PathBuf,
    pub zoneinfo: PathBuf,
    pub keymaps: PathBuf,
    /// The live image's own accounts and groups: names the administrator
    /// may not take.
    pub passwd: PathBuf,
    pub group: PathBuf,
    pub sysusers: Vec<PathBuf>,
    /// How long a plan is valid (the `installer::` tier shortens it).
    pub plan_valid_seconds: u64,
    /// The `installer::` tier's fault: while this file exists, `Apply` stops
    /// after it has probed the disk and before it claims it, says so by
    /// creating `<file>.reached`, and goes on when the file is removed. A
    /// mount or an exclusive open made in that moment is the race the claim
    /// itself must catch.
    pub pause_before_hold: Option<PathBuf>,
    /// The digests answers mode saw in this boot ([`crate::stamp::SEEN`]): the
    /// installation that ends writes them into the disk's stamp.
    pub seen_file: PathBuf,
    /// The digests of the stamps this boot's installations wiped
    /// ([`crate::stamp::EARLIER`]): kept just before each wipe, so that a retry
    /// after a failed installation still carries them.
    pub earlier_file: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            table: "/usr/share/ilex-installer/compositions.json".into(),
            edition_file: "/usr/share/ilex-installer/edition.json".into(),
            edition: None,
            definitions: "/usr/lib/ilex-installer/repart".into(),
            run_dir: "/run/ilex-installer".into(),
            dmi_dir: "/sys/class/dmi/id".into(),
            efi_dir: "/sys/firmware/efi".into(),
            cmdline: "/proc/cmdline".into(),
            zoneinfo: "/usr/share/zoneinfo".into(),
            keymaps: "/usr/lib/kbd/keymaps".into(),
            passwd: "/etc/passwd".into(),
            group: "/etc/group".into(),
            sysusers: vec![
                "/usr/lib/sysusers.d".into(),
                "/etc/sysusers.d".into(),
                "/run/sysusers.d".into(),
            ],
            plan_valid_seconds: PLAN_VALID_SECONDS,
            pause_before_hold: None,
            seen_file: crate::stamp::SEEN.into(),
            earlier_file: crate::stamp::EARLIER.into(),
        }
    }
}

/// The most plans kept: each is a few KiB.
const PLANS_KEPT: usize = 8;

/// A plan the helper made, with the disk as it was probed then: `Apply`
/// compares the disk it finds with this, not only with the plan's summary.
#[derive(Clone)]
pub struct Recorded {
    pub plan: Plan,
    pub disk: Disk,
    /// When the helper made it, on `CLOCK_BOOTTIME` ([`sys::boottime`]): a plan's
    /// age is not the wall clock's (which a clock set backwards, or an NTP
    /// step, moves), and it counts the time the machine was suspended too.
    pub made: Duration,
}

pub struct Helper {
    pub config: Config,
    pub edition: Edition,
    /// The whole catalogue: detection counts every machine of it.
    full: Table,
    /// This edition's catalogue.
    pub table: Table,
    /// The edition's display name.
    pub(crate) edition_name: String,
    pub(crate) plans: Mutex<VecDeque<Recorded>>,
    /// The secrets `SetSecret` was given, by plan. Held in locked memory and
    /// wiped when taken or when their plan is forgotten.
    pub(crate) secrets: Mutex<BTreeMap<(String, SecretKind), Secret>>,
    /// The recovery key of the installation that runs: made by the
    /// installation, taken and confirmed through the bus.
    pub(crate) recovery: crate::recovery::Holder,
    /// Where an installation is.
    pub tracker: Tracker,
    /// One probe, plan or apply at a time: they share scratch space and the
    /// disks.
    pub(crate) work: Mutex<()>,
}

impl Helper {
    pub fn new(config: Config) -> Result<Self> {
        let bytes = fs::read(&config.table)
            .with_context(|| format!("reading {}", config.table.display()))?;
        let full = Table::parse(&bytes)?;
        // The image build writes the edition record; `--edition` overrides it
        // (a development image), and a table without either is the Anaconda
        // path's GNOME table.
        let record = match fs::read(&config.edition_file) {
            Ok(bytes) => Some(EditionRecord::parse(&bytes).with_context(|| {
                format!(
                    "reading the edition record {}",
                    config.edition_file.display()
                )
            })?),
            // Absent is the Anaconda path's table; anything else (a record
            // that cannot be read) is a broken image, not a missing record.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                bail!(
                    "reading the edition record {}: {e}",
                    config.edition_file.display()
                );
            }
        };
        let edition = match (&config.edition, &record) {
            (Some(shell), _) => Edition::new(shell)?,
            (None, Some(record)) => record.edition(),
            (None, None) => Edition::of_default(&full)?,
        };
        let table = full.for_edition(&edition)?;
        let config_run_dir = config.run_dir.clone();
        let edition_name = match &record {
            Some(record) if record.edition() == edition => record.display_name.clone(),
            _ => edition.fallback_name(&full),
        };
        Ok(Self {
            edition_name,
            config,
            edition,
            full,
            table,
            plans: Mutex::default(),
            secrets: Mutex::default(),
            recovery: crate::recovery::Holder::default(),
            tracker: Tracker::new(&config_run_dir),
            work: Mutex::default(),
        })
    }

    pub fn state(&self) -> State {
        self.tracker.state()
    }

    /// Write the record of this helper's own state (a helper that finds an
    /// installation another one did not end records that it is over).
    pub fn publish_state(&self) {
        let _ = crate::progress::write_state(&self.config.run_dir, &self.state());
    }

    /// While an installation runs the disks and the scratch space are its:
    /// nothing else probes, plans or applies.
    pub(crate) fn refuse_while_installing(&self) -> Result<(), PlanError> {
        if self.tracker.active() {
            return Err(PlanError::new(
                ErrorCode::InstallationActive,
                None,
                "an installation is running; wait for it to finish",
            ));
        }
        Ok(())
    }

    pub fn probe(&self) -> Result<Probe, PlanError> {
        let _busy = self.work.lock().unwrap_or_else(|e| e.into_inner());
        self.refuse_while_installing()?;
        self.probe_locked()
            .map_err(|e| PlanError::new(ErrorCode::Internal, None, format!("{e:#}")))
    }

    pub(crate) fn probe_locked(&self) -> Result<Probe> {
        let identity = Identity::read(&self.config.dmi_dir);
        // Detection sees every machine of the catalogue, then maps into this
        // edition: a conflict with a machine of another edition still counts.
        let detection = self.full.detect_for_edition(&self.edition, identity)?;
        let cmdline = fs::read_to_string(&self.config.cmdline).unwrap_or_default();
        let choice = fs::read_to_string(self.config.run_dir.join("choice")).ok();
        let (selection, selection_error) =
            match select(&self.table, &detection, &cmdline, choice.as_deref()) {
                Ok(selected) => (Some(selected), None),
                Err(error) => (None, Some(error.to_string())),
            };
        let disks = disks::scan(&self.config.run_dir)?;
        Ok(Probe {
            schema: ilex_installer_model::probe::PROBE_SCHEMA,
            helper: HelperInfo {
                version: env!("CARGO_PKG_VERSION").into(),
                systemd: systemd_version(),
            },
            edition: EditionInfo {
                id: self.edition.id().into(),
                name: self.edition_name.clone(),
            },
            firmware: sys::firmware(&self.config.efi_dir),
            enrollment: serde_json::to_value(ilex_enroll::probe())?,
            detection,
            selection,
            selection_error,
            catalogue: self.table.clone(),
            disks,
            memory: sys::memory()?,
        })
    }

    /// Check a request against a fresh probe, the catalogue, this system's
    /// zone and keymap names and the registry; return the plan repart's dry
    /// run makes of it. Writes no disk.
    pub fn plan(&self, request_text: &str) -> Result<Plan, PlanError> {
        let request = PlanRequest::parse(request_text)?;
        let internal =
            |e: anyhow::Error| PlanError::new(ErrorCode::Internal, None, format!("{e:#}"));
        let _busy = self.work.lock().unwrap_or_else(|e| e.into_inner());
        self.refuse_while_installing()?;
        let probe = self.probe_locked().map_err(internal)?;
        let checked = check_request(&request, &probe)?;
        self.check_system_names(&checked.request)?;

        let tag = &checked.composition.tag;
        let target = self.table.image_reference(tag);
        let reference_tag = self.table.testing_tag.as_deref().unwrap_or(tag);
        let image = registry::resolve(&self.table.repository, reference_tag, &target)?;

        let id = sys::hex(&sys::random_bytes::<16>().map_err(internal)?);
        let seed = sys::random_uuid().map_err(internal)?;
        // The shipped directory of this layout is what repart reads: it is
        // never copied or assembled, and holds exactly one root partition.
        let layout =
            repart::layout(&self.config.definitions, checked.request.encrypt).map_err(internal)?;
        let partitions = repart::dry_run(&layout, &checked.disk.path, &seed)
            .map_err(|e| PlanError::new(ErrorCode::Repart, None, format!("{e:#}")))?;

        // The kernel's claim on the chosen disk, once more and as late as it
        // can be: a use that began since the probe, or one the trace does not
        // show, is a use all the same.
        if let Some(why) = disks::recheck(checked.disk) {
            return Err(PlanError::new(
                ErrorCode::InUse,
                Some("disk"),
                format!("{} is in use by the running system: {why}", checked.disk.id),
            ));
        }

        let mut warnings = Vec::new();
        let enrollment_reason = probe.enrollment["reason"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let tpm = checked.request.tpm_mode();
        if checked.disk.removable {
            warnings.push(format!("{} is a removable disk", checked.disk.id));
        }
        // The list of other systems may be incomplete: say so.
        warnings.extend(checked.disk.notes.iter().cloned());
        if !checked.disk.other_systems.is_empty() {
            warnings.push(format!(
                "everything on {} will be erased, including {} other system(s) found on it",
                checked.disk.id,
                checked.disk.other_systems.len()
            ));
        }
        // The security chip is enrolled only with a recovery key that a person
        // has confirmed, on a computer the probe says is eligible. Say now when
        // it will not be, and why: the disk then asks for its passphrase at
        // every start.
        let recovery_key = checked.request.encrypt && checked.request.recovery_key;
        let eligible = probe.enrollment["verdict"] == "eligible";
        let will_enroll = recovery_key && tpm == TpmMode::Auto && eligible;
        // The most basic reason first: a computer that cannot is not helped by
        // a key, and a key without a computer that can is not enough.
        let tpm_reason = if tpm == TpmMode::Off {
            "not-requested".to_owned()
        } else if !eligible || recovery_key {
            enrollment_reason
        } else {
            "no-recovery-key".to_owned()
        };
        if checked.request.encrypt && tpm == TpmMode::Auto && !will_enroll {
            warnings.push(format!(
                "unlocking the disk with this computer's security chip is not used ({}); the disk asks for its passphrase at every start",
                ilex_installer_model::recovery::tpm_reason(&tpm_reason)
            ));
        }
        let plan = Plan {
            schema: PLAN_SCHEMA,
            id,
            created: sys::unix_time(),
            valid_seconds: self.config.plan_valid_seconds,
            seed,
            edition: self.edition.id().into(),
            request: checked.request.clone(),
            composition: PlannedComposition {
                tag: tag.clone(),
                machine: checked.composition.machine.clone(),
                shells: checked.composition.shells.clone(),
                reason: probe
                    .selection
                    .as_ref()
                    .filter(|s| &s.tag == tag)
                    .map(|s| s.reason.clone())
                    .or_else(|| {
                        (probe.detection.tag == *tag).then(|| probe.detection.reason.clone())
                    }),
            },
            image,
            disk: PlannedDisk {
                id: checked.disk.id.clone(),
                path: checked.disk.path.clone(),
                model: checked.disk.model.clone(),
                serial: checked.disk.serial.clone(),
                size_bytes: checked.disk.size_bytes,
            },
            partitions,
            erases: Erase {
                disk: checked.disk.id.clone(),
                partitions: checked
                    .disk
                    .partitions
                    .iter()
                    .map(|p| p.path.clone())
                    .collect(),
                systems: checked.disk.other_systems.clone(),
            },
            encryption: PlannedEncryption {
                enabled: checked.request.encrypt,
                recovery_key,
                tpm: PlannedTpm {
                    requested: tpm,
                    will_enroll,
                    reason: tpm_reason,
                },
            },
            warnings,
        };
        {
            let mut plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
            plans.push_back(Recorded {
                plan: plan.clone(),
                disk: checked.disk.clone(),
                made: sys::boottime(),
            });
            let mut secrets = self.secrets.lock().unwrap_or_else(|e| e.into_inner());
            while plans.len() > PLANS_KEPT {
                if let Some(forgotten) = plans.pop_front() {
                    // A secret does not outlive the plan it was given for.
                    secrets.retain(|(id, _), _| *id != forgotten.plan.id);
                }
            }
        }
        self.tracker.set_plan(&plan.id);
        Ok(plan)
    }

    /// The checks that need this system's files: names the target's own tools
    /// would otherwise reject mid-installation.
    fn check_system_names(&self, request: &PlanRequest) -> Result<(), PlanError> {
        if !time_zone_exists(&self.config.zoneinfo, &request.time_zone) {
            return Err(PlanError::new(
                ErrorCode::InvalidField,
                Some("time_zone"),
                format!("unknown time zone {}", request.time_zone),
            ));
        }
        if !keymap_exists(&self.config.keymaps, &request.keyboard) {
            return Err(PlanError::new(
                ErrorCode::InvalidField,
                Some("keyboard"),
                format!("unknown console keymap {}", request.keyboard),
            ));
        }
        let names = system_names(
            &self.config.passwd,
            &self.config.group,
            &self.config.sysusers,
        );
        if names.contains(&request.administrator) {
            return Err(PlanError::new(
                ErrorCode::ReservedName,
                Some("administrator"),
                format!(
                    "{} is an account or group the system defines; pick a distinct maintenance name such as ilexmaint",
                    request.administrator
                ),
            ));
        }
        Ok(())
    }
}

/// Every user and group name this (live) system defines: its `/etc/passwd`
/// and `/etc/group`, and the `u`, `g` and `m` lines of `sysusers.d`, which
/// the installed system, built from the same distribution, shares.
pub fn system_names(passwd: &Path, group: &Path, sysusers: &[PathBuf]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for file in [passwd, group] {
        for line in fs::read_to_string(file).unwrap_or_default().lines() {
            if let Some((name, _)) = line.split_once(':') {
                names.insert(name.to_owned());
            }
        }
    }
    for dir in sysusers {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "conf") {
                continue;
            }
            for line in fs::read_to_string(&path).unwrap_or_default().lines() {
                let mut fields = line.split_whitespace();
                let kind = fields.next().unwrap_or("");
                // `u name ...`, `g name ...`, `m user group`; `r` and
                // `#` lines name no one.
                let wanted = match kind.chars().next() {
                    Some('u' | 'g') => 1,
                    Some('m') => 2,
                    _ => continue,
                };
                names.extend(
                    fields
                        .take(wanted)
                        .filter(|n| !n.starts_with(['-', '/', '%']))
                        .map(str::to_owned),
                );
            }
        }
    }
    names
}

/// A zone file inside the zoneinfo tree: a regular file that starts with
/// the TZif magic (so not `zone.tab`, `tzdata.zi` or a directory), whose name
/// is not one of the duplicate trees (`posix/`, `right/`) or `posixrules`, and
/// whose links stay inside the tree.
pub fn time_zone_exists(root: &Path, zone: &str) -> bool {
    if zone
        .split('/')
        .next()
        .is_some_and(|first| ["posix", "right"].contains(&first))
        || zone == "posixrules"
    {
        return false;
    }
    let Ok(root) = fs::canonicalize(root) else {
        return false;
    };
    let Ok(path) = fs::canonicalize(root.join(zone)) else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(&root) else {
        return false;
    };
    if relative
        .components()
        .next()
        .is_some_and(|c| ["posix", "right"].iter().any(|d| c.as_os_str() == *d))
    {
        return false;
    }
    let mut magic = [0u8; 4];
    path.is_file()
        && fs::File::open(&path)
            .and_then(|mut file| file.read_exact(&mut magic))
            .is_ok()
        && &magic == b"TZif"
}

/// A console keymap `<name>.map[.gz]` anywhere in the kbd keymaps tree.
pub fn keymap_exists(root: &Path, name: &str) -> bool {
    fn walk(dir: &Path, name: &str, depth: u32) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let path = entry.path();
            if path.is_dir() {
                depth < 4 && walk(&path, name, depth + 1)
            } else {
                let file = entry.file_name().to_string_lossy().into_owned();
                file == format!("{name}.map.gz") || file == format!("{name}.map")
            }
        })
    }
    walk(root, name, 0)
}

fn systemd_version() -> Option<String> {
    let out = crate::exec::run(
        "systemctl",
        &["--version"],
        std::time::Duration::from_secs(10),
    )
    .ok()?;
    out.stdout_text().lines().next().map(str::to_owned)
}
