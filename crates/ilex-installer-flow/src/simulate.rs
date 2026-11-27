//! `--simulate <scenario>`: the helper's answers from JSON fixtures, so an
//! interface can be built, looked at and tested without a helper, a disk or a
//! registry. Each fixture holds a complete `Probe` document, a `Plan` to
//! return, and optionally a refusal, a failing probe or an installation's
//! `State`. The simulation still applies the model's own `check_request` to
//! every request, so a refusal on a page is the real rule's, not a canned one.
//!
//! A scenario name may carry modifiers after `+` (`generic+fail-image+sticks`):
//! how the installation that `Apply` starts goes (`success`, the default,
//! `stalled`, `fail-image`, `fail-partition`, `helper-stopped`, `lost`), the
//! state the helper is in when the interface starts (`attach-installing`,
//! `attach-done`, `attach-failed`), whether USB sticks are plugged in
//! (`sticks`, or `bare` for none), whether writing a log to one fails
//! (`log-fails`), how `Apply` answers (`apply-refused`, `apply-lost`) and
//! whether Restart and Power off fail (`finish-fails`); `early` holds an
//! installation in its first stage. They are
//! what the self-check and the screenshots use to reach every page and every
//! error state; a boot can name them only with `ilex.installer.simulate` (see
//! [`allowed`]).
//!
//! Fixtures are compiled in. A released ISO must not be talked into
//! pretending: `ilex-installer` refuses `--simulate` unless the kernel command
//! line carries `ilex.installer.simulate` or the environment variable
//! `ILEX_INSTALLER_SIMULATE_ALLOW` is set (the development and self-check
//! containers), see [`allowed`].

use crate::backend::Backend;
use crate::messages::ClientError;
use crate::secret::Secret;
use anyhow::{Context, Result, bail};
use ilex_installer_model::finish::{FinishAction, Saved, check_log_device};
use ilex_installer_model::plan::{
    ErrorCode, Erase, Plan, PlanError, PlanRequest, PlannedDisk, PlannedEncryption, PlannedTpm,
    SecretKind, TpmMode, check_request,
};
use ilex_installer_model::probe::{Disk, Partition, Probe};
use ilex_installer_model::state::{
    Outcome, Stage, StageRecord, StageStatus, State, Summary,
};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::fs;
use std::sync::Mutex;

const FIXTURES: &[(&str, &str)] = &[
    ("generic", include_str!("../fixtures/generic.json")),
    ("ms-7c35", include_str!("../fixtures/ms-7c35.json")),
    (
        "ms-7c35-board",
        include_str!("../fixtures/ms-7c35-board.json"),
    ),
    ("conflict", include_str!("../fixtures/conflict.json")),
    (
        "no-composition",
        include_str!("../fixtures/no-composition.json"),
    ),
    (
        "boot-override",
        include_str!("../fixtures/boot-override.json"),
    ),
    (
        "bad-override",
        include_str!("../fixtures/bad-override.json"),
    ),
    ("bios", include_str!("../fixtures/bios.json")),
    ("no-disk", include_str!("../fixtures/no-disk.json")),
    ("helper-down", include_str!("../fixtures/helper-down.json")),
    (
        "registry-down",
        include_str!("../fixtures/registry-down.json"),
    ),
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

/// How the installation that `Apply` starts goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Script {
    /// Every stage, then the outcome: success.
    Success,
    /// The image stage at 40 percent, for ever (a screenshot of the page).
    Stalled,
    /// The image stage fails after the disk was written.
    FailImage,
    /// The first stage fails before anything was written.
    FailPartition,
    /// The helper dies and its successor reports what it found.
    HelperStopped,
    /// The helper stops answering `State`.
    Lost,
    /// The partition stage, at 30 percent, for ever (a screenshot).
    Early,
}

/// What `Apply` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyMode {
    /// It begins the installation.
    Begins,
    /// It refuses: the plan is too old.
    Refuses,
    /// Its answer is lost, and nothing began.
    Lost,
}

/// The state the helper is in when the interface starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attach {
    Nothing,
    Installing,
    Done,
    Failed,
}

/// What a scenario's modifiers say.
#[derive(Clone, Debug)]
struct Modifiers {
    script: Script,
    attach: Attach,
    apply: ApplyMode,
    sticks: bool,
    /// No removable disk but the medium the installer started from.
    bare: bool,
    log_fails: bool,
    /// Restart and Power off fail.
    finish_fails: bool,
}

