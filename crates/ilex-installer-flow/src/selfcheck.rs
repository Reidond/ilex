//! The flow's share of `ilex-installer --self-check`: its rules exercised on
//! the simulation's fixtures, and the system's keyboard catalogue read. (The
//! repository has no unit tests; anything worth testing in a shipped binary is
//! observable from inside the VM.)

use crate::account::{self, Check};
use crate::backend::Backend;
use crate::flow::{Flow, PageId, PlanCheck, Step, usable_disks};
use crate::keyboard::{self, Layouts, Sources};
use crate::language;
use crate::machine::{MachineView, Origin};
use crate::messages::{ClientError, plan_error};
use crate::progress::{Progress, Status};
use crate::secret::Secret;
use crate::simulate::{self, Simulated};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::plan::{ErrorCode, PlanError};
use ilex_installer_model::state::{Outcome, Stage, State};
use std::collections::BTreeSet;

/// A small keyboard catalogue in xkeyboard-config's own format, for the
/// parser's rules (the live image's real files are read by [`system`]).
const RULES: &str = "\
! model
  pc105           Generic 105-key PC
! layout
  us              English (US)
  de              German
  fr              French
! variant
  intl            us: English (US, intl., with dead keys)
  nodeadkeys      de: German (no dead keys)
  oss             fr: French (alt.)
! option
  grp             Switching to another layout
";

fn maps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

