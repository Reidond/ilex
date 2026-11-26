//! `--simulate <scenario>`: the helper's answers from JSON fixtures, so an
//! interface can be built, looked at and tested without a helper, a disk or a
//! registry. Each fixture holds a complete `Probe` document, a `Plan` to
//! return, and optionally a refusal, a failing probe or an installation's
//! `State`. The simulation still applies the model's own `check_request` to
//! every request, so a refusal on a page is the real rule's, not a canned one.
//!
//! Fixtures are compiled in. A released ISO must not be talked into
//! pretending: `ilex-installer` refuses `--simulate` unless the kernel command
//! line carries `ilex.installer.simulate` or the environment variable
//! `ILEX_INSTALLER_SIMULATE_ALLOW` is set (the development and self-check
//! containers), see [`allowed`].

use crate::backend::Backend;
use crate::messages::ClientError;
use anyhow::{Context, Result, bail};
use ilex_installer_model::plan::{
    Erase, Plan, PlanError, PlanRequest, PlannedDisk, PlannedEncryption, PlannedTpm, TpmMode,
    check_request,
};
use ilex_installer_model::probe::Probe;
use ilex_installer_model::state::State;
use serde::Deserialize;
use std::fs;

const FIXTURES: &[(&str, &str)] = &[
    ("generic", include_str!("../fixtures/generic.json")),
    ("ms-7c35", include_str!("../fixtures/ms-7c35.json")),
    ("ms-7c35-board", include_str!("../fixtures/ms-7c35-board.json")),
    ("conflict", include_str!("../fixtures/conflict.json")),
    ("no-composition", include_str!("../fixtures/no-composition.json")),
    ("boot-override", include_str!("../fixtures/boot-override.json")),
    ("bad-override", include_str!("../fixtures/bad-override.json")),
    ("bios", include_str!("../fixtures/bios.json")),
    ("no-disk", include_str!("../fixtures/no-disk.json")),
    ("helper-down", include_str!("../fixtures/helper-down.json")),
    ("registry-down", include_str!("../fixtures/registry-down.json")),
    ("installing", include_str!("../fixtures/installing.json")),
];

/// The scenario `--simulate` takes when it is given none.
pub const DEFAULT_SCENARIO: &str = "generic";

/// The environment variable that allows `--simulate` outside the live ISO's
/// own test boots (the development and self-check containers set it).
pub const ALLOW_ENV: &str = "ILEX_INSTALLER_SIMULATE_ALLOW";

/// The kernel argument that allows `--simulate` on the live ISO (a test boot).
pub const ALLOW_ARGUMENT: &str = "ilex.installer.simulate";

pub fn names() -> impl Iterator<Item = &'static str> {
    FIXTURES.iter().map(|(name, _)| *name)
}

/// May this process simulate? On a real boot the kernel command line has no
/// `ilex.installer.simulate`, so a person at the boot menu cannot make the
/// installer pretend by editing it with the usual arguments.
pub fn allowed(cmdline: &str) -> bool {
    std::env::var_os(ALLOW_ENV).is_some_and(|v| !v.is_empty())
        || cmdline
            .split_whitespace()
            .any(|word| word == ALLOW_ARGUMENT || word.starts_with("ilex.installer.simulate="))
}

/// The scenario a kernel argument names (`ilex.installer.simulate=<name>`).
pub fn scenario_from_cmdline(cmdline: &str) -> Option<&str> {
    cmdline
        .split_whitespace()
        .find_map(|word| word.strip_prefix("ilex.installer.simulate="))
}

pub fn cmdline() -> String {
    fs::read_to_string("/proc/cmdline").unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    description: String,
    probe: Option<Probe>,
    probe_error: Option<String>,
    plan: Option<Plan>,
    plan_error: Option<PlanError>,
    state: Option<State>,
}

/// Accounts the live image defines beyond the model's own reserved list: the
/// real helper reads them from its `/etc/passwd`, `/etc/group` and
/// `sysusers.d`.
const LIVE_IMAGE_NAMES: &[&str] = &["rpcuser", "nfsnobody", "geoclue", "flatpak", "cockpit-ws"];

