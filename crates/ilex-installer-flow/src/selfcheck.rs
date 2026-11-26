//! The flow's share of `ilex-installer --self-check`: its rules exercised on
//! the simulation's fixtures, and the system's keyboard catalogue read. (The
//! repository has no unit tests; anything worth testing in a shipped binary is
//! observable from inside the VM.)

use crate::account::{self, Check};
use crate::backend::Backend;
use crate::flow::{Flow, KeyboardStatus, PageId, PlanCheck, ProbeState, Step, usable_disks};
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
    generations()?;
    keyboard_switches()?;
    retries()?;
    gates()?;
    marker_formats()?;
    bus_errors()?;
    progress()?;
    Ok(format!(
        "{} scenarios, {} sample layouts, {} languages",
        simulate::names().count(),
        layouts.len(),
        language::LANGUAGES.len()
    ))
}

/// The keyboard catalogue of the self-check's own sample, for the flow and
/// for a renderer on a machine that lacks the system's (the pages are drawn
/// from this one then).
pub fn sample_layouts() -> Result<Layouts> {
    Layouts::build(
        RULES,
        &maps(&["us", "de", "de-nodeadkeys", "fr-oss", "us-intl"]),
    )
}

/// A flow whose first look at the computer has been answered by the scenario.
fn flow_on(name: &str) -> Result<(Flow, Simulated)> {
    let sim = Simulated::load(name)?;
    let mut flow = Flow::new(sample_layouts()?);
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("a new flow does not ask for the probe");
    };
    ensure!(
        flow.probe_finished(generation, sim.probe()) == Step::Stay,
        "the first probe's answer moves the flow"
    );
    Ok((flow, sim))
}

/// A flow on the account page with a valid administrator.
fn at_account(name: &str) -> Result<(Flow, Simulated)> {
    let (mut flow, sim) = flow_on(name)?;
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine)
            && flow.advance() == Step::Goto(PageId::Account),
        "{name}: welcome and machine do not continue"
    );
    ensure!(
        flow.set_administrator("ilexmaint")
            && flow.set_password(Secret::new("correct horse"))
            && flow.set_confirmation(Secret::new("correct horse")),
        "{name}: the account fields are refused"
    );
    Ok((flow, sim))
}

fn run_plan(flow: &mut Flow) -> Result<(u64, Box<ilex_installer_model::plan::PlanRequest>)> {
    match flow.advance() {
        Step::RunPlan {
            generation,
            request,
        } => Ok((generation, request)),
        other => bail!("the account page asked for {other:?}, not the plan"),
    }
}

fn walk() -> Result<()> {
    let (mut flow, sim) = flow_on("generic")?;
    ensure!(
        flow.page() == PageId::Welcome && flow.can_continue(),
        "welcome does not continue"
    );
    // The layout is the session's only once the compositor says so.
    let Step::SwitchKeyboard { generation, layout } = flow.set_keyboard("de-nodeadkeys") else {
        bail!("a new layout does not ask for a switch");
    };
    ensure!(
        layout.id == "de-nodeadkeys"
            && flow.keyboard() == "de-nodeadkeys"
            && flow.keyboard_status() == KeyboardStatus::Switching
            && !flow.can_continue()
            && flow.advance() == Step::Stay,
        "Continue works before the layout is in effect"
    );
    ensure!(
        flow.keyboard_switched(generation, Ok(())) == Step::Stay
            && flow.keyboard_status() == KeyboardStatus::Applied
            && flow.can_continue(),
        "the layout does not take effect"
    );
    ensure!(
        flow.set_language("de_DE.UTF-8")
            && flow.set_keyboard("nope") == Step::Stay
            && flow.keyboard() == "de-nodeadkeys",
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
    flow.set_password(Secret::new("correct horse"));
    ensure!(
        !flow.can_continue(),
        "account continues without a confirmation"
    );
    flow.set_confirmation(Secret::new("correct hose"));
    ensure!(
        flow.account_checks().confirmation.message().is_some(),
        "mismatch not noticed"
    );
    flow.set_confirmation(Secret::new("correct horse"));
    ensure!(
        flow.confirmation().expose() == "correct horse"
            && flow.account_checks().confirmation.is_valid(),
        "the confirmation is not held"
    );
    ensure!(flow.can_continue(), "a valid account does not continue");
    let (generation, request) = run_plan(&mut flow)?;
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
        !flow.can_continue()
            && !flow.can_go_back()
            && flow.continue_label() == crate::messages::CHECKING_LABEL,
        "buttons while the plan is checked"
    );
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::Checked(_)),
        "plan result"
    );
    ensure!(flow.back() == Some(PageId::Account), "back from ready");

    // The helper's own refusal of a name the live image defines stays on the
    // account page, beside the name.
    flow.set_administrator("rpcuser");
    let (generation, request) = run_plan(&mut flow)
        .context("a name the page cannot know to be taken is refused locally")?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Stay
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