/// Everything that needs no system file.
pub fn run() -> Result<String> {
    // Keyboard catalogue: layouts and variants named by the rules list, only
    // keymaps that exist, each with the XKB source GNOME takes.
    let layouts = Layouts::build(
        RULES,
        &maps(&[
            "us",
            "de",
            "de-nodeadkeys",
            "fr-oss",
            "us-intl",
            "xx",
            "de-nosuchvariant",
        ]),
    )?;
    ensure!(
        layouts.len() == 5,
        "the catalogue offers {} layouts, expected 5 (a keymap whose layout or variant xkeyboard-config does not name is not offered)",
        layouts.len()
    );
    let de = layouts.get("de-nodeadkeys").context("de-nodeadkeys")?;
    ensure!(
        de.name == "German (no dead keys)" && de.gnome_source() == "de+nodeadkeys",
        "de-nodeadkeys is {de:?}"
    );
    ensure!(
        layouts.get("us").is_some_and(|l| l.gnome_source() == "us") && layouts.default_id() == "us",
        "us is not the plain layout and the default"
    );
    ensure!(
        layouts
            .all()
            .iter()
            .any(|l| Layouts::matches(l, "german dead"))
            && !layouts.all().iter().any(|l| Layouts::matches(l, "klingon")),
        "layout search"
    );

    // Languages: every entry is a well-formed UTF-8 locale and offered once.
    let mut seen = BTreeSet::new();
    for language in language::LANGUAGES {
        ensure!(
            ilex_installer_model::plan::is_language(language.locale)
                && seen.insert(language.locale),
            "language {} is malformed or repeated",
            language.locale
        );
    }
    ensure!(
        language::find(language::DEFAULT).is_some(),
        "no default language"
    );

    // Messages: every refusal the helper can make has a title.
    for code in [
        ErrorCode::InvalidRequest,
        ErrorCode::InvalidField,
        ErrorCode::UnsupportedFirmware,
        ErrorCode::UnknownComposition,
        ErrorCode::UnknownDisk,
        ErrorCode::LiveMedium,
        ErrorCode::InUse,
        ErrorCode::DiskTooSmall,
        ErrorCode::UnsupportedMode,
        ErrorCode::TpmNeedsEncryption,
        ErrorCode::ReservedName,
        ErrorCode::Registry,
        ErrorCode::ImageNotFound,
        ErrorCode::Repart,
        ErrorCode::Internal,
    ] {
        let message = plan_error(&PlanError::new(code, None, "because"));
        ensure!(!message.title.is_empty(), "{code:?} has no message");
    }

    // The account page's rules.
    let profiles: BTreeSet<&str> = ["andriishafar"].into();
    for (name, valid) in [
        ("ilexmaint", true),
        ("a", true),
        ("_svc-1", true),
        ("", false),
        ("Ilexmaint", false),
        ("1admin", false),
        ("has space", false),
        ("root", false),
        ("systemd-foo", false),
        ("andriishafar", false),
        ("abcdefghijklmnopqrstuvwxyzabcdefg", false),
    ] {
        let check = account::name(name, &profiles);
        ensure!(
            check.is_valid() == valid,
            "administrator name {name:?} is {check:?}, expected {}",
            if valid { "valid" } else { "refused" }
        );
    }
    ensure!(
        account::name("", &profiles) == Check::Empty
            && account::password(&Secret::new("")) == Check::Empty
            && account::password(&Secret::new("two\nlines"))
                .message()
                .is_some()
            && account::password(&Secret::new(&"x".repeat(256)))
                .message()
                .is_some()
            && account::password(&Secret::new("correct horse")).is_valid()
            && account::confirmation(&Secret::new("a"), &Secret::new("b"))
                .message()
                .is_some()
            && account::confirmation(&Secret::new("a"), &Secret::new("a")).is_valid()
            && account::password_advice(&Secret::new("short")).is_some()
            && account::password_advice(&Secret::new("a longer one")).is_none(),
        "the password rules"
    );

    // Every scenario loads, and the machine page reads each as the model's
    // detection says.
    let expect: [(&str, Option<&str>, &str); 9] = [
        ("generic", None, "not-recognised"),
        ("ms-7c35", Some("ms-7c35.gnome"), "detected"),
        ("ms-7c35-board", Some("ms-7c35.gnome"), "detected"),
        ("conflict", Some("generic.gnome"), "conflict"),
        ("no-composition", Some("generic.gnome"), "no-composition"),
        ("boot-override", Some("ms-7c35.gnome"), "chosen"),
        ("bad-override", Some("generic.gnome"), "bad-override"),
        ("bios", None, "not-recognised"),
        ("no-disk", None, "not-recognised"),
    ];
    for (name, preselected, origin) in expect {
        let scenario = Simulated::load(name)?;
        let probe = scenario.probe()?;
        probe
            .catalogue
            .validate()
            .with_context(|| format!("{name}: catalogue"))?;
        let view = MachineView::from_probe(&probe);
        ensure!(
            view.origin.word() == origin
                && preselected.is_none_or(|tag| view.preselected == tag)
                && view.choices.len() == probe.catalogue.compositions.len(),
            "{name}: the machine page reads {:?} preselecting {}",
            view.origin,
            view.preselected
        );
        let notice = view.notice();
        ensure!(!notice.message.title.is_empty(), "{name}: no notice");
    }
    let unavailable = Simulated::load("helper-down")?;
    ensure!(
        matches!(unavailable.probe(), Err(ClientError::HelperUnavailable(_))),
        "helper-down does not fail the probe"
    );
    for name in simulate::names() {
        Simulated::load(name)?;
    }

    // The flow end to end on the generic scenario.
    walk()?;
    gates()?;
    progress()?;
    Ok(format!(
        "{} scenarios, {} sample layouts, {} languages",
        simulate::names().count(),
        layouts.len(),
        language::LANGUAGES.len()
    ))
}

fn flow_on(name: &str) -> Result<(Flow, Simulated)> {
    let sim = Simulated::load(name)?;
    let mut flow = Flow::new(Layouts::build(
        RULES,
        &maps(&["us", "de", "de-nodeadkeys"]),
    )?);
    flow.set_probe(sim.probe());
    Ok((flow, sim))
}

