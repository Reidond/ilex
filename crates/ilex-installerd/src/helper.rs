//! The helper's state and its three read-only operations: probe this computer,
//! plan an installation, report where things stand. Nothing here writes to a
//! disk; the partitions a plan names are repart's dry run of them.

use crate::{disks, registry, repart, sys};
use anyhow::{Context, Result};
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
    collections::VecDeque,
    fs,
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
    pub cmdline: PathBuf,
    pub zoneinfo: PathBuf,
    pub keymaps: PathBuf,
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
            cmdline: "/proc/cmdline".into(),
            zoneinfo: "/usr/share/zoneinfo".into(),
            keymaps: "/usr/lib/kbd/keymaps".into(),
        }
    }
}

/// The most plans kept: each is a few KiB.
const PLANS_KEPT: usize = 8;

pub struct Helper {
    pub config: Config,
    pub edition: Edition,
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
            Ok(bytes) => Some(EditionRecord::parse(&bytes)?),
            Err(_) => None,
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
        let detection = self.table.detect(identity)?;
        let cmdline = fs::read_to_string(&self.config.cmdline).unwrap_or_default();
        let choice = fs::read_to_string(self.config.run_dir.join("choice")).ok();
        let (selection, selection_error) =
            match select(&self.table, &detection, &cmdline, choice.as_deref()) {
                Ok(selected) => (Some(selected), None),
                Err(error) => (None, Some(error.to_string())),
            };
        let scratch = self.config.run_dir.join("scratch");
        fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
        let disks = disks::scan(&scratch)?;
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
            firmware: sys::firmware(),
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
        let scratch = self.config.run_dir.join("plans").join(&id);
        let layout = repart::assemble(&self.config.definitions, checked.request.encrypt, &scratch)
            .map_err(internal);
        let partitions = layout.and_then(|layout| {
            repart::dry_run(&layout, &checked.disk.path, &seed)
                .map_err(|e| PlanError::new(ErrorCode::Repart, None, format!("{e:#}")))
        });
        let _ = fs::remove_dir_all(&scratch);
        let partitions = partitions?;

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
        Ok(())
    }
}

/// A zone file inside the zoneinfo tree (a symlink may not leave it).
pub fn time_zone_exists(root: &Path, zone: &str) -> bool {
    let Ok(root) = fs::canonicalize(root) else {
        return false;
    };
    fs::canonicalize(root.join(zone)).is_ok_and(|p| p.starts_with(&root) && p.is_file())
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
