//! The helper's state and its three read-only operations: probe this computer,
//! plan an installation, report where things stand. Nothing here writes to a
//! disk; the partitions a plan names are repart's dry run of them.

use crate::{disks, registry, repart, sys};
use anyhow::{Context, Result, bail};
use ilex_installer_model::{
    Edition, Identity, Table,
    edition::EditionRecord,
    plan::{
        Erase, ErrorCode, PLAN_SCHEMA, PLAN_VALID_SECONDS, Plan, PlanError, PlanRequest,
        PlannedComposition, PlannedDisk, PlannedEncryption, PlannedTpm, TpmMode, check_request,
    },
    probe::{EditionInfo, HelperInfo, Probe},
    select,
    state::State,
};
use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
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
        }
    }
}

/// The most plans kept: each is a few KiB.
const PLANS_KEPT: usize = 8;

pub struct Helper {
    pub config: Config,
    pub edition: Edition,
    /// The whole catalogue: detection counts every machine of it.
    full: Table,
    /// This edition's catalogue.
    pub table: Table,
    /// The edition's display name.
    edition_name: String,
    plans: Mutex<VecDeque<Plan>>,
    state: Mutex<State>,
    /// One probe or plan at a time: they share scratch space and the disks.
    work: Mutex<()>,
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
            state: Mutex::new(State::idle(sys::unix_time())),
            work: Mutex::default(),
        })
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_plan(&self, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.plan = Some(id.to_owned());
        state.updated = sys::unix_time();
        let _ = write_state(&self.config.run_dir, &state);
    }

    pub fn publish_state(&self) {
        let _ = write_state(&self.config.run_dir, &self.state());
    }

    pub fn probe(&self) -> Result<Probe> {
        let _busy = self.work.lock().unwrap_or_else(|e| e.into_inner());
        self.probe_locked()
    }

    fn probe_locked(&self) -> Result<Probe> {
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

        let mut warnings = Vec::new();
        let enrollment_reason = probe.enrollment["reason"]
            .as_str()
            .unwrap_or("unknown")
            .to_owned();
        let eligible = probe.enrollment["verdict"] == "eligible";
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
        if tpm == TpmMode::Auto && !eligible {
            warnings.push(format!(
                "automatic unlock is not possible on this computer ({enrollment_reason}); the disk will ask for its passphrase"
            ));
        }
        let plan = Plan {
            schema: PLAN_SCHEMA,
            id,
            created: sys::unix_time(),
            valid_seconds: PLAN_VALID_SECONDS,
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
                tpm: PlannedTpm {
                    requested: tpm,
                    will_enroll: checked.request.encrypt && tpm == TpmMode::Auto && eligible,
                    reason: enrollment_reason,
                },
            },
            warnings,
        };
        {
            let mut plans = self.plans.lock().unwrap_or_else(|e| e.into_inner());
            plans.push_back(plan.clone());
            while plans.len() > PLANS_KEPT {
                plans.pop_front();
            }
        }
        self.set_plan(&plan.id);
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

/// `/run/ilex-installer/state.json`: the stage record the plan describes.
fn write_state(dir: &Path, state: &State) -> Result<()> {
    use std::io::Write;
    fs::create_dir_all(dir)?;
    let temporary = dir.join(".state.json.tmp");
    let mut file = fs::File::create(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(state)?)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join("state.json"))?;
    Ok(())
}