fn walk() -> Result<()> {
    let (mut flow, sim) = flow_on("generic")?;
    ensure!(
        flow.page() == PageId::Welcome && flow.can_continue(),
        "welcome does not continue"
    );
    ensure!(
        flow.set_keyboard("de-nodeadkeys")
            && flow.set_language("de_DE.UTF-8")
            && !flow.set_keyboard("nope"),
        "welcome choices"
    );
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine),
        "welcome -> machine"
    );
    ensure!(
        flow.machine_tag() == Some("generic.gnome")
            && flow.composition_line().as_deref()
                == Some(
                    "ILEX-COMPOSITION generic.gnome (no machine configuration matches this computer) installs ghcr.io/reidond/ilex:generic.gnome"
                ),
        "machine page preselection and its serial line: {:?}",
        flow.composition_line()
    );
    ensure!(
        flow.select_machine("ms-7c35.gnome") && !flow.select_machine("ms-7c35.kde"),
        "machine choice"
    );
    ensure!(
        flow.composition_line().as_deref()
            == Some(
                "ILEX-COMPOSITION ms-7c35.gnome (chosen in the installer) installs ghcr.io/reidond/ilex:ms-7c35.gnome"
            ),
        "the line after the person's own choice"
    );
    ensure!(
        flow.advance() == Step::Goto(PageId::Account),
        "machine -> account"
    );
    ensure!(!flow.can_continue(), "account continues empty");
    flow.set_administrator("root");
    ensure!(
        flow.account_checks().name.message().is_some(),
        "root is accepted"
    );
    flow.set_administrator("ilexmaint");
    flow.set_password("correct horse");
    ensure!(
        !flow.can_continue(),
        "account continues without a confirmation"
    );
    flow.set_confirmation("correct hose");
    ensure!(
        flow.account_checks().confirmation.message().is_some(),
        "mismatch not noticed"
    );
    flow.set_confirmation("correct horse");
    ensure!(flow.can_continue(), "a valid account does not continue");
    let Step::RunPlan(request) = flow.advance() else {
        bail!("account does not ask for the plan check");
    };
    ensure!(
        request.administrator == "ilexmaint"
            && request.keyboard == "de-nodeadkeys"
            && request.language == "de_DE.UTF-8"
            && request.composition == "ms-7c35.gnome"
            && request.disk == "nvme-eui.0025385c1140a2b3",
        "the plan request: {request:?}"
    );
    ensure!(
        !serde_json::to_string(&*request)?.contains("horse"),
        "the password is in the plan request"
    );
    ensure!(
        !flow.can_continue() && !flow.can_go_back(),
        "buttons while the plan is checked"
    );
    ensure!(
        flow.plan_finished(sim.plan(&request)) == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::Checked(_)),
        "plan result"
    );
    ensure!(flow.back() == Some(PageId::Account), "back from ready");

    // The helper's own refusal of a name the live image defines stays on the
    // account page, beside the name.
    flow.set_administrator("rpcuser");
    let Step::RunPlan(request) = flow.advance() else {
        bail!("a name the page cannot know to be taken is refused locally");
    };
    ensure!(
        flow.plan_finished(sim.plan(&request)) == Step::Stay
            && flow.page() == PageId::Account
            && flow.account_checks().name.message().is_some()
            && !flow.can_continue(),
        "the helper's refusal of the name"
    );
    flow.set_administrator("ilexmaint2");
    ensure!(
        flow.account_checks().name.is_valid(),
        "the refusal outlives the name"
    );
    Ok(())
}