/// Answers that arrive late, twice or out of order change nothing, and nothing
/// the plan is made of changes while the helper is asked.
fn generations() -> Result<()> {
    let (mut flow, sim) = at_account("generic")?;
    let (first, request) = run_plan(&mut flow)?;
    ensure!(flow.is_checking(), "the check is not running");
    // The choices are frozen while the helper answers, and a second check
    // cannot be started: a stale answer could never overtake the latest.
    ensure!(
        !flow.set_administrator("someone")
            && !flow.set_password(Secret::new("other password"))
            && !flow.set_confirmation(Secret::new("other password"))
            && !flow.select_machine("ms-7c35.gnome")
            && !flow.set_language("de_DE.UTF-8")
            && flow.set_keyboard("de") == Step::Stay
            && flow.retry() == Step::Stay
            && flow.advance() == Step::Stay
            && flow.start_probe() == Step::Stay
            && flow.back().is_none()
            && flow.is_checking()
            && flow.administrator() == "ilexmaint",
        "the choices change while the plan is checked"
    );
    // Another request's answer is not this one's.
    ensure!(
        flow.plan_finished(first + 1, sim.plan(&request)) == Step::Stay
            && flow.is_checking()
            && flow.plan_finished(first.wrapping_sub(1), sim.plan(&request)) == Step::Stay
            && flow.is_checking(),
        "an answer to another request ends the check"
    );
    ensure!(
        flow.plan_is_current(first)
            && flow.plan_finished(first, sim.plan(&request)) == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::Checked(_)),
        "the answer to the request does not end the check"
    );
    // The same answer again, or a failure of the same request, is late.
    ensure!(
        flow.plan_finished(first, Err(ClientError::HelperUnavailable("late".into()))) == Step::Stay
            && matches!(flow.plan_check(), PlanCheck::Checked(_)),
        "a second answer replaced the first"
    );
    // A check later, the first one's answer is stale.
    ensure!(flow.back() == Some(PageId::Account), "back from ready");
    let (second, request) = run_plan(&mut flow)?;
    ensure!(second != first, "two checks share a number");
    ensure!(
        flow.plan_finished(first, Err(ClientError::Other("an old answer".into()))) == Step::Stay
            && flow.is_checking()
            && flow.page() == PageId::Account,
        "an old answer ended the new check"
    );
    ensure!(
        flow.plan_finished(second, sim.plan(&request)) == Step::Goto(PageId::Ready),
        "the new check's answer"
    );
    // Setting the text that is already there is no change: a checked plan
    // stays checked.
    ensure!(
        flow.set_password(Secret::new("correct horse"))
            && matches!(flow.plan_check(), PlanCheck::Checked(_)),
        "the same password reset the check"
    );
    ensure!(
        !format!("{:?}", flow.password()).contains("horse"),
        "the password prints"
    );
    Ok(())
}