impl Modifiers {
    fn parse(words: &[&str]) -> Result<Self> {
        let mut modifiers = Self {
            script: Script::Success,
            attach: Attach::Nothing,
            apply: ApplyMode::Begins,
            sticks: false,
            bare: false,
            log_fails: false,
            finish_fails: false,
        };
        for word in words {
            match *word {
                "stalled" => modifiers.script = Script::Stalled,
                "fail-image" => modifiers.script = Script::FailImage,
                "fail-partition" => modifiers.script = Script::FailPartition,
                "helper-stopped" => modifiers.script = Script::HelperStopped,
                "lost" => modifiers.script = Script::Lost,
                "early" => modifiers.script = Script::Early,
                "apply-refused" => modifiers.apply = ApplyMode::Refuses,
                "apply-lost" => modifiers.apply = ApplyMode::Lost,
                "bare" => modifiers.bare = true,
                "finish-fails" => modifiers.finish_fails = true,
                "attach-installing" => modifiers.attach = Attach::Installing,
                "attach-done" => modifiers.attach = Attach::Done,
                "attach-failed" => modifiers.attach = Attach::Failed,
                "sticks" => modifiers.sticks = true,
                "log-fails" => modifiers.log_fails = true,
                other => bail!("no simulation modifier '{other}'"),
            }
        }
        Ok(modifiers)
    }
}

/// What the simulated helper remembers of an installation.
#[derive(Default)]
struct Session {
    /// The plan `plan` answered last: the one `Apply` accepts.
    plan: Option<Plan>,
    secrets: BTreeSet<String>,
    /// How many `State` requests the running installation has answered.
    step: Option<usize>,
    summary: Option<Summary>,
    applied: Option<String>,
}

pub struct Simulated {
    pub name: String,
    pub description: String,
    probe: Option<Probe>,
    probe_error: Option<String>,
    plan: Option<Plan>,
    plan_error: Option<PlanError>,
    state: Option<State>,
    modifiers: Modifiers,
    session: Mutex<Session>,
}

/// Two USB sticks, the way a probe lists them: one a log can be written to,
/// one with nothing on it.
fn sticks() -> Vec<Disk> {
    let stick = |id: &str, model: &str, serial: &str, partitions: Vec<Partition>| Disk {
        id: id.into(),
        path: format!("/dev/sd{}", serial[..1].to_lowercase()),
        model: model.into(),
        serial: serial.into(),
        size_bytes: 16_000_000_000,
        sector_size: 512,
        bus: "usb".into(),
        removable: true,
        live_medium: false,
        in_use: vec![],
        notes: vec![],
        partition_table: (!partitions.is_empty()).then(|| "gpt".to_owned()),
        partitions,
        free_extents: vec![],
        other_systems: vec![],
    };
    vec![
        stick(
            "usb-SanDisk_Cruzer_Blade_4C530001-0:0",
            "Cruzer Blade",
            "4C530001",
            vec![Partition {
                number: 1,
                path: "/dev/sdb1".into(),
                start_bytes: 1 << 20,
                size_bytes: 15_900_000_000,
                type_id: "ebd0a0a2-b9e5-4433-87c0-68b6b72699c7".into(),
                label: String::new(),
                filesystem: "vfat".into(),
                filesystem_label: "STICK".into(),
            }],
        ),
        stick(
            "usb-Kingston_DataTraveler_0123-0:0",
            "DataTraveler 3.0",
            "0123",
            vec![],
        ),
    ]
}

impl Simulated {
    pub fn load(name: &str) -> Result<Self> {
        let mut words = name.split('+');
        let base = words.next().unwrap_or_default();
        let modifiers = Modifiers::parse(&words.collect::<Vec<_>>())
            .with_context(|| format!("simulation scenario '{name}'"))?;
        let Some((_, text)) = FIXTURES.iter().find(|(n, _)| *n == base) else {
            bail!(
                "no simulation scenario '{base}'; available: {}",
                names().collect::<Vec<_>>().join(", ")
            );
        };
        let fixture: Fixture =
            serde_json::from_str(text).with_context(|| format!("fixture {base}"))?;
        let mut probe = fixture.probe;
        if let Some(probe) = &mut probe {
            if modifiers.bare {
                probe.disks.retain(|d| !d.removable || d.live_medium);
            }
            if modifiers.sticks {
                probe.disks.extend(sticks());
            }
        }
        let state = match modifiers.attach {
            Attach::Nothing => fixture.state,
            _ => None,
        };
        let mut simulated = Self {
            name: name.to_owned(),
            description: fixture.description,
            probe,
            probe_error: fixture.probe_error,
            plan: fixture.plan,
            plan_error: fixture.plan_error,
            state,
            modifiers,
            session: Mutex::default(),
        };
        simulated.start_attached();
        Ok(simulated)
    }