fn gates() -> Result<()> {
    // Legacy BIOS stops the installer on the first page.
    let (flow, _) = flow_on("bios")?;
    ensure!(
        flow.blocking_problem().is_some() && !flow.can_continue(),
        "BIOS does not stop the installer"
    );
    // A helper that does not answer does too, with its own message.
    let mut flow = Flow::new(Layouts::build(RULES, &maps(&["us"]))?);
    flow.set_probe(Simulated::load("helper-down")?.probe());
    ensure!(
        flow.blocking_problem()
            .is_some_and(|m| m.title.contains("helper")),
        "a missing helper is not reported"
    );
    // No usable disk: the plan is not asked for, the page says why.
    let (mut flow, _) = flow_on("no-disk")?;
    ensure!(
        usable_disks(flow.probe().context("probe")?).is_empty(),
        "no-disk has a disk"
    );
    flow.show(PageId::Account);
    flow.set_administrator("ilexmaint");
    flow.set_password("correct horse");
    flow.set_confirmation("correct horse");
    ensure!(
        flow.advance() == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::NoDisk(reasons) if reasons.len() == 3),
        "no usable disk"
    );
    // A registry that does not answer is reported on the ready page.
    let (mut flow, sim) = flow_on("registry-down")?;
    flow.show(PageId::Account);
    flow.set_administrator("ilexmaint");
    flow.set_password("correct horse");
    flow.set_confirmation("correct horse");
    let Step::RunPlan(request) = flow.advance() else {
        bail!("registry-down: no plan request");
    };
    ensure!(
        flow.plan_finished(sim.plan(&request)) == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::Failed(m) if m.title.contains("registry")),
        "a registry failure"
    );
    // A boot argument naming a tag the edition does not offer is reported,
    // and the detection's choice stays usable.
    let (flow, _) = flow_on("bad-override")?;
    ensure!(
        matches!(
            flow.machine_view().map(|v| &v.origin),
            Some(Origin::BadOverride { .. })
        ) && flow.machine_tag() == Some("generic.gnome"),
        "bad override"
    );
    Ok(())
}

fn progress() -> Result<()> {
    let sim = Simulated::load("installing")?;
    let request = ilex_installer_model::plan::PlanRequest::parse(
        r#"{"composition":"generic.gnome","disk":"nvme-eui.0025385c1140a2b3","mode":"whole-disk","administrator":"ilexmaint"}"#,
    )?;
    let plan = sim.plan(&request)?;
    let mut progress = Progress::for_plan(&plan);
    ensure!(
        progress.rows.iter().any(|r| r.stage == Stage::Encrypt) && progress.overall() == 0.0,
        "the encrypted plan's stages"
    );
    let state = sim.state()?;
    progress.apply(&state);
    let running = progress.running().context("no running stage")?;
    ensure!(
        running.stage == Stage::Image
            && progress
                .rows
                .iter()
                .take_while(|r| r.stage != Stage::Image)
                .all(|r| r.status == Status::Done)
            && progress.overall() > 0.2
            && progress.overall() < 0.8,
        "the progress rows: {progress:?}"
    );
    let mut earlier = progress.overall();
    for stage in [
        Stage::Configure,
        Stage::Account,
        Stage::Recovery,
        Stage::Finalise,
    ] {
        progress.apply(&State {
            stage,
            progress: 0.5,
            ..state.clone()
        });
        ensure!(
            progress.overall() >= earlier,
            "the overall bar went back at {stage:?}"
        );
        earlier = progress.overall();
    }
    progress.apply(&State {
        stage: Stage::Done,
        outcome: Some(Outcome::Success),
        ..state.clone()
    });
    ensure!(
        (progress.overall() - 1.0).abs() < f64::EPSILON,
        "a finished installation is not complete"
    );
    progress.apply(&State {
        stage: Stage::Image,
        outcome: Some(Outcome::Failed {
            stage: Stage::Image,
            reason: "no route to host".into(),
        }),
        ..state
    });
    ensure!(
        progress.failed().is_some_and(|r| r.stage == Stage::Image),
        "a failed stage is not shown"
    );
    ensure!(keyboard::DEFAULT == "us", "default keymap");
    let _ = Sources::default();
    Ok(())
}

/// The part that reads the live image's own files: the keyboard catalogue
/// (xkeyboard-config's list and kbd's keymaps) must offer the default layout.
pub fn system(sources: &Sources) -> Result<String> {
    let layouts = Layouts::load(sources)
        .with_context(|| format!("keyboard layouts from {}", keyboard::describe(sources)))?;
    ensure!(
        layouts.get(keyboard::DEFAULT).is_some() && layouts.len() >= 100,
        "the keyboard catalogue has {} layouts and {}",
        layouts.len(),
        if layouts.get(keyboard::DEFAULT).is_some() {
            "the default"
        } else {
            "no default layout"
        }
    );
    Ok(format!("{} keyboard layouts", layouts.len()))
}