/// The session's layout follows the person's last choice whatever order the
/// compositor's answers come in, and a failure is shown, not swallowed.
fn keyboard_switches() -> Result<()> {
    let (mut flow, _) = flow_on("generic")?;
    // The session starts on the default layout: asking for it is no work.
    ensure!(
        flow.set_keyboard("us") == Step::Stay && flow.keyboard_status() == KeyboardStatus::Applied,
        "the layout in effect is switched to again"
    );
    let Step::SwitchKeyboard {
        generation: first, ..
    } = flow.set_keyboard("de")
    else {
        bail!("no switch to de");
    };
    // A second choice while the first is under way waits for it.
    ensure!(
        flow.set_keyboard("de-nodeadkeys") == Step::Stay && !flow.can_continue(),
        "two switches are outstanding"
    );
    ensure!(
        flow.keyboard_switched(first + 7, Ok(())) == Step::Stay
            && flow.keyboard_status() == KeyboardStatus::Switching,
        "a stale answer was taken"
    );
    // The first answer comes: the latest choice is asked for next.
    let Step::SwitchKeyboard {
        generation: second,
        layout,
    } = flow.keyboard_switched(first, Ok(()))
    else {
        bail!("the latest choice is not asked for after the first answer");
    };
    ensure!(
        layout.id == "de-nodeadkeys" && !flow.can_continue(),
        "the second switch is for {layout:?}"
    );
    ensure!(
        flow.keyboard_switched(first, Ok(())) == Step::Stay,
        "the first answer counted twice"
    );
    // The latest one fails: shown, Continue stays off, a retry asks again.
    ensure!(
        flow.keyboard_switched(second, Err("no compositor".into())) == Step::Stay
            && matches!(flow.keyboard_status(), KeyboardStatus::Failed(m) if m.detail.contains("no compositor"))
            && !flow.can_continue(),
        "a failed switch is not reported"
    );
    let Step::SwitchKeyboard {
        generation: third, ..
    } = flow.retry_keyboard()
    else {
        bail!("no retry of the layout");
    };
    ensure!(
        flow.keyboard_switched(third, Ok(())) == Step::Stay
            && flow.keyboard_status() == KeyboardStatus::Applied
            && flow.can_continue(),
        "the retried layout does not take effect"
    );
    // A failure of a layout the person has replaced is not shown: the newer
    // choice is asked for next, even if it is the layout the session had.
    let Step::SwitchKeyboard {
        generation: fourth, ..
    } = flow.set_keyboard("de")
    else {
        bail!("no switch to de");
    };
    ensure!(
        flow.set_keyboard("us") == Step::Stay,
        "waits for the switch"
    );
    let Step::SwitchKeyboard { layout, .. } = flow.keyboard_switched(fourth, Err("gone".into()))
    else {
        bail!("the newer choice is not asked for after a failure");
    };
    ensure!(
        layout.id == "us" && flow.keyboard_status() == KeyboardStatus::Switching,
        "after a replaced layout's failure: {layout:?}"
    );
    Ok(())
}