pub struct Simulated {
    pub name: String,
    pub description: String,
    probe: Option<Probe>,
    probe_error: Option<String>,
    plan: Option<Plan>,
    plan_error: Option<PlanError>,
    state: Option<State>,
}

impl Simulated {
    pub fn load(name: &str) -> Result<Self> {
        let Some((_, text)) = FIXTURES.iter().find(|(n, _)| *n == name) else {
            bail!(
                "no simulation scenario '{name}'; available: {}",
                names().collect::<Vec<_>>().join(", ")
            );
        };
        let fixture: Fixture =
            serde_json::from_str(text).with_context(|| format!("fixture {name}"))?;
        Ok(Self {
            name: name.to_owned(),
            description: fixture.description,
            probe: fixture.probe,
            probe_error: fixture.probe_error,
            plan: fixture.plan,
            plan_error: fixture.plan_error,
            state: fixture.state,
        })
    }

    pub fn probe_document(&self) -> Option<&Probe> {
        self.probe.as_ref()
    }

    fn refuse(error: PlanError) -> ClientError {
        ClientError::Refused(error)
    }
}

impl Backend for Simulated {
    fn probe(&self) -> Result<Probe, ClientError> {
        if let Some(why) = &self.probe_error {
            return Err(ClientError::HelperUnavailable(why.clone()));
        }
        self.probe
            .clone()
            .ok_or_else(|| ClientError::BadAnswer("the fixture has no probe".into()))
    }

    fn plan(&self, request: &PlanRequest) -> Result<Plan, ClientError> {
        let probe = self.probe()?;
        let checked = check_request(request, &probe).map_err(Self::refuse)?;
        if LIVE_IMAGE_NAMES.contains(&request.administrator.as_str()) {
            return Err(Self::refuse(PlanError::new(
                ilex_installer_model::plan::ErrorCode::ReservedName,
                Some("administrator"),
                format!(
                    "{} is an account or group the system defines; pick a distinct maintenance name such as ilexmaint",
                    request.administrator
                ),
            )));
        }
        if let Some(error) = &self.plan_error {
            return Err(Self::refuse(error.clone()));
        }
        let mut plan = self
            .plan
            .clone()
            .ok_or_else(|| ClientError::BadAnswer("the fixture has no plan".into()))?;
        let tag = checked.composition.tag.clone();
        let tpm = checked.request.tpm_mode();
        let eligible = probe.enrollment["verdict"] == "eligible";
        plan.request = checked.request.clone();
        plan.composition.tag.clone_from(&tag);
        plan.composition.machine.clone_from(&checked.composition.machine);
        plan.composition.shells.clone_from(&checked.composition.shells);
        plan.composition.reason = probe
            .selection
            .as_ref()
            .filter(|s| s.tag == tag)
            .map(|s| s.reason.clone());
        plan.image.target = probe.catalogue.image_reference(&tag);
        plan.disk = PlannedDisk {
            id: checked.disk.id.clone(),
            path: checked.disk.path.clone(),
            model: checked.disk.model.clone(),
            serial: checked.disk.serial.clone(),
            size_bytes: checked.disk.size_bytes,
        };
        plan.erases = Erase {
            disk: checked.disk.id.clone(),
            partitions: checked.disk.partitions.iter().map(|p| p.path.clone()).collect(),
            systems: checked.disk.other_systems.clone(),
        };
        plan.encryption = PlannedEncryption {
            enabled: checked.request.encrypt,
            tpm: PlannedTpm {
                requested: tpm,
                will_enroll: checked.request.encrypt && tpm == TpmMode::Auto && eligible,
                reason: probe.enrollment["reason"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_owned(),
            },
        };
        plan.warnings = if checked.disk.other_systems.is_empty() {
            vec![]
        } else {
            vec![format!(
                "everything on {} will be erased, including {} other system(s) found on it",
                checked.disk.id,
                checked.disk.other_systems.len()
            )]
        };
        Ok(plan)
    }

    fn state(&self) -> Result<State, ClientError> {
        Ok(self
            .state
            .clone()
            .unwrap_or_else(|| State::idle(0)))
    }
}
