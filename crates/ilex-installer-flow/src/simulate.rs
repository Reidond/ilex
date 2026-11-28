//! `--simulate <scenario>`: the helper's answers from JSON fixtures, so an
//! interface can be built, looked at and tested without a helper, a disk or a
//! registry. Each fixture holds a complete `Probe` document, a `Plan` to
//! return, and optionally a refusal, a failing probe or an installation's
//! `State`. The simulation still applies the model's own `check_request` to
//! every request, so a refusal on a page is the real rule's, not a canned one.
//!
//! A scenario name may carry modifiers after `+` (`generic+fail-image+sticks`):
//! how the installation that `Apply` starts goes (`success`, the default,
//! `stalled`, `fail-image`, `fail-partition`, `helper-stopped`, `lost`,
//! `lost-then-done` and `lost-then-running`, where the helper is silent for a
//! while and then answers again), the
//! state the helper is in when the interface starts (`attach-installing`,
//! `attach-done`, `attach-failed`, `attach-starting`: an `Apply` that is still
//! checking, with an earlier installation's failure (another plan's) as the
//! rest of its record; `attach-starting-refused`: the same for three
//! questions, after which that `Apply` has been refused and the record is the
//! earlier failure again, with the refusal; `attach-starting-refused-idle`: the
//! same on a first installation, where the record is then idle and says
//! nothing of the refusal), whether USB sticks are plugged in
//! (`sticks`, or `bare` for none), whether writing a log to one fails
//! (`log-fails`), how `Apply` answers (`apply-refused`, `apply-lost`, and
//! `apply-slow`: the answer is lost and the helper goes on, saying `starting`
//! for a few questions and then installing; `apply-lost-other`: the answer is
//! lost and the helper is installing another plan; `apply-active`: it refuses
//! because an installation is active), whether `State` fails the first time it
//! is asked (`state-down`: a helper that is restarting) or the second
//! (`state-down-second`), whether the second look at the computer is refused
//! because an installation is active (`probe-active`), whether the
//! disk a plan was made for vanishes afterwards (`vanish`) and
//! whether Restart and Power off fail (`finish-fails`); `recovery` makes the
//! simulated helper make the recovery key of an encrypted installation whose
//! request asks for one (a fixed test key, [`TEST_KEY`]), hand it over once,
//! compare the whole key typed back and hold the installation in its recovery
//! stage until it matches or the person goes on without a key; `take-fails`
//! (the key cannot be fetched), `slot-refuses` (the volume refuses the right
//! key) and `skip-fails` (going on without a key is not answered) make the
//! helper fail those requests; `chip` makes this computer eligible for the
//! security chip (the fixtures' probes are not); `attach-key` and
//! `attach-key-shown` and `attach-key-acked` start the interface on an
//! installation whose key waits to be taken, was taken by an interface that is
//! gone, or was confirmed already; `encrypted` makes the
//! plan of an installation that is already running an encrypted one; `early` holds an
//! installation in its first stage; `plan-slow` answers the plan check after
//! [`PLAN_SLOW`] (a review page that stays frozen while the check runs, for a
//! trial to press keys in). They are
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
    Erase, ErrorCode, Plan, PlanError, PlanRequest, PlannedDisk, PlannedEncryption, PlannedTpm,
    SecretKind, TpmMode, check_request, passphrase_problem,
};
use ilex_installer_model::probe::{Disk, Partition, Probe};
use ilex_installer_model::recovery::{self, KeyStatus, TpmOutcome, TpmStatus};
use ilex_installer_model::selfcheck::TEST_KEY;
use ilex_installer_model::state::{
    Outcome, Refusal, Stage, StageRecord, StageStatus, State, Summary,
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
    /// The helper stops answering `State` for a while (ten requests, more than
    /// the interface's patience) and then finishes the installation.
    LostThenDone,
    /// The same silence, then the installation is where it was.
    LostThenRunning,
}

/// The `State` requests the `lost-then-*` scripts leave unanswered: from the
/// fourth (counting from zero) to the thirty-ninth, long enough that the
/// interface gives the helper up (eight requests) and shows the problem page
/// for a good while before the helper answers again.
const LOST_FROM: usize = 3;
const LOST_UNTIL: usize = 40;