/// Try again looks at the computer again, then checks; the person's
/// configuration stays chosen while the new look still offers it; a late
/// answer of the old look is ignored.
fn retries() -> Result<()> {
    let (mut flow, sim) = at_account("generic")?;
    // Back on the machine page the person picks the specific configuration.
    ensure!(
        flow.back() == Some(PageId::Machine),
        "back to the machine page"
    );
    ensure!(flow.select_machine("ms-7c35.gnome"), "machine choice");
    ensure!(
        flow.advance() == Step::Goto(PageId::Account),
        "machine -> account"
    );
    let (generation, request) = run_plan(&mut flow)?;
    // The disk is gone by the time the helper looks: a refusal that only a
    // new look can mend.
    let refusal = ClientError::Refused(PlanError::new(
        ErrorCode::UnknownDisk,
        Some("disk"),
        "that disk is gone",
    ));
    ensure!(
        flow.plan_finished(generation, Err(refusal)) == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::Failed(_)),
        "a refused plan is reported on the closing page"
    );
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("Try again does not look at the computer again");
    };
    ensure!(
        flow.is_checking() && !flow.can_go_back() && !flow.can_retry_probe(),
        "the look is not part of the check"
    );
    ensure!(
        flow.probe_finished(look + 3, sim.probe()) == Step::Stay
            && matches!(flow.probe_state(), ProbeState::Pending),
        "an answer to another look was taken"
    );
    let Step::RunPlan {
        generation: again,
        request: second,
    } = flow.probe_finished(look, sim.probe())
    else {
        bail!("the look is not followed by the check");
    };
    ensure!(
        again != generation && *second == *request && flow.machine_tag() == Some("ms-7c35.gnome"),
        "the person's configuration is not kept: {:?}",
        flow.machine_tag()
    );
    // The new look no longer offers it: the preselection is back.
    ensure!(
        flow.plan_finished(again, Err(ClientError::HelperUnavailable("gone".into()))) == Step::Stay
            && matches!(flow.plan_check(), PlanCheck::Failed(_)),
        "a failed check on the closing page"
    );
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("no second retry");
    };
    let other = Simulated::load("no-composition")?;
    ensure!(
        matches!(
            flow.probe_finished(look, other.probe()),
            Step::RunPlan { .. }
        ) && flow.machine_tag() == Some("generic.gnome"),
        "a configuration that is no longer offered stays chosen: {:?}",
        flow.machine_tag()
    );
    // A look that fails ends the retry on the closing page, and the page can
    // try again.
    let (mut flow, sim) = at_account("generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Ready),
        "plan result"
    );
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("no retry from a checked plan");
    };
    ensure!(
        flow.probe_finished(look, Simulated::load("helper-down")?.probe()) == Step::Stay
            && matches!(flow.plan_check(), PlanCheck::Failed(m) if m.title.contains("helper"))
            && flow.page() == PageId::Ready
            && flow.can_retry_probe(),
        "a look that fails"
    );
    // A refusal of the name found when the page was the closing one goes back
    // to the account page, and the step says so.
    let (mut flow, sim) = at_account("generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Ready),
        "plan result"
    );
    flow.set_administrator("rpcuser");
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("no retry");
    };
    let Step::RunPlan {
        generation,
        request,
    } = flow.probe_finished(look, sim.probe())
    else {
        bail!("no check after the look");
    };
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Account)
            && flow.page() == PageId::Account,
        "the refusal of a name found on the closing page does not go back"
    );
    // After a check that found no disk, Continue also looks again first.
    let (mut flow, _) = at_account("no-disk")?;
    ensure!(
        flow.advance() == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::NoDisk(reasons) if reasons.len() == 3)
            && flow
                .check_problem()
                .is_some_and(|m| m.detail.contains("Connect a disk")),
        "no usable disk"
    );
    ensure!(
        matches!(flow.back(), Some(PageId::Account))
            && matches!(flow.advance(), Step::RunProbe { .. }),
        "Continue after a check that found no disk does not look again"
    );
    Ok(())
}

fn gates() -> Result<()> {
    // Legacy BIOS stops the installer on the first page.
    let (flow, _) = flow_on("bios")?;
    ensure!(
        flow.blocking_problem().is_some() && !flow.can_continue() && !flow.can_retry_probe(),
        "BIOS does not stop the installer"
    );
    // The answer can come after the person has left the first page: the
    // problem is the flow's, whatever page is showing.
    let sim = Simulated::load("bios")?;
    let mut flow = Flow::new(sample_layouts()?);
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no probe");
    };
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine) && flow.blocking_problem().is_none(),
        "the first page waits for the probe"
    );
    flow.probe_finished(generation, sim.probe());
    ensure!(
        flow.page() == PageId::Machine
            && flow
                .blocking_problem()
                .is_some_and(|m| m.title.contains("BIOS"))
            && !flow.can_continue()
            && flow.advance() == Step::Stay,
        "a BIOS answer after the first page is a silent dead end"
    );
    // A helper that does not answer does too, with its own message, and the
    // person can ask again.
    let mut flow = Flow::new(sample_layouts()?);
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no probe");
    };
    flow.probe_finished(generation, Simulated::load("helper-down")?.probe());
    ensure!(
        flow.blocking_problem()
            .is_some_and(|m| m.title.contains("helper"))
            && flow.can_retry_probe()
            && matches!(flow.start_probe(), Step::RunProbe { .. }),
        "a missing helper is not reported"
    );
    // No usable disk: the plan is not asked for, the page says why.
    let (mut flow, _) = at_account("no-disk")?;
    ensure!(
        usable_disks(flow.probe().context("probe")?).is_empty(),
        "no-disk has a disk"
    );
    ensure!(
        flow.advance() == Step::Goto(PageId::Ready)
            && matches!(flow.plan_check(), PlanCheck::NoDisk(reasons) if reasons.len() == 3),
        "no usable disk"
    );
    // A registry that does not answer is reported on the ready page.
    let (mut flow, sim) = at_account("registry-down")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Ready)
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