    /// The `attach-*` modifiers: the helper is already in the state the
    /// interface re-attaches to.
    fn start_attached(&mut self) {
        let attach = self.modifiers.attach;
        if attach == Attach::Nothing {
            return;
        }
        let (Some(probe), Some(plan)) = (&self.probe, &self.plan) else {
            return;
        };
        let mut plan = plan.clone();
        if let Some(disk) = probe.disks.iter().find(|d| d.id == plan.disk.id) {
            plan.disk.model.clone_from(&disk.model);
            plan.disk.serial.clone_from(&disk.serial);
        }
        let summary = summary_of(&plan, &probe.edition.name);
        let script = match attach {
            Attach::Installing => Script::Stalled,
            Attach::Done => Script::Success,
            _ => Script::FailImage,
        };
        let step = match attach {
            Attach::Installing => 3,
            Attach::Done => 99,
            _ => 3,
        };
        if let Ok(state) = script_state(script, step, &plan, &summary) {
            self.state = Some(state);
        }
    }

    pub fn probe_document(&self) -> Option<&Probe> {
        self.probe.as_ref()
    }

    fn refuse(error: PlanError) -> ClientError {
        ClientError::Refused(error)
    }

    fn session(&self) -> std::sync::MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What `Apply` records of a plan, as the helper does.
fn summary_of(plan: &Plan, edition: &str) -> Summary {
    Summary {
        edition: edition.to_owned(),
        composition: plan.composition.tag.clone(),
        version: plan.image.version.clone(),
        digest: plan.image.digest.clone(),
        disk: plan.disk.id.clone(),
        disk_label: format!("{} {}", plan.disk.model.trim(), plan.disk.serial.trim())
            .trim()
            .to_owned(),
    }
}

/// The helper's record of a scripted installation after `step` answers.
fn script_state(
    script: Script,
    step: usize,
    plan: &Plan,
    summary: &Summary,
) -> Result<State, ClientError> {
    let stages = Stage::UNENCRYPTED;
    // Which stage runs and how far, then what the tools printed.
    let (running, fraction, failing): (usize, f64, Option<Stage>) = match step {
        0 => (0, 0.3, None),
        1 => (1, 0.5, None),
        2 => (2, 0.1, None),
        3 => (2, 0.4, None),
        4 => (2, 0.8, None),
        5 => (3, 0.5, None),
        6 => (4, 0.5, None),
        _ => (5, 0.5, None),
    };
    let finished = step >= 8;
    let (running, fraction) = match script {
        Script::Stalled | Script::Lost => (2, 0.4),
        Script::Early => (0, 0.3),
        _ => (running, fraction),
    };
    let _ = failing;
    let stage = stages[running.min(stages.len() - 1)];
    let mut lines: Vec<String> = Vec::new();
    match stage {
        Stage::Partition => lines.extend([
            "$ wipefs --all --force /dev/disk/by-id/nvme-eui.0025385c1140a2b3".to_owned(),
            "$ systemd-repart --dry-run=no --empty=force --seed=… /dev/disk/by-id/nvme-eui.0025385c1140a2b3".to_owned(),
            "Created partition 1 of type esp and UUID 0b1a…, with 512M.".to_owned(),
        ]),
        Stage::Mount => lines.extend([
            "$ mount /dev/disk/by-id/nvme-eui.0025385c1140a2b3-part3 /run/ilex-installer/target".to_owned(),
        ]),
        Stage::Image => lines.extend([
            format!("$ bootc install to-filesystem --source-imgref {} …", plan.image.source),
            "Fetching ostree layers: layer 12 of 61".to_owned(),
            "Importing layer sha256:5e3b9d0c…".to_owned(),
            "Importing layer sha256:9a41f7c2…".to_owned(),
        ]),
        _ => lines.push(format!("stage {} runs", stage.name())),
    }
    let mut state = State::idle(1_700_000_000 + step as u64);
    state.plan = Some(plan.id.clone());
    state.applied = Some(plan.id.clone());
    state.summary = Some(summary.clone());
    state.started = Some(1_700_000_000);
    state.stage = stage;
    state.progress = fraction;
    state.message = match stage {
        Stage::Partition => "Writing the partition table".to_owned(),
        Stage::Mount => "Mounting the new partitions".to_owned(),
        Stage::Image => "Downloading and installing ilex".to_owned(),
        Stage::Configure => "Configuring the system".to_owned(),
        Stage::Account => "Creating the administrator account".to_owned(),
        _ => "Finishing and unmounting".to_owned(),
    };
    state.log = lines;
    state.active = true;
    state.disk_changed = running >= 1 || (running == 0 && fraction > 0.0);
    state.stages = stages
        .iter()
        .enumerate()
        .map(|(i, &stage)| StageRecord {
            stage,
            status: match i.cmp(&running) {
                std::cmp::Ordering::Less => StageStatus::Done,
                std::cmp::Ordering::Equal => StageStatus::Running,
                std::cmp::Ordering::Greater => StageStatus::Pending,
            },
            started: (i <= running).then_some(1_700_000_000 + i as u64),
            finished: (i < running).then_some(1_700_000_010 + i as u64),
        })
        .collect();
    let fail = |state: &mut State, stage: Stage, reason: &str, changed: bool| {
        state.active = false;
        state.stage = stage;
        state.disk_changed = changed;
        state.message = format!("The installation failed during the {} stage", stage.name());
        state.outcome = Some(Outcome::Failed {
            stage,
            reason: reason.to_owned(),
        });
        for record in &mut state.stages {
            if record.stage == stage {
                record.status = StageStatus::Failed;
            }
        }
    };
    match script {
        Script::Success if finished => {
            state.active = false;
            state.stage = Stage::Done;
            state.progress = 1.0;
            state.message = "The installation is complete".into();
            state.outcome = Some(Outcome::Success);
            state.disk_changed = true;
            for record in &mut state.stages {
                record.status = StageStatus::Done;
            }
        }
        Script::FailImage if step >= 3 => {
            state
                .log
                .push("error: Pulling image: reading manifest: connection reset by peer".into());
            fail(
                &mut state,
                Stage::Image,
                "bootc install failed (exit 1): Pulling image: reading manifest: connection reset by peer",
                true,
            );
        }
        Script::FailPartition if step >= 1 => {
            state.log = vec![
                "$ systemd-repart --dry-run=no --empty=force /dev/disk/by-id/nvme-eui.0025385c1140a2b3".into(),
                "Failed to open the device: Device or resource busy".into(),
            ];
            state.stage = Stage::Partition;
            state.stages = stages
                .iter()
                .map(|&stage| StageRecord {
                    stage,
                    status: StageStatus::Pending,
                    started: None,
                    finished: None,
                })
                .collect();
            state.stages[0].status = StageStatus::Running;
            fail(
                &mut state,
                Stage::Partition,
                "systemd-repart failed (exit 1): Failed to open the device: Device or resource busy",
                false,
            );
        }
        Script::HelperStopped if step >= 3 => {
            state.active = false;
            state.message = "The installer's helper stopped during the image stage; nothing resumes by itself.".into();
            state.outcome = Some(Outcome::Failed {
                stage: Stage::Image,
                reason: "the installer's helper stopped (it crashed or was stopped) during the image stage; the disk was already changed, so install again from the start".into(),
            });
            for record in &mut state.stages {
                if record.stage == Stage::Image {
                    record.status = StageStatus::Failed;
                }
            }
        }
        Script::Lost if step >= 3 => {
            return Err(ClientError::HelperUnavailable(
                "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
            ));
        }
        _ => {}
    }
    Ok(state)
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
                ErrorCode::ReservedName,
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
        plan.composition
            .machine
            .clone_from(&checked.composition.machine);
        plan.composition
            .shells
            .clone_from(&checked.composition.shells);
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
            partitions: checked
                .disk
                .partitions
                .iter()
                .map(|p| p.path.clone())
                .collect(),
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
        for partition in &mut plan.partitions {
            partition.encrypted = checked.request.encrypt && partition.kind == "root";
        }
        plan.warnings = if checked.disk.other_systems.is_empty() {
            vec![]
        } else {
            vec![format!(
                "everything on {} will be erased, including {} other system(s) found on it",
                checked.disk.id,
                checked.disk.other_systems.len()
            )]
        };
        self.session().plan = Some(plan.clone());
        Ok(plan)
    }