/// What `Apply` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApplyMode {
    /// It begins the installation.
    Begins,
    /// It refuses: the plan is too old.
    Refuses,
    /// Its answer is lost, and nothing began.
    Lost,
    /// Its answer is lost, and the helper goes on: `State` says `starting` for
    /// a few questions, then the installation is running.
    Slow,
    /// Its answer is lost, and the helper is installing another plan.
    LostOther,
    /// It refuses: an installation is active already.
    Active,
}

/// How many `State` requests answer `starting` after an `apply-slow`.
const STARTING_POLLS: usize = 3;

/// The plan of the earlier installation whose failure the `attach-starting*`
/// scenarios keep under the `Apply` that is checking.
const EARLIER_PLAN: &str = "0123456789abcdef0123456789abcdef";

/// The plan of the installation another interface started (`apply-lost-other`).
const OTHER_PLAN: &str = "fedcba9876543210fedcba9876543210";

/// The state the helper is in when the interface starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attach {
    Nothing,
    Installing,
    Done,
    Failed,
    /// An `Apply` that is still checking, over an earlier installation's
    /// failure.
    Starting,
    /// The same for [`STARTING_POLLS`] questions, then the `Apply` has been
    /// refused (with the reason the helper records) and the earlier failure is
    /// all there is.
    StartingRefused,
    /// The same on a first installation: then the helper has recorded nothing
    /// and its state is idle.
    StartingRefusedIdle,
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
    /// The `State` request that fails, counting from zero (a helper that is
    /// restarting): the first at start-up, or the second.
    state_down: Option<usize>,
    /// The second look at the computer is refused: an installation is active.
    probe_active: bool,
    /// The disk a plan was made for is gone from every later probe.
    vanish: bool,
    /// The fixture's own plan (what an installation that is already running
    /// was made from) is an encrypted one. (Every plan `plan` makes follows its
    /// request.)
    encrypted: bool,
    /// The helper makes a recovery key (for a request that asks for one).
    recovery: bool,
    /// This computer is eligible for the security chip.
    chip: bool,
    /// The helper cannot hand the key over (three tries fail: the page offers
    /// going on without one).
    take_fails: bool,
    /// The key's keyslot refuses the right key (the volume's trouble).
    slot_refuses: bool,
    /// The helper does not answer `ContinueWithoutRecoveryKey`.
    skip_fails: bool,
    /// For the `attach-key*` scenarios: the key's status when the interface
    /// starts.
    key_at_start: Option<KeyStatus>,
    /// The plan check answers only after [`PLAN_SLOW`].
    plan_slow: bool,
}