/// The marker formats the trials read, and the one rule about what they may
/// carry: nothing typed in a field, but for the layout test field in a test.
fn marker_formats() -> Result<()> {
    let typed = Secret::new("hunter2 try");
    ensure!(
        crate::markers::tried("us", &typed, false).is_none(),
        "the layout test field's text is printed without the test credential"
    );
    ensure!(
        crate::markers::tried("de-nodeadkeys", &Secret::new("z y"), true).as_deref()
            == Some("ILEX-INSTALLER event=try layout=de-nodeadkeys text=z_y"),
        "the echoed layout test"
    );
    let (mut flow, sim) = flow_on("generic")?;
    ensure!(
        flow.page_marker("gnome")
            == "ILEX-INSTALLER page=welcome edition=gnome language=en_US.UTF-8 keyboard=us",
        "the welcome marker: {}",
        flow.page_marker("gnome")
    );
    flow.advance();
    ensure!(
        flow.page_marker("gnome").starts_with(
            "ILEX-INSTALLER page=machine edition=gnome selected=generic.gnome origin=not-recognised detected=generic.gnome offered=2 choices=generic.gnome,ms-7c35.gnome"
        ),
        "the machine marker: {}",
        flow.page_marker("gnome")
    );
    let probe = crate::markers::probe(&sim.probe());
    ensure!(
        probe.starts_with("ILEX-INSTALLER event=probe result=ok disks="),
        "{probe}"
    );
    let notice = crate::markers::notice(&flow.machine_view().context("no machine view")?.notice());
    ensure!(
        notice.starts_with(
            "ILEX-INSTALLER event=notice severity=info text=No_configuration_is_specific"
        ),
        "{notice}"
    );
    // Nothing a person typed is in any marker of the walk.
    for line in [
        flow.page_marker("gnome"),
        crate::markers::plan(&Err(ClientError::Other("x".into()))),
        crate::markers::layout("us", true),
    ] {
        ensure!(!line.contains("hunter2"), "{line}");
    }
    Ok(())
}

/// What each kind of D-Bus error becomes: errors that say the helper or the
/// bus is gone are `HelperUnavailable` (the backend then connects afresh),
/// the helper's refusals keep their document.
#[cfg(target_os = "linux")]
fn bus_errors() -> Result<()> {
    use crate::backend::classify_bus_error;
    for name in [
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "org.freedesktop.DBus.Error.NameHasNoOwner",
        "org.freedesktop.DBus.Error.NoReply",
        "org.freedesktop.DBus.Error.Disconnected",
        "org.freedesktop.DBus.Error.TimedOut",
        "org.freedesktop.DBus.Error.Timeout",
        "org.freedesktop.DBus.Error.NoServer",
        "org.freedesktop.DBus.Error.Spawn.ExecFailed",
        "org.freedesktop.DBus.Error.Spawn.ServiceNotFound",
        "org.freedesktop.systemd1.NoSuchUnit",
        "org.freedesktop.systemd1.UnitMasked",
    ] {
        ensure!(
            matches!(
                classify_bus_error(name, "why".into()),
                ClientError::HelperUnavailable(_)
            ),
            "{name} is not 'the helper is not available'"
        );
    }
    ensure!(
        matches!(
            classify_bus_error("org.freedesktop.DBus.Error.AccessDenied", "no".into()),
            ClientError::AccessDenied(_)
        ) && matches!(
            classify_bus_error("org.freedesktop.DBus.Error.UnknownMethod", "no".into()),
            ClientError::Other(_)
        ),
        "access denied and an unknown method"
    );
    let refusal = PlanError::new(ErrorCode::UnknownDisk, Some("disk"), "gone");
    ensure!(
        classify_bus_error(
            "org.ilex.Installer1.Error.UnknownDisk",
            serde_json::to_string(&refusal)?
        ) == ClientError::Refused(refusal)
            && matches!(
                classify_bus_error("org.ilex.Installer1.Error.Internal", "not json".into()),
                ClientError::Other(_)
            ),
        "the helper's refusals"
    );
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn bus_errors() -> Result<()> {
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