    fn state(&self) -> Result<State, ClientError> {
        let mut session = self.session();
        if let (Some(step), Some(plan), Some(summary)) =
            (session.step, session.plan.clone(), session.summary.clone())
        {
            session.step = Some(step + 1);
            return script_state(self.modifiers.script, step, &plan, &summary);
        }
        Ok(self.state.clone().unwrap_or_else(|| State::idle(0)))
    }

    fn set_secret(
        &self,
        plan: &str,
        kind: SecretKind,
        secret: &Secret,
    ) -> Result<(), ClientError> {
        let mut session = self.session();
        if session.plan.as_ref().map(|p| p.id.as_str()) != Some(plan) {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::UnknownPlan,
                Some("plan"),
                "the helper has no such plan",
            )));
        }
        let text = secret.expose();
        if kind != SecretKind::AdminPassword
            || text.is_empty()
            || text.contains(['\n', '\r', '\0'])
        {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::InvalidSecret,
                Some("fd"),
                "the password cannot be used",
            )));
        }
        session.secrets.insert(plan.to_owned());
        Ok(())
    }

    fn apply(&self, plan: &str, erase_disk: &str) -> Result<State, ClientError> {
        match self.modifiers.apply {
            ApplyMode::Begins => {}
            ApplyMode::Refuses => {
                return Err(Self::refuse(PlanError::new(
                    ErrorCode::PlanExpired,
                    Some("plan"),
                    "the plan is 16 minutes old and is valid for 15 minutes: plan again",
                )));
            }
            ApplyMode::Lost => {
                return Err(ClientError::HelperUnavailable(
                    "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
                ));
            }
        }
        let mut session = self.session();
        let Some(planned) = session.plan.clone().filter(|p| p.id == plan) else {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::UnknownPlan,
                Some("plan"),
                "the helper has no such plan: plan again",
            )));
        };
        if erase_disk != planned.disk.id {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::InvalidField,
                Some("erase_disk"),
                "whole-disk mode erases the disk: repeat its id to confirm",
            )));
        }
        if !session.secrets.contains(plan) {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::MissingSecret,
                Some("admin-password"),
                "the administrator's password has not been set (SetSecret)",
            )));
        }
        let edition = self
            .probe
            .as_ref()
            .map_or("ilex", |p| p.edition.name.as_str())
            .to_owned();
        let summary = summary_of(&planned, &edition);
        session.summary = Some(summary.clone());
        session.applied = Some(plan.to_owned());
        session.secrets.clear();
        session.step = Some(1);
        script_state(self.modifiers.script, 0, &planned, &summary)
    }

    fn finish(&self, action: &FinishAction) -> Result<Option<Saved>, ClientError> {
        match action {
            FinishAction::Restart | FinishAction::PowerOff if self.modifiers.finish_fails => {
                Err(ClientError::HelperUnavailable(
                    "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
                ))
            }
            FinishAction::Restart | FinishAction::PowerOff => Ok(None),
            FinishAction::SaveLog { device } => {
                let probe = self.probe()?;
                let target = self.session().summary.as_ref().map(|s| s.disk.clone());
                let (disk, _) =
                    check_log_device(&probe, device, target.as_deref()).map_err(Self::refuse)?;
                if self.modifiers.log_fails {
                    return Err(Self::refuse(PlanError::new(
                        ErrorCode::Internal,
                        Some("device"),
                        format!(
                            "the log could not be written to {device}: mount failed: wrong fs type, bad option, bad superblock"
                        ),
                    )));
                }
                let plan = self.session().applied.clone().unwrap_or_default();
                Ok(Some(Saved {
                    file: format!(
                        "ilex-installer-log-{}-1700000100.tar",
                        &plan[..8.min(plan.len())]
                    ),
                    device: device.clone(),
                    label: format!("{} {}", disk.model.trim(), disk.serial.trim())
                        .trim()
                        .to_owned(),
                    bytes: 48_128,
                }))
            }
        }
    }
}