/// How long `plan-slow` holds the plan check.
pub const PLAN_SLOW: std::time::Duration = std::time::Duration::from_secs(12);

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
            state_down: None,
            probe_active: false,
            vanish: false,
            encrypted: false,
            recovery: false,
            chip: false,
            take_fails: false,
            slot_refuses: false,
            skip_fails: false,
            key_at_start: None,
            plan_slow: false,
        };
        for word in words {
            match *word {
                "stalled" => modifiers.script = Script::Stalled,
                "fail-image" => modifiers.script = Script::FailImage,
                "fail-partition" => modifiers.script = Script::FailPartition,
                "helper-stopped" => modifiers.script = Script::HelperStopped,
                "lost" => modifiers.script = Script::Lost,
                "early" => modifiers.script = Script::Early,
                "lost-then-done" => modifiers.script = Script::LostThenDone,
                "lost-then-running" => modifiers.script = Script::LostThenRunning,
                "state-down" => modifiers.state_down = Some(0),
                "state-down-second" => modifiers.state_down = Some(1),
                "probe-active" => modifiers.probe_active = true,
                "vanish" => modifiers.vanish = true,
                "plan-slow" => modifiers.plan_slow = true,
                "apply-refused" => modifiers.apply = ApplyMode::Refuses,
                "apply-lost" => modifiers.apply = ApplyMode::Lost,
                "apply-slow" => modifiers.apply = ApplyMode::Slow,
                "apply-lost-other" => modifiers.apply = ApplyMode::LostOther,
                "apply-active" => modifiers.apply = ApplyMode::Active,
                "bare" => modifiers.bare = true,
                "encrypted" => modifiers.encrypted = true,
                "recovery" => modifiers.recovery = true,
                "chip" => modifiers.chip = true,
                "take-fails" => modifiers.take_fails = true,
                "slot-refuses" => modifiers.slot_refuses = true,
                "skip-fails" => modifiers.skip_fails = true,
                "attach-key" => {
                    modifiers.attach = Attach::Installing;
                    modifiers.recovery = true;
                    modifiers.encrypted = true;
                    modifiers.key_at_start = Some(KeyStatus::Waiting);
                }
                "attach-key-acked" => {
                    // Confirmed already: it stays where it is, for a page to look
                    // at.
                    modifiers.script = Script::Stalled;
                    modifiers.attach = Attach::Installing;
                    modifiers.recovery = true;
                    modifiers.encrypted = true;
                    modifiers.key_at_start = Some(KeyStatus::Acknowledged);
                }
                "attach-key-shown" => {
                    modifiers.attach = Attach::Installing;
                    modifiers.recovery = true;
                    modifiers.encrypted = true;
                    modifiers.key_at_start = Some(KeyStatus::Shown);
                }
                "finish-fails" => modifiers.finish_fails = true,
                "attach-installing" => modifiers.attach = Attach::Installing,
                "attach-done" => modifiers.attach = Attach::Done,
                "attach-failed" => modifiers.attach = Attach::Failed,
                "attach-starting" => modifiers.attach = Attach::Starting,
                "attach-starting-refused" => modifiers.attach = Attach::StartingRefused,
                "attach-starting-refused-idle" => modifiers.attach = Attach::StartingRefusedIdle,
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
    secrets: BTreeSet<(String, SecretKind)>,
    /// How many `State` requests the running installation has answered.
    step: Option<usize>,
    summary: Option<Summary>,
    applied: Option<String>,
    /// How many more `State` requests say that an `Apply` is checking (the
    /// plan it names), after which the installation runs, or (when `refused`
    /// is set) it was refused and the record is what `Simulated::state` holds.
    starting: Option<(String, usize)>,
    /// The `Apply` that is checking is refused when its questions are over, and
    /// the helper records why (for the plan in `refused_plan`).
    refused: Option<PlanError>,
    refused_plan: Option<String>,
    /// The helper is installing this other plan (the answer to `Apply` was
    /// lost, and it was not this interface's).
    other: Option<(Plan, Summary)>,
    /// How many times `State` and `Probe` have been asked.
    asked: usize,
    probes: usize,
    /// The recovery key, as the helper holds it: waiting, taken, confirmed.
    key: Option<KeyStatus>,
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
            if modifiers.chip {
                // A computer the security chip can unlock: what `ilex-enroll`'s
                // probe says of one.
                probe.enrollment = serde_json::json!({
                    "schema": 1, "secure_boot": true, "setup_mode": false, "tpm2": true,
                    "verdict": "eligible", "reason": "pcr7-sha256"
                });
            }
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
            plan: fixture.plan.map(|mut plan| {
                // The fixtures' plans are unencrypted unless the scenario says
                // so (a plan the interface asks for follows its request).
                let encrypted = modifiers.encrypted;
                plan.request.encrypt = encrypted;
                plan.encryption.enabled = encrypted;
                plan.request.recovery_key = encrypted && modifiers.recovery;
                plan.encryption.recovery_key = encrypted && modifiers.recovery;
                plan.encryption.tpm.will_enroll = encrypted && modifiers.recovery && modifiers.chip;
                for partition in &mut plan.partitions {
                    partition.encrypted = encrypted && partition.kind.starts_with("root");
                }
                plan
            }),
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
        // An installation that ended had its key confirmed.
        let key = self.modifiers.key_at_start.or_else(|| {
            (attach == Attach::Done && plan.encryption.recovery_key)
                .then_some(KeyStatus::Acknowledged)
        });
        let script = match attach {
            // With a key under way the installation really goes on (it waits for
            // the confirmation, then ends); once confirmed it stays where it is
            // (a page to look at).
            Attach::Installing if key.is_some() && key != Some(KeyStatus::Acknowledged) => {
                Script::Success
            }
            Attach::Installing => Script::Stalled,
            Attach::Done => Script::Success,
            _ => Script::FailImage,
        };
        let step = match attach {
            Attach::Installing => 3,
            Attach::Done => 99,
            _ => 3,
        };
        let starting = matches!(
            attach,
            Attach::Starting | Attach::StartingRefused | Attach::StartingRefusedIdle
        );
        if attach == Attach::StartingRefusedIdle {
            // A first installation: there is no earlier record under it.
            self.state = None;
        } else if let Ok(mut state) = script_state(script, step, &plan, &summary, key) {
            if starting {
                // The rest of the record is an earlier installation's failure
                // (another plan's): what an interface must not show, and must
                // not take for the outcome of the one being checked.
                state.applied = Some(EARLIER_PLAN.to_owned());
            }
            self.state = Some(state);
        }
        if key.is_some() {
            // The helper is running this installation: it goes on from here.
            let session = self.session.get_mut().unwrap_or_else(|e| e.into_inner());
            session.key = key;
            session.plan = Some(plan.clone());
            session.applied = Some(plan.id.clone());
            session.summary = Some(summary.clone());
            session.step = Some(step + 1);
        }
        if starting {
            let session = self.session.get_mut().unwrap_or_else(|e| e.into_inner());
            if attach == Attach::Starting {
                // For ever (a screenshot): the mark is part of the fixture's
                // record.
                let state = self.state.get_or_insert_with(|| State::idle(0));
                state.starting = Some(plan.id.clone());
            } else {
                session.starting = Some((plan.id.clone(), STARTING_POLLS));
                session.refused = Some(PlanError::new(
                    ErrorCode::PlanExpired,
                    Some("plan"),
                    "the plan is 16 minutes old and is valid for 15 minutes: plan again",
                ));
                session.refused_plan = Some(plan.id.clone());
            }
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
        encrypted: plan.encryption.enabled,
    }
}

/// The helper's record of a scripted installation after `step` answers.
fn script_state(
    script: Script,
    step: usize,
    plan: &Plan,
    summary: &Summary,
    key: Option<KeyStatus>,
) -> Result<State, ClientError> {
    // An encrypted plan's list has the encrypt stage after the partition
    // stage; the script's second step is that stage there (the mount stage,
    // which follows, is over by the time the image is pulled).
    let encrypted = plan.encryption.enabled;
    let stages = Stage::of_plan(plan);
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
        Script::Stalled | Script::Lost | Script::LostThenRunning => (2, 0.4),
        Script::Early => (0, 0.3),
        _ => (running, fraction),
    };
    let _ = failing;
    let stage = Stage::UNENCRYPTED[running.min(Stage::UNENCRYPTED.len() - 1)];
    let stage = if encrypted && stage == Stage::Mount {
        Stage::Encrypt
    } else {
        stage
    };
    // With a recovery key the installation waits, after the account, for the
    // whole key (the recovery stage), and goes on when it matches or the person
    // gives it up.
    let waiting_key = plan.encryption.recovery_key
        && stage == Stage::Finalise
        && !matches!(key, Some(KeyStatus::Acknowledged | KeyStatus::Declined));
    let stage = if waiting_key { Stage::Recovery } else { stage };
    let finished = finished && !waiting_key;
    let (fraction, running) = if waiting_key {
        (0.0, stages.iter().position(|s| *s == stage).unwrap_or(0))
    } else {
        (
            fraction,
            stages.iter().position(|s| *s == stage).unwrap_or(0),
        )
    };
    let mut lines: Vec<String> = Vec::new();
    match stage {
        Stage::Partition => lines.extend([
            "$ wipefs --all --force /dev/disk/by-id/nvme-eui.0025385c1140a2b3".to_owned(),
            "$ systemd-repart --dry-run=no --empty=force --seed=… /dev/disk/by-id/nvme-eui.0025385c1140a2b3".to_owned(),
            "Created partition 1 of type esp and UUID 0b1a…, with 512M.".to_owned(),
        ]),
        Stage::Encrypt => lines.extend([
            "$ cryptsetup luksUUID /dev/disk/by-id/nvme-eui.0025385c1140a2b3-part3".to_owned(),
            "LUKS2 volume 0b5d9a3e-6a4e-4a2c-9d0e-2f7c7b1c9f11: aes-xts-plain64, 512-bit payload key, one keyslot derived with argon2id, 4096-byte sectors".to_owned(),
            "$ cryptsetup open --type luks2 --allow-discards --key-file /proc/self/fd/7 /dev/disk/by-id/nvme-eui.0025385c1140a2b3-part3 luks-0b5d9a3e-6a4e-4a2c-9d0e-2f7c7b1c9f11".to_owned(),
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
        Stage::Encrypt => "Opening the encrypted volume".to_owned(),
        Stage::Mount => "Mounting the new partitions".to_owned(),
        Stage::Image => "Downloading and installing ilex".to_owned(),
        Stage::Configure => "Configuring the system".to_owned(),
        Stage::Account => "Creating the administrator account".to_owned(),
        Stage::Recovery => "Waiting for you to confirm the recovery key".to_owned(),
        _ => "Finishing and unmounting".to_owned(),
    };
    if plan.encryption.recovery_key && step >= 2 {
        state.security.recovery_key = key;
    }
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
        Script::LostThenDone | Script::LostThenRunning
            if (LOST_FROM..LOST_UNTIL).contains(&step) =>
        {
            return Err(ClientError::HelperUnavailable(
                "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
            ));
        }
        Script::Success | Script::LostThenDone if finished => {
            state.active = false;
            state.stage = Stage::Done;
            state.progress = 1.0;
            state.message = "The installation is complete".into();
            state.outcome = Some(Outcome::Success);
            state.disk_changed = true;
            for record in &mut state.stages {
                record.status = StageStatus::Done;
            }
            if plan.encryption.enabled {
                state.security.tpm = Some(if key == Some(KeyStatus::Declined) {
                    // Went on without a recovery key: passphrase only, as the
                    // helper ends it.
                    TpmOutcome {
                        status: TpmStatus::PassphraseOnly,
                        reason: "recovery-key-failed".into(),
                    }
                } else if plan.encryption.tpm.will_enroll {
                    TpmOutcome {
                        status: TpmStatus::EnrolledUnverified,
                        reason: "pcr7-sha256".into(),
                    }
                } else {
                    TpmOutcome {
                        status: TpmStatus::PassphraseOnly,
                        reason: plan.encryption.tpm.reason.clone(),
                    }
                });
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
            state.message =
                "The installer's helper stopped during the image stage; nothing resumes by itself."
                    .into();
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

impl Simulated {
    /// The look at the computer, as `Probe` and `Plan` (which makes its own)
    /// answer it.
    fn read_probe(&self) -> Result<Probe, ClientError> {
        if let Some(why) = &self.probe_error {
            return Err(ClientError::HelperUnavailable(why.clone()));
        }
        let mut probe = self
            .probe
            .clone()
            .ok_or_else(|| ClientError::BadAnswer("the fixture has no probe".into()))?;
        if self.modifiers.vanish
            && let Some(plan) = &self.session().plan
        {
            // The disk that was planned has been unplugged since.
            probe.disks.retain(|d| d.id != plan.disk.id);
        }
        Ok(probe)
    }
}

impl Backend for Simulated {
    fn probe(&self) -> Result<Probe, ClientError> {
        if self.modifiers.probe_active {
            let mut session = self.session();
            session.probes += 1;
            if session.probes == 2 {
                // The second look is refused: an installation is active (the
                // `State` that follows says whether it is).
                return Err(Self::refuse(PlanError::new(
                    ErrorCode::InstallationActive,
                    None,
                    "an installation is running; wait for it to finish",
                )));
            }
        }
        self.read_probe()
    }

    fn plan(&self, request: &PlanRequest) -> Result<Plan, ClientError> {
        if self.modifiers.plan_slow {
            std::thread::sleep(PLAN_SLOW);
        }
        let probe = self.read_probe()?;
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
        if let Some(ilex_installer_model::plan::DailyUser::Custom { name, .. }) = &request.daily
            && LIVE_IMAGE_NAMES.contains(&name.as_str())
        {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::ReservedName,
                Some("daily"),
                format!(
                    "{name} is an account or group the system defines; pick another daily user name"
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
        // As the helper decides it: a key only when asked for (and the simulated
        // helper makes one), the chip only with a key, on an eligible computer.
        let recovery_key =
            checked.request.encrypt && checked.request.recovery_key && self.modifiers.recovery;
        let eligible = probe.enrollment["verdict"] == "eligible";
        let will_enroll = recovery_key && tpm == TpmMode::Auto && eligible;
        let reason = if tpm == TpmMode::Off {
            "not-requested".to_owned()
        } else if !eligible || recovery_key {
            probe.enrollment["reason"]
                .as_str()
                .unwrap_or("unknown")
                .to_owned()
        } else {
            "no-recovery-key".to_owned()
        };
        plan.encryption = PlannedEncryption {
            enabled: checked.request.encrypt,
            recovery_key,
            tpm: PlannedTpm {
                requested: tpm,
                will_enroll,
                reason: reason.clone(),
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
        if checked.request.encrypt && tpm == TpmMode::Auto && !will_enroll {
            plan.warnings.push(format!(
                "unlocking the disk with this computer's security chip is not used ({}); the disk asks for its passphrase at every start",
                recovery::tpm_reason(&reason)
            ));
        }
        self.session().plan = Some(plan.clone());
        Ok(plan)
    }

    fn state(&self) -> Result<State, ClientError> {
        let mut session = self.session();
        let call = session.asked;
        session.asked += 1;
        if self.modifiers.state_down == Some(call) {
            return Err(ClientError::HelperUnavailable(
                "org.freedesktop.DBus.Error.ServiceUnknown: the helper is starting".into(),
            ));
        }
        if let Some((plan, polls)) = session.starting.clone() {
            // `Apply` is still checking: the rest of the record is the one
            // before it, with the mark that it is not this installation's.
            if polls > 0 {
                session.starting = Some((plan.clone(), polls - 1));
                let mut state = self.state.clone().unwrap_or_else(|| State::idle(0));
                state.starting = Some(plan);
                return Ok(state);
            }
            session.starting = None;
            if let Some(error) = session.refused.take() {
                // It was refused, and nothing began: the record is the one
                // before it. A first installation's helper (restarted, or
                // one that records nothing) says nothing of why.
                let mut state = self.state.clone().unwrap_or_else(|| State::idle(0));
                if self.modifiers.attach == Attach::StartingRefused {
                    state.refusal = session
                        .refused_plan
                        .clone()
                        .map(|plan| Refusal { plan, error });
                }
                return Ok(state);
            }
            session.step = Some(0);
        }
        if let Some((plan, summary)) = session.other.clone() {
            // Another interface's installation is running.
            return script_state(Script::Stalled, 3, &plan, &summary, None);
        }
        if let (Some(step), Some(plan), Some(summary)) =
            (session.step, session.plan.clone(), session.summary.clone())
        {
            session.step = Some(step + 1);
            // The helper has made the key once the volume is open.
            if plan.encryption.recovery_key && step >= 2 && session.key.is_none() {
                session.key = Some(KeyStatus::Waiting);
            }
            return script_state(self.modifiers.script, step, &plan, &summary, session.key);
        }
        Ok(self.state.clone().unwrap_or_else(|| State::idle(0)))
    }

    fn set_secret(&self, plan: &str, kind: SecretKind, secret: &Secret) -> Result<(), ClientError> {
        let mut session = self.session();
        if session.plan.as_ref().map(|p| p.id.as_str()) != Some(plan) {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::UnknownPlan,
                Some("plan"),
                "the helper has no such plan",
            )));
        }
        let text = secret.expose();
        let usable = match kind {
            SecretKind::AdminPassword => !text.is_empty() && !text.contains(['\n', '\r', '\0']),
            SecretKind::DiskPassphrase => passphrase_problem(text).is_none(),
        };
        if !usable {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::InvalidSecret,
                Some("fd"),
                "the password cannot be used",
            )));
        }
        if kind == SecretKind::DiskPassphrase
            && !session.plan.as_ref().is_some_and(|p| p.encryption.enabled)
        {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::InvalidSecret,
                Some("fd"),
                "this plan is not encrypted: it has no passphrase",
            )));
        }
        session.secrets.insert((plan.to_owned(), kind));
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
            ApplyMode::Active => {
                return Err(Self::refuse(PlanError::new(
                    ErrorCode::InstallationActive,
                    None,
                    "an installation is already running",
                )));
            }
            ApplyMode::LostOther => {
                // Another `Apply` (another plan) took the helper's lock first
                // and is installing; this one never got its answer.
                let mut session = self.session();
                if let Some(mut other) = session.plan.clone() {
                    other.id = OTHER_PLAN.to_owned();
                    let edition = self
                        .probe
                        .as_ref()
                        .map_or("ilex", |p| p.edition.name.as_str());
                    let summary = summary_of(&other, edition);
                    session.other = Some((other, summary));
                }
                return Err(ClientError::HelperUnavailable(
                    "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
                ));
            }
            ApplyMode::Slow => {}
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
        if !session
            .secrets
            .contains(&(plan.to_owned(), SecretKind::AdminPassword))
        {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::MissingSecret,
                Some("admin-password"),
                "the administrator's password has not been set (SetSecret)",
            )));
        }
        if planned.encryption.enabled
            && !session
                .secrets
                .contains(&(plan.to_owned(), SecretKind::DiskPassphrase))
        {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::MissingSecret,
                Some("disk-passphrase"),
                "the disk's passphrase has not been set (SetSecret)",
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
        if self.modifiers.apply == ApplyMode::Slow {
            // The helper took the `Apply` and is checking; whoever called it
            // gets no answer (the call is lost, or the interface was killed).
            session.starting = Some((plan.to_owned(), STARTING_POLLS));
            session.step = None;
            return Err(ClientError::HelperUnavailable(
                "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
            ));
        }
        session.step = Some(1);
        session.key = None;
        script_state(self.modifiers.script, 0, &planned, &summary, None)
    }

    fn take_recovery_key(&self, plan: &str) -> Result<Secret, ClientError> {
        let mut session = self.session();
        let known = session.applied.as_deref() == Some(plan)
            && session
                .plan
                .as_ref()
                .is_some_and(|p| p.encryption.recovery_key);
        match (known, session.key) {
            (false, _) | (_, None) => Err(Self::refuse(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to take",
            ))),
            (true, Some(KeyStatus::Waiting)) if self.modifiers.take_fails => {
                Err(ClientError::HelperUnavailable(
                    "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
                ))
            }
            (true, Some(KeyStatus::Waiting)) => {
                session.key = Some(KeyStatus::Shown);
                Ok(Secret::new(TEST_KEY))
            }
            (true, Some(KeyStatus::Declined)) => Err(Self::refuse(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "the installation goes on without a recovery key",
            ))),
            (true, Some(_)) => Err(Self::refuse(PlanError::new(
                ErrorCode::RecoveryKeyTaken,
                Some("plan"),
                "the recovery key was handed over once and is not shown again",
            ))),
        }
    }

    fn acknowledge_recovery_key(&self, plan: &str, answer: &Secret) -> Result<(), ClientError> {
        let mut session = self.session();
        let mismatch =
            |why: &str| Self::refuse(PlanError::new(ErrorCode::RecoveryKeyMismatch, None, why));
        if session.applied.as_deref() != Some(plan) || session.key.is_none() {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to confirm",
            )));
        }
        if session.key == Some(KeyStatus::Waiting) {
            return Err(mismatch("the recovery key has not been shown yet"));
        }
        if session.key == Some(KeyStatus::Declined) {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "the installation goes on without a recovery key",
            )));
        }
        if !recovery::matches(TEST_KEY, answer.expose()) {
            return Err(mismatch("that is not the recovery key"));
        }
        if self.modifiers.slot_refuses {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::Internal,
                None,
                "the recovery key does not open its keyslot: Keyslot open failed",
            )));
        }
        session.key = Some(KeyStatus::Acknowledged);
        Ok(())
    }

    fn continue_without_recovery_key(&self, plan: &str) -> Result<(), ClientError> {
        let mut session = self.session();
        if session.applied.as_deref() != Some(plan) {
            return Err(Self::refuse(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to skip",
            )));
        }
        if self.modifiers.skip_fails {
            return Err(ClientError::HelperUnavailable(
                "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
            ));
        }
        match session.key {
            None => Err(Self::refuse(PlanError::new(
                ErrorCode::NoRecoveryKey,
                Some("plan"),
                "this installation has no recovery key to skip",
            ))),
            Some(KeyStatus::Acknowledged) => Err(Self::refuse(PlanError::new(
                ErrorCode::RecoveryKeyConfirmed,
                Some("plan"),
                "the recovery key was confirmed: the installation keeps it",
            ))),
            Some(_) => {
                session.key = Some(KeyStatus::Declined);
                Ok(())
            }
        }
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
                let probe = self.read_probe()?;
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
