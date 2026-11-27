//! The flow's share of `ilex-installer --self-check`: its rules exercised on
//! the simulation's fixtures, and the system's keyboard catalogue read. (The
//! repository has no unit tests; anything worth testing in a shipped binary is
//! observable from inside the VM.)

use crate::account::{self, Check};
use crate::backend::Backend;
use crate::flow::{
    Flow, KeyboardStatus, LogSave, MarkerConfig, PageId, Phase, PlanCheck, ProbeState, ProblemKind,
    Step, usable_disks,
};
use crate::keyboard::{self, Layouts, Sources};
use crate::language;
use crate::machine::{MachineView, Origin};
use crate::messages::{ClientError, plan_error};
use crate::progress::{Progress, Status};
use crate::secret::Secret;
use crate::simulate::{self, Simulated};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::finish::FinishAction;
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
    probe_answers()?;
    keyboard_switches()?;
    retries()?;
    gates()?;
    marker_formats()?;
    bus_errors()?;
    progress()?;
    disk_page()?;
    installation()?;
    install_markers()?;
    failures()?;
    attaching()?;
    log_dialog()?;
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
            && flow.advance() == Step::Goto(PageId::Disk)
            && flow.advance() == Step::Goto(PageId::Account),
        "{name}: welcome, machine and disk do not continue"
    );
    fill_account(&mut flow, name)?;
    Ok((flow, sim))
}

/// A flow on the account page of a computer whose disk page cannot continue
/// (nothing to install to): the page is shown as a later look at the computer
/// can leave it (a disk that was unplugged since).
fn at_account_without_disk(name: &str) -> Result<(Flow, Simulated)> {
    let (mut flow, sim) = flow_on(name)?;
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine)
            && flow.advance() == Step::Goto(PageId::Disk)
            && !flow.can_continue()
            && flow.advance() == Step::Stay,
        "{name}: the disk page continues without a disk"
    );
    flow.show(PageId::Account);
    fill_account(&mut flow, name)?;
    Ok((flow, sim))
}

fn fill_account(flow: &mut Flow, name: &str) -> Result<()> {
    ensure!(
        flow.set_administrator("ilexmaint")
            && flow.set_password(Secret::new("correct horse"))
            && flow.set_confirmation(Secret::new("correct horse")),
        "{name}: the account fields are refused"
    );
    Ok(())
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
        flow.advance() == Step::Goto(PageId::Disk)
            && flow.disk() == Some("nvme-eui.0025385c1140a2b3")
            && flow.advance() == Step::Goto(PageId::Account),
        "machine -> disk -> account"
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
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review)
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
            && flow.plan_finished(first, sim.plan(&request)) == Step::Goto(PageId::Review)
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
        flow.plan_finished(second, sim.plan(&request)) == Step::Goto(PageId::Review),
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

/// A probe's answer is taken only for the request in flight: not before any
/// was asked, not twice, not while the plan is checked; only one probe is asked
/// at a time; and the account page does not continue on a look that is still
/// pending.
fn probe_answers() -> Result<()> {
    let sim = Simulated::load("generic")?;
    let mut flow = Flow::new(sample_layouts()?);
    // Before the first request nothing is waiting for an answer.
    ensure!(
        flow.probe_finished(0, sim.probe()) == Step::Stay
            && matches!(flow.probe_state(), ProbeState::Pending)
            && flow.machine_view().is_none()
            && flow.machine_tag().is_none()
            && !flow.probe_is_current(0),
        "an answer to a probe nobody asked for was taken"
    );
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("a new flow does not ask for the probe");
    };
    ensure!(
        flow.start_probe() == Step::Stay && flow.probe_is_current(generation),
        "a second probe starts while the first is outstanding"
    );
    ensure!(
        flow.probe_finished(generation + 1, sim.probe()) == Step::Stay
            && matches!(flow.probe_state(), ProbeState::Pending)
            && flow.probe_is_current(generation),
        "the answer to another probe was taken"
    );
    ensure!(
        flow.probe_finished(generation, sim.probe()) == Step::Stay
            && flow.probe().is_some()
            && !flow.probe_is_current(generation),
        "the answer to the probe in flight was not taken"
    );
    // The same answer again, even a failure, changes nothing.
    ensure!(
        flow.probe_finished(
            generation,
            Err(ClientError::HelperUnavailable("late".into()))
        ) == Step::Stay
            && flow.probe().is_some()
            && flow.blocking_problem().is_none()
            && flow.probe_finished(generation, sim.probe()) == Step::Stay,
        "a repeated answer changed the probe"
    );

    // While the plan is checked, no answer is taken: the person's machine, the
    // view and the probe stay what the check was asked for. (Probes number
    // from 1; every number is tried.)
    let (mut flow, sim) = at_account("ms-7c35")?;
    let (generation, request) = run_plan(&mut flow)?;
    let other = Simulated::load("no-composition")?;
    for number in 0..=3 {
        ensure!(
            flow.probe_finished(number, other.probe()) == Step::Stay
                && flow.is_checking()
                && flow.machine_tag() == Some("ms-7c35.gnome")
                && flow.machine_view().map(|v| v.origin.word()) == Some("detected"),
            "an unsolicited probe answer ({number}) changed the choices while the plan is checked"
        );
    }
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review),
        "the check does not end after the unsolicited answers"
    );

    // A retry's own look is part of the check: no competing probe starts.
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("Try again does not look at the computer again");
    };
    ensure!(
        flow.start_probe() == Step::Stay && flow.probe_is_current(look),
        "a probe competes with the retry's look"
    );
    ensure!(
        matches!(flow.probe_finished(look, sim.probe()), Step::RunPlan { .. }),
        "the retry's look was disturbed"
    );

    // After Back and a new look, Continue waits for the answer instead of
    // asking for a plan the flow has no probe for.
    let (mut flow, sim) = at_account("generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review),
        "plan result"
    );
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("no retry from a checked plan");
    };
    flow.probe_finished(look, Simulated::load("helper-down")?.probe());
    ensure!(
        flow.back() == Some(PageId::Account) && flow.can_retry_probe(),
        "back to the account page after a look that failed"
    );
    let Step::RunProbe { generation: again } = flow.start_probe() else {
        bail!("the account page's banner cannot ask again");
    };
    ensure!(
        flow.account_checks().all_valid()
            && !flow.can_continue()
            && flow.advance() == Step::Stay
            && flow.page() == PageId::Account,
        "Continue runs while the look is pending"
    );
    ensure!(
        flow.probe_finished(again, sim.probe()) == Step::Stay
            && flow.can_continue()
            && matches!(flow.advance(), Step::RunPlan { .. }),
        "Continue does not follow the new look"
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
        flow.back() == Some(PageId::Disk) && flow.back() == Some(PageId::Machine),
        "back to the machine page"
    );
    ensure!(flow.select_machine("ms-7c35.gnome"), "machine choice");
    ensure!(
        flow.advance() == Step::Goto(PageId::Disk) && flow.advance() == Step::Goto(PageId::Account),
        "machine -> disk -> account"
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
        flow.plan_finished(generation, Err(refusal)) == Step::Goto(PageId::Review)
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
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review),
        "plan result"
    );
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("no retry from a checked plan");
    };
    ensure!(
        flow.probe_finished(look, Simulated::load("helper-down")?.probe()) == Step::Stay
            && matches!(flow.plan_check(), PlanCheck::Failed(m) if m.title.contains("helper"))
            && flow.page() == PageId::Review
            && flow.can_retry_probe(),
        "a look that fails"
    );
    // A refusal of the name found when the page was the closing one goes back
    // to the account page, and the step says so.
    let (mut flow, sim) = at_account("generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review),
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
    let (mut flow, _) = at_account_without_disk("no-disk")?;
    ensure!(
        flow.advance() == Step::Goto(PageId::Review)
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
    let (mut flow, _) = at_account_without_disk("no-disk")?;
    ensure!(
        usable_disks(flow.probe().context("probe")?).is_empty(),
        "no-disk has a disk"
    );
    ensure!(
        flow.advance() == Step::Goto(PageId::Review)
            && matches!(flow.plan_check(), PlanCheck::NoDisk(reasons) if reasons.len() == 3),
        "no usable disk"
    );
    // Try again is offered on the closing page when asking again can mend the
    // result, never while the helper is asked, and not when only a restart
    // helps: a look that finds a BIOS boot ends there.
    let (mut flow, sim) = at_account("generic")?;
    ensure!(!flow.can_retry(), "Try again on the account page");
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(!flow.can_retry(), "Try again while the plan is checked");
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review)
            && flow.can_retry(),
        "no Try again after a checked plan"
    );
    let Step::RunProbe { generation: look } = flow.retry() else {
        bail!("no retry");
    };
    ensure!(!flow.can_retry(), "Try again while the look is pending");
    ensure!(
        flow.probe_finished(look, Simulated::load("bios")?.probe()) == Step::Stay
            && flow.page() == PageId::Review
            && flow
                .check_problem()
                .is_some_and(|m| m.title.contains("BIOS"))
            && !flow.can_retry()
            && flow.retry() == Step::Stay
            && !flow.is_checking(),
        "a BIOS boot found by the look is offered Try again"
    );
    // A registry that does not answer is reported on the ready page.
    let (mut flow, sim) = at_account("registry-down")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review)
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

/// The marker formats the trials read, which line the flow queues when, and the
/// one rule about what they may carry: nothing typed in a field, but for the
/// layout test field in a test.
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

    // Without markers the flow queues nothing and the field is not read.
    let mut quiet = Flow::new(sample_layouts()?);
    quiet.window_shown("renderer=none");
    quiet.try_text(&typed);
    ensure!(
        !quiet.echo_try() && quiet.drain_markers().is_empty(),
        "a flow without markers queued some"
    );
    // The test field: closed on a normal boot, open for the credential or a
    // simulated session.
    for (simulated, credential, open) in [
        (false, false, false),
        (false, true, true),
        (true, false, true),
    ] {
        let mut flow = Flow::new(sample_layouts()?).with_markers(config(simulated, credential));
        flow.window_shown("renderer=none");
        flow.drain_markers();
        flow.try_text(&Secret::new("z y"));
        let lines = flow.drain_markers();
        ensure!(
            flow.echo_try() == open
                && lines
                    == if open {
                        vec!["ILEX-INSTALLER event=try layout=us text=z_y".to_owned()]
                    } else {
                        Vec::new()
                    },
            "the test field with simulated={simulated} credential={credential}: {lines:?}"
        );
    }

    // The walk, line by line.
    let sim = Simulated::load("generic")?;
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(false, false));
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no probe");
    };
    ensure!(
        flow.drain_markers().is_empty(),
        "a probe request is printed"
    );
    // An answer nobody waits for says so, and changes nothing.
    flow.probe_finished(generation + 1, sim.probe());
    ensure!(
        flow.drain_markers() == ["ILEX-INSTALLER event=probe result=stale"],
        "a stale probe answer"
    );
    flow.probe_finished(generation, sim.probe());
    let lines = flow.drain_markers();
    ensure!(
        lines.len() == 1 && lines[0].starts_with("ILEX-INSTALLER event=probe result=ok disks="),
        "the probe answer: {lines:?}"
    );
    // The first page is printed when the window is on screen, with the
    // toolkit's words and the uptime; never again.
    flow.window_shown("renderer=gsk scale=2");
    let lines = flow.drain_markers();
    ensure!(
        lines.len() == 1
            && lines[0].starts_with(
                "ILEX-INSTALLER page=welcome edition=gnome language=en_US.UTF-8 keyboard=us renderer=gsk scale=2 uptime="
            ),
        "the welcome marker: {lines:?}"
    );
    flow.window_shown("renderer=gsk scale=2");
    ensure!(flow.drain_markers().is_empty(), "the first page twice");
    // The language is printed when it changes, once.
    ensure!(
        flow.set_language("de_DE.UTF-8")
            && flow.set_language("de_DE.UTF-8")
            && !flow.set_language("xx_XX.UTF-8"),
        "languages"
    );
    ensure!(
        flow.drain_markers() == ["ILEX-INSTALLER event=language language=de_DE.UTF-8"],
        "the language event"
    );
    // The layout is printed with the compositor's answer, for the switch it
    // answers; another answer says it is stale.
    let Step::SwitchKeyboard { generation, .. } = flow.set_keyboard("de-nodeadkeys") else {
        bail!("no switch");
    };
    ensure!(
        flow.drain_markers().is_empty(),
        "the layout request is printed"
    );
    flow.keyboard_switched(generation + 5, Ok(()));
    flow.keyboard_switched(generation, Ok(()));
    flow.keyboard_switched(generation, Ok(()));
    ensure!(
        flow.drain_markers()
            == [
                "ILEX-INSTALLER event=layout result=stale",
                "ILEX-INSTALLER event=layout layout=de-nodeadkeys applied=true",
                "ILEX-INSTALLER event=layout result=stale",
            ],
        "the layout answers"
    );
    // The machine page: its marker, the composition line and the notice.
    flow.try_text(&typed);
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine),
        "welcome does not continue"
    );
    let lines = flow.drain_markers();
    ensure!(
        lines.len() == 3
            && lines[0].starts_with(
                "ILEX-INSTALLER page=machine edition=gnome selected=generic.gnome origin=not-recognised detected=generic.gnome offered=2 choices=generic.gnome,ms-7c35.gnome"
            )
            && lines[1]
                == "ILEX-COMPOSITION generic.gnome (no machine configuration matches this computer) installs ghcr.io/reidond/ilex:generic.gnome"
            && lines[2].starts_with(
                "ILEX-INSTALLER event=notice severity=info text=No_configuration_is_specific"
            ),
        "the machine page: {lines:?}"
    );
    // Choosing prints the composition line of an accepted, changed choice.
    ensure!(
        flow.select_machine("ms-7c35.gnome")
            && flow.select_machine("ms-7c35.gnome")
            && !flow.select_machine("ms-7c35.kde"),
        "machine choice"
    );
    ensure!(
        flow.drain_markers()
            == [
                "ILEX-COMPOSITION ms-7c35.gnome (chosen in the installer) installs ghcr.io/reidond/ilex:ms-7c35.gnome"
            ],
        "the choice's line"
    );
    // A new look answers on the machine page: the page is announced again,
    // with the person's choice kept.
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no new look");
    };
    flow.probe_finished(generation, sim.probe());
    let lines = flow.drain_markers();
    ensure!(
        lines.len() == 4
            && lines[0].starts_with("ILEX-INSTALLER event=probe result=ok")
            && lines[1]
                .starts_with("ILEX-INSTALLER page=machine edition=gnome selected=ms-7c35.gnome")
            && lines[2].starts_with("ILEX-COMPOSITION ms-7c35.gnome (chosen in the installer)")
            && lines[3].starts_with("ILEX-INSTALLER event=notice"),
        "the machine page after a new look: {lines:?}"
    );
    // The administrator's name is one word in the marker whatever was typed.
    ensure!(
        flow.advance() == Step::Goto(PageId::Disk)
            && flow.advance() == Step::Goto(PageId::Account)
            && flow.set_administrator("two words\nhere")
            && flow.back() == Some(PageId::Disk)
            && flow.advance() == Step::Goto(PageId::Account),
        "account page"
    );
    let lines = flow.drain_markers();
    ensure!(
        lines
            .iter()
            .any(|l| l.ends_with(" administrator=two_words_here"))
            && !lines.iter().any(|l| l.contains("two words")),
        "the account marker: {lines:?}"
    );
    flow.set_administrator("ilexmaint");
    flow.set_password(Secret::new("hunter2 correct"));
    flow.set_confirmation(Secret::new("hunter2 correct"));
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.drain_markers().is_empty(),
        "the check's request is printed"
    );
    flow.plan_finished(generation + 3, sim.plan(&request));
    ensure!(
        flow.drain_markers() == ["ILEX-INSTALLER event=plan result=stale"],
        "a stale plan answer"
    );
    flow.plan_finished(generation, sim.plan(&request));
    let lines = flow.drain_markers();
    ensure!(
        lines.len() == 2
            && lines[0] == "ILEX-INSTALLER event=plan result=ok"
            && lines[1] == "ILEX-INSTALLER page=review edition=gnome check=ok",
        "the checked plan: {lines:?}"
    );
    // A look that no longer offers the person's configuration resets it, and
    // says so off the machine page too (the machine page announces itself).
    let Step::RunProbe { generation } = flow.retry() else {
        bail!("no retry");
    };
    ensure!(
        flow.drain_markers().is_empty(),
        "the retry's look is printed"
    );
    flow.probe_finished(generation, Simulated::load("no-composition")?.probe());
    let lines = flow.drain_markers();
    ensure!(
        lines.len() == 2
            && lines[0].starts_with("ILEX-INSTALLER event=probe result=ok")
            && lines[1].starts_with("ILEX-COMPOSITION generic.gnome ("),
        "the retry's look: {lines:?}"
    );
    ensure!(
        lines.iter().all(|l| !l.contains("hunter2")),
        "a password is in a marker"
    );
    Ok(())
}

fn config(simulated: bool, echo_credential: bool) -> MarkerConfig {
    MarkerConfig {
        edition: "gnome".to_owned(),
        simulated,
        echo_credential,
    }
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

// --- the disk page and the installation ----------------------------------------

/// Carry out one of the flow's steps against the simulation, as an interface's
/// worker would, and give back what the flow asks for next.
fn carry_out(flow: &mut Flow, sim: &Simulated, step: Step) -> Result<Step> {
    Ok(match step {
        Step::RunProbe { generation } => flow.probe_finished(generation, sim.probe()),
        Step::RunPlan {
            generation,
            request,
        } => flow.plan_finished(generation, sim.plan(&request)),
        Step::RunApply {
            generation,
            plan,
            erase_disk,
            password,
        } => flow.apply_finished(
            generation,
            sim.start_installation(&plan, &erase_disk, &password),
        ),
        Step::RunState { generation, .. } => flow.state_finished(generation, sim.state()),
        Step::RunLogProbe { generation } => flow.log_probe_finished(generation, sim.probe()),
        Step::RunFinish { generation, action } => {
            flow.finish_finished(generation, sim.finish(&action))
        }
        other => other,
    })
}

/// Carry steps out until the flow asks for nothing more of the simulation.
fn settle(flow: &mut Flow, sim: &Simulated, mut step: Step) -> Result<Step> {
    for _ in 0..200 {
        if !matches!(
            step,
            Step::RunProbe { .. }
                | Step::RunPlan { .. }
                | Step::RunApply { .. }
                | Step::RunState { .. }
                | Step::RunLogProbe { .. }
                | Step::RunFinish { .. }
        ) {
            return Ok(step);
        }
        step = carry_out(flow, sim, step)?;
    }
    bail!("the flow never stops asking")
}

/// A flow whose choices have been checked: on the review page.
fn at_review(name: &str) -> Result<(Flow, Simulated)> {
    let (mut flow, sim) = at_account(name)?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review)
            && flow.can_install(),
        "{name}: the review page cannot install"
    );
    Ok((flow, sim))
}

/// The review page's button, the dialog and the helper's `Apply`: the flow on
/// its installing page.
fn start_install(flow: &mut Flow, sim: &Simulated) -> Result<Step> {
    let Step::Confirm(_) = flow.install() else {
        bail!("the button does not ask for the dialog");
    };
    let step = flow.confirm_install();
    ensure!(
        matches!(step, Step::RunApply { .. }),
        "the confirmation does not apply: {step:?}"
    );
    carry_out(flow, sim, step)
}

fn disk_page() -> Result<()> {
    let (mut flow, sim) = flow_on("generic")?;
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine) && flow.advance() == Step::Goto(PageId::Disk),
        "welcome and machine do not continue"
    );
    let view = flow.disk_view().context("no disk view")?.clone();
    let ids: Vec<&str> = view.choices.iter().map(|c| c.id.as_str()).collect();
    ensure!(
        ids == [
            "nvme-eui.0025385c1140a2b3",
            "ata-WDC_WDS500G2B0A_214512800345",
            "usb-SanDisk_Ultra_Fit_4C530001230815113423-0:0",
            "usb-Generic_Flash_Disk_AA00000000000489-0:0"
        ] && view.offered().count() == 2,
        "the disks are listed {ids:?}: offered first, the others after"
    );
    let first = &view.choices[0];
    ensure!(
        first.title == "Samsung SSD 990 PRO 2TB"
            && first.subtitle.starts_with("1.8 TiB")
            && first.subtitle.contains("NVMe")
            && first.subtitle.contains("S73VNJ0W412345X")
            && first.contents.iter().any(|l| l == "2 partitions")
            && first.contents.iter().any(|l| l.contains("Windows"))
            && first.unavailable.is_none()
            && first.warning.is_none(),
        "the first disk: {first:?}"
    );
    ensure!(
        view.choices[2]
            .unavailable
            .as_deref()
            .is_some_and(|w| w.contains("medium the installer started from"))
            && view.choices[3]
                .unavailable
                .as_deref()
                .is_some_and(|w| w.contains("too small")),
        "why the others are not offered: {:?}",
        view.choices
    );
    ensure!(
        view.modes.len() == 2
            && view.modes[0].available
            && !view.modes[1].available
            && view.modes[0].title == "Erase the whole disk",
        "the ways of installing: {:?}",
        view.modes
    );
    // The first disk that can be installed to is chosen until the person says.
    ensure!(
        flow.disk() == Some("nvme-eui.0025385c1140a2b3")
            && flow.can_continue()
            && flow
                .marker_fields()
                .starts_with("disk=nvme-eui.0025385c1140a2b3 offered=2 unavailable=2"),
        "the default disk: {:?}",
        flow.marker_fields()
    );
    ensure!(
        !flow.set_disk("usb-SanDisk_Ultra_Fit_4C530001230815113423-0:0")
            && !flow.set_disk("usb-Generic_Flash_Disk_AA00000000000489-0:0")
            && !flow.set_disk("nope")
            && flow.disk() == Some("nvme-eui.0025385c1140a2b3"),
        "a disk that is not offered was chosen"
    );
    ensure!(
        flow.set_disk("ata-WDC_WDS500G2B0A_214512800345")
            && flow.disk() == Some("ata-WDC_WDS500G2B0A_214512800345")
            && flow
                .disk_choice()
                .is_some_and(|c| c.title == "WDC WDS500G2B0A"),
        "the person's choice"
    );
    // The choice survives another look while the disk is still offered...
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no look");
    };
    flow.probe_finished(generation, sim.probe());
    ensure!(
        flow.disk() == Some("ata-WDC_WDS500G2B0A_214512800345"),
        "a new look changed the person's disk"
    );
    // ...and goes into the request, whole-disk and unencrypted.
    ensure!(
        flow.advance() == Step::Goto(PageId::Account),
        "disk -> account"
    );
    fill_account(&mut flow, "generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        request.disk == "ata-WDC_WDS500G2B0A_214512800345" && !request.encrypt,
        "the request: {request:?}"
    );
    let plan = sim.plan(&request)?;
    ensure!(
        plan.disk.id == request.disk && plan.partitions.iter().all(|p| !p.encrypted),
        "the plan is not for the chosen disk, unencrypted"
    );
    flow.plan_finished(generation, Ok(plan));
    // A disk that is gone after the check invalidates it, and the default
    // moves on to one that is offered.
    ensure!(
        flow.back() == Some(PageId::Account) && flow.back() == Some(PageId::Disk),
        "back to the disk page"
    );
    ensure!(
        matches!(flow.plan_check(), PlanCheck::Checked(_)),
        "going back dropped the check"
    );
    ensure!(
        flow.set_disk("nvme-eui.0025385c1140a2b3")
            && matches!(flow.plan_check(), PlanCheck::NotRun),
        "choosing another disk leaves the earlier check standing"
    );
    ensure!(
        flow.set_disk("nvme-eui.0025385c1140a2b3") && flow.disk_choice().is_some(),
        "choosing the chosen disk again is refused"
    );

    // Nothing to install to: Continue does not work, and looking again does.
    let (mut flow, sim) = flow_on("no-disk")?;
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine)
            && flow.advance() == Step::Goto(PageId::Disk)
            && flow.disk().is_none()
            && !flow.can_continue()
            && flow.advance() == Step::Stay
            && flow.can_look_again()
            && flow
                .disk_view()
                .is_some_and(|v| v.offered().next().is_none() && v.choices.len() == 3),
        "the disk page without a disk"
    );
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no look again");
    };
    ensure!(
        !flow.can_look_again() || flow.probe_is_current(generation),
        "looking again twice"
    );
    flow.probe_finished(generation, sim.probe());

    // Markers: the page, and the choice.
    let (mut flow, _sim) = flow_on("generic")?;
    let mut flow = std::mem::replace(&mut flow, Flow::new(sample_layouts()?))
        .with_markers(config(true, false));
    flow.window_shown("renderer=test scale=1");
    flow.drain_markers();
    flow.advance();
    flow.advance();
    let lines = flow.drain_markers();
    ensure!(
        lines.last().is_some_and(|l| l.starts_with("ILEX-INSTALLER page=disk edition=gnome disk=nvme-eui.0025385c1140a2b3 offered=2 unavailable=2")),
        "the disk page's marker: {lines:?}"
    );
    flow.set_disk("ata-WDC_WDS500G2B0A_214512800345");
    flow.set_disk("ata-WDC_WDS500G2B0A_214512800345");
    ensure!(
        flow.drain_markers() == ["ILEX-INSTALLER event=disk disk=ata-WDC_WDS500G2B0A_214512800345"],
        "the disk's marker is not printed once"
    );
    Ok(())
}

/// The review page's destructive button, the dialog, `Apply` and the installing
/// page to the done page, with every refusal on the way.
fn installation() -> Result<()> {
    let (mut flow, sim) = at_review("generic")?;
    let review = flow.review().context("no review")?;
    ensure!(
        review.disk_label == "Samsung SSD 990 PRO 2TB S73VNJ0W412345X"
            && review.erase
                == "Everything on Samsung SSD 990 PRO 2TB S73VNJ0W412345X will be erased, including 1 other system found on it."
            && review.systems.len() == 1
            && review.button == "Erase Samsung SSD 990 PRO 2TB and install"
            && review.partitions.len() == 3
            && review.partitions[0].title == "EFI system partition"
            && review.partitions[0].detail == "512 MiB · vfat"
            && review.partitions[2].detail.ends_with("btrfs")
            && review
                .image
                .iter()
                .any(|r| r.title == "Version" && r.value == "45.20261126.0")
            && review
                .image
                .iter()
                .any(|r| r.title == "Download" && r.value == "2.2 GiB")
            && review
                .image
                .iter()
                .any(|r| r.title == "Digest" && r.value.starts_with("sha256:9f2c"))
            && review
                .choices
                .iter()
                .any(|r| r.title == "Administrator" && r.value == "ilexmaint")
            && review
                .choices
                .iter()
                .any(|r| r.title == "Edition" && r.value == "ilex Glabra")
            && review.warnings.is_empty(),
        "the review: {review:#?}"
    );
    ensure!(
        flow.phase() == Phase::Idle && flow.can_go_back() && flow.can_install(),
        "the review page before the button"
    );
    // The button asks for the dialog and nothing else: nothing is written, the
    // helper is not asked, and the choices are frozen under it.
    let Step::Confirm(dialog) = flow.install() else {
        bail!("the button does not open the dialog");
    };
    ensure!(
        dialog.title == "Erase Samsung SSD 990 PRO 2TB S73VNJ0W412345X?"
            && dialog
                .body
                .contains("will be erased, including 1 other system")
            && dialog.body.contains("cannot be undone")
            && dialog.cancel == "Cancel"
            && dialog.confirm == "Erase and install"
            && flow.confirm_dialog() == Some(dialog)
            && flow.phase() == Phase::Confirming
            && flow.is_frozen()
            && !flow.can_install()
            && !flow.can_go_back()
            && !flow.set_administrator("other")
            && !flow.set_disk("ata-WDC_WDS500G2B0A_214512800345")
            && flow.install() == Step::Stay
            && flow.retry() == Step::Stay,
        "the dialog"
    );
    flow.cancel_install();
    ensure!(
        flow.phase() == Phase::Idle
            && flow.can_install()
            && flow.confirm_dialog().is_none()
            && flow.confirm_install() == Step::Stay,
        "cancelling the dialog"
    );
    // Nothing was asked of the helper: the simulation has no installation.
    ensure!(
        sim.state()?.stage == Stage::Idle,
        "the helper was asked before the confirmation"
    );
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog the second time");
    };
    let step = flow.confirm_install();
    let Step::RunApply {
        generation,
        plan,
        erase_disk,
        password,
    } = step
    else {
        bail!("the confirmation does not apply");
    };
    ensure!(
        plan == "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e"
            && erase_disk == "nvme-eui.0025385c1140a2b3"
            && password.expose() == "correct horse"
            && format!("{password:?}") == "Secret(<redacted>)"
            && flow.is_starting()
            && flow.confirm_install() == Step::Stay
            && flow.page() == PageId::Review,
        "what is given to the helper"
    );
    // The answer to a request that is not the one in flight changes nothing.
    ensure!(
        flow.apply_finished(
            generation + 1,
            sim.start_installation(&plan, &erase_disk, &password)
        ) == Step::Stay
            && flow.is_starting(),
        "a stale answer to Apply"
    );
    let started = sim.start_installation(&plan, &erase_disk, &password);
    let first = flow.apply_finished(generation, started.clone());
    let Step::RunState { delay_ms, .. } = first else {
        bail!("an accepted Apply does not poll");
    };
    ensure!(
        delay_ms == 1000
            && flow.page() == PageId::Installing
            && flow.phase() == Phase::Running
            && flow.apply_finished(generation, started) == Step::Stay,
        "the installing page"
    );
    let view = flow.installing_view().context("no installing view")?;
    ensure!(
        view.heading == "Installing ilex Glabra"
            && view.rows.len() == 6
            && view.rows[0].status == crate::progress::Status::Running
            && view.rows[1].status == crate::progress::Status::Pending
            && !view.log.is_empty()
            && view.overall > 0.0
            && view.overall < 0.2,
        "the first state: {view:?}"
    );
    // Follow the installation to its end, one request at a time.
    let mut step = first;
    let mut seen = BTreeSet::new();
    let mut polls = 0;
    while let Step::RunState { generation, .. } = step {
        polls += 1;
        ensure!(polls < 40, "the installation never ends");
        step = flow.state_finished(generation, sim.state());
        if let Some(view) = flow.installing_view() {
            seen.extend(
                view.rows
                    .iter()
                    .filter(|r| r.status == crate::progress::Status::Running)
                    .map(|r| r.stage),
            );
        }
    }
    ensure!(
        flow.page() == PageId::Done && flow.phase() == Phase::Succeeded && seen.len() >= 4,
        "the installation did not end on the done page after seeing its stages ({seen:?}, page {:?})",
        flow.page()
    );
    ensure!(
        flow.password().is_empty() && flow.confirmation().is_empty(),
        "the password is still held after the installation succeeded"
    );
    let done = flow.done_view().context("no done view")?;
    ensure!(
        done.heading == "ilex Glabra is installed"
            && done
                .rows
                .iter()
                .any(|r| r.title == "Version" && r.value == "45.20261126.0")
            && done
                .rows
                .iter()
                .any(|r| r.title == "Configuration" && r.value == "generic.gnome")
            && done
                .rows
                .iter()
                .any(|r| r.title == "Installed on" && r.value.contains("Samsung"))
            && done.can_finish
            && done.finishing.is_none()
            && !flow.can_go_back()
            && !flow.can_continue()
            && flow.advance() == Step::Stay
            && flow.problem_view().is_none(),
        "the done page: {done:?}"
    );
    // Restart: asked once, accepted, then the page says so.
    let Step::RunFinish { generation, action } = flow.restart() else {
        bail!("Restart does not ask the helper");
    };
    ensure!(
        action == FinishAction::Restart
            && flow.restart() == Step::Stay
            && flow.power_off() == Step::Stay
            && !flow.can_finish()
            && flow.finish_finished(generation + 1, Ok(None)) == Step::Stay,
        "Restart is asked twice"
    );
    flow.finish_finished(generation, sim.finish(&action));
    let done = flow.done_view().context("no done view")?;
    ensure!(
        done.finishing == Some("Restarting…") && !done.can_finish && done.error.is_none(),
        "after Restart: {done:?}"
    );
    // A refused Power off says so and can be asked again.
    let (mut flow, sim) = at_review("generic")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    ensure!(flow.page() == PageId::Done, "no done page after settling");
    let Step::RunFinish { generation, .. } = flow.power_off() else {
        bail!("Power off does not ask the helper");
    };
    flow.finish_finished(
        generation,
        Err(ClientError::Refused(PlanError::new(
            ErrorCode::InstallationActive,
            None,
            "an installation is running",
        ))),
    );
    let done = flow.done_view().context("no done view")?;
    ensure!(
        done.error.is_some() && done.finishing.is_none() && done.can_finish,
        "after a refused Power off: {done:?}"
    );
    Ok(())
}

/// Every way an installation or its start can go wrong, and what the flow
/// shows for each.
fn failures() -> Result<()> {
    // The image stage fails after the disk was written.
    let (mut flow, sim) = at_review("generic+fail-image")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    ensure!(
        flow.page() == PageId::Problem && flow.phase() == Phase::Failed,
        "no problem page after a failed image stage: {:?}",
        flow.page()
    );
    let problem = flow.problem_view().context("no problem view")?;
    ensure!(
        problem.kind
            == ProblemKind::Failed {
                stage: Stage::Image
            }
            && problem.message.title
                == "The installation failed while downloading and installing ilex"
            && problem.message.detail.contains("connection reset by peer.")
            && problem
                .message
                .detail
                .contains("Samsung SSD 990 PRO 2TB S73VNJ0W412345X has been changed")
            && problem
                .log
                .last()
                .is_some_and(|l| l.starts_with("error: Pulling image"))
            && problem.can_retry
            && problem.can_save_log
            && problem.can_restart
            && !flow.can_go_back()
            && flow.done_view().is_none(),
        "the problem page: {problem:?}"
    );
    // Try again: a fresh look, the same choices checked, the review page, and
    // the erase asked for again.
    let Step::RunProbe { generation } = flow.retry_install() else {
        bail!("Try again does not look at the computer first");
    };
    ensure!(
        flow.page() == PageId::Review
            && flow.phase() == Phase::Idle
            && flow.is_checking()
            && flow.administrator() == "ilexmaint"
            && flow.retry_install() == Step::Stay,
        "Try again after a failure"
    );
    let step = flow.probe_finished(generation, sim.probe());
    let Step::RunPlan { .. } = step else {
        bail!("no check after the look: {step:?}");
    };
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Review
            && flow.can_install()
            && flow.phase() == Phase::Idle,
        "the review page again, to confirm the erase again: {step:?}"
    );

    // The first stage fails before anything was written.
    let (mut flow, sim) = at_review("generic+fail-partition")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    let problem = flow.problem_view().context("no problem view")?;
    ensure!(
        problem.kind
            == ProblemKind::Failed {
                stage: Stage::Partition
            }
            && problem.message.title == "The installation failed while preparing the disk"
            && problem
                .message
                .detail
                .ends_with("Nothing was written to the disk.")
            && problem.can_retry,
        "a failure before the disk was written: {problem:?}"
    );

    // The helper died and its successor reported what it found.
    let (mut flow, sim) = at_review("generic+helper-stopped")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    let problem = flow.problem_view().context("no problem view")?;
    ensure!(
        problem.kind
            == ProblemKind::Failed {
                stage: Stage::Image
            }
            && problem.message.detail.contains("helper stopped")
            && problem.message.detail.matches("changed").count() == 1
            && problem
                .message
                .detail
                .ends_with("Try again to install from the start, or save the log and restart."),
        "a helper that stopped: {problem:?}"
    );

    // The helper stops answering: a few requests of patience, the page that
    // says so, and slower asking until it is back.
    let (mut flow, sim) = at_review("generic+lost")?;
    let step = start_install(&mut flow, &sim)?;
    let mut step = step;
    let mut polls = 0;
    while let Step::RunState {
        generation,
        delay_ms,
    } = step
    {
        polls += 1;
        ensure!(polls < 60, "the helper is never given up on");
        if flow.page() == PageId::Problem {
            ensure!(
                delay_ms == 2000,
                "a helper that is gone is asked every {delay_ms} ms"
            );
            break;
        }
        step = flow.state_finished(generation, sim.state());
    }
    let problem = flow.problem_view().context("no problem view")?;
    ensure!(
        flow.phase() == Phase::Lost
            && problem.kind == ProblemKind::Lost
            && problem.message.title == "The installer lost contact with its helper"
            && problem.can_retry
            && !problem.can_save_log
            && problem.can_restart,
        "the helper is lost: {problem:?}"
    );
    ensure!(
        matches!(flow.retry_install(), Step::RunState { delay_ms: 0, .. }),
        "Try again does not ask the helper again"
    );

    // A refusal of Apply (nothing was written): the review page, with Try again.
    let (mut flow, sim) = at_review("generic")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { generation, .. } = flow.confirm_install() else {
        bail!("no apply");
    };
    let step = flow.apply_finished(
        generation,
        Err(ClientError::Refused(PlanError::new(
            ErrorCode::PlanExpired,
            Some("plan"),
            "the plan is 16 minutes old",
        ))),
    );
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Review
            && flow.phase() == Phase::Idle
            && flow
                .check_problem()
                .is_some_and(|m| m.title.contains("no longer valid"))
            && flow.can_retry()
            && !flow.can_install()
            && flow.administrator() == "ilexmaint",
        "a refused Apply"
    );
    let Step::RunProbe { generation } = flow.retry() else {
        bail!("Try again after a refused Apply does not look again");
    };
    let step = flow.probe_finished(generation, sim.probe());
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Stay && flow.page() == PageId::Review && flow.can_install(),
        "the plan is made again: {step:?}"
    );

    // The answer to Apply is lost: the helper is asked. If it says nothing
    // began, the review page says so; if it says the installation runs, so does
    // the flow.
    let (mut flow, _sim) = at_review("generic")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { generation, .. } = flow.confirm_install() else {
        bail!("no apply");
    };
    let Step::RunState {
        generation: asked,
        delay_ms,
    } = flow.apply_finished(
        generation,
        Err(ClientError::HelperUnavailable("the answer was lost".into())),
    )
    else {
        bail!("a lost answer to Apply does not ask the helper");
    };
    ensure!(
        delay_ms < 1000 && flow.is_starting() && flow.page() == PageId::Review,
        "waiting for the helper's word"
    );
    ensure!(
        flow.state_finished(asked + 5, Ok(State::idle(1))) == Step::Stay && flow.is_starting(),
        "a stale state answer"
    );
    let step = flow.state_finished(asked, Ok(State::idle(1)));
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Review
            && flow.phase() == Phase::Idle
            && flow
                .check_problem()
                .is_some_and(|m| m.title == "The installation did not start"
                    && m.detail.contains("Nothing was written")),
        "the helper says nothing began: {step:?}"
    );
    // The same loss, and this time the helper did begin.
    let (mut flow, sim) = at_review("generic")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply {
        generation,
        plan,
        erase_disk,
        password,
    } = flow.confirm_install()
    else {
        bail!("no apply");
    };
    let began = sim.start_installation(&plan, &erase_disk, &password)?;
    let Step::RunState {
        generation: asked, ..
    } = flow.apply_finished(
        generation,
        Err(ClientError::HelperUnavailable("the answer was lost".into())),
    )
    else {
        bail!("no state request");
    };
    let step = flow.state_finished(asked, Ok(began));
    ensure!(
        matches!(step, Step::RunState { .. })
            && flow.page() == PageId::Installing
            && flow.phase() == Phase::Running,
        "the helper says it began: {step:?}"
    );
    // A helper that does not answer at all after a lost answer is the lost
    // page, not a review page that claims nothing happened.
    let (mut flow, _sim) = at_review("generic")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { generation, .. } = flow.confirm_install() else {
        bail!("no apply");
    };
    let mut step = flow.apply_finished(
        generation,
        Err(ClientError::HelperUnavailable("gone".into())),
    );
    let mut polls = 0;
    while let Step::RunState { generation, .. } = step {
        polls += 1;
        ensure!(polls < 40, "never given up on");
        step = flow.state_finished(
            generation,
            Err(ClientError::HelperUnavailable("gone".into())),
        );
        if flow.phase() == Phase::Lost {
            break;
        }
    }
    ensure!(
        flow.phase() == Phase::Lost && flow.page() == PageId::Problem,
        "a helper that never answers after Apply: {:?} {:?}",
        flow.phase(),
        flow.page()
    );
    Ok(())
}

/// An interface that starts while an installation is under way, or ended while
/// it was away, comes back on that page.
fn attaching() -> Result<()> {
    for (name, page, phase, marker) in [
        (
            "generic+attach-installing",
            PageId::Installing,
            Phase::Running,
            "installing",
        ),
        (
            "generic+attach-done",
            PageId::Done,
            Phase::Succeeded,
            "done",
        ),
        (
            "generic+attach-failed",
            PageId::Problem,
            Phase::Failed,
            "failed",
        ),
    ] {
        let sim = Simulated::load(name)?;
        let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
        flow.window_shown("renderer=test scale=1");
        flow.drain_markers();
        let Step::RunState {
            generation,
            delay_ms,
        } = flow.start()
        else {
            bail!("{name}: start does not ask where the installation is");
        };
        ensure!(
            delay_ms == 0 && flow.start() == Step::Stay && flow.start_probe_is_idle(),
            "{name}: a second start"
        );
        let step = flow.state_finished(generation, sim.state());
        ensure!(
            flow.page() == page
                && flow.phase() == phase
                && (page != PageId::Installing
                    || matches!(step, Step::RunState { delay_ms: 1000, .. }))
                && (page == PageId::Installing || step == Step::Goto(page)),
            "{name}: the flow is on {:?} ({:?}), asked {step:?}",
            flow.page(),
            flow.phase()
        );
        let lines = flow.drain_markers();
        ensure!(
            lines[0] == format!("ILEX-INSTALLER event=attach result={marker}")
                && lines.iter().any(|l| l
                    .starts_with(&format!("ILEX-INSTALLER page={} edition=gnome", page.id()))
                    && l.contains("reattached=true")),
            "{name}: the markers: {lines:?}"
        );
        match page {
            PageId::Installing => {
                let view = flow.installing_view().context("no installing view")?;
                ensure!(
                    view.rows.len() == 6
                        && view.rows[2].status == crate::progress::Status::Running
                        && view.heading == "Installing ilex Glabra",
                    "{name}: {view:?}"
                );
            }
            PageId::Done => {
                let done = flow.done_view().context("no done view")?;
                ensure!(
                    done.heading == "ilex Glabra is installed",
                    "{name}: {done:?}"
                );
            }
            _ => {
                let problem = flow.problem_view().context("no problem view")?;
                ensure!(
                    problem.kind
                        == ProblemKind::Failed {
                            stage: Stage::Image
                        }
                        && problem.can_retry,
                    "{name}: {problem:?}"
                );
                // The password and the choices are gone with the old
                // interface: Try again starts from the welcome page.
                let Step::RunProbe { .. } = flow.retry_install() else {
                    bail!("{name}: Try again does not look at the computer");
                };
                ensure!(
                    flow.page() == PageId::Welcome && flow.phase() == Phase::Idle,
                    "{name}: Try again lands on {:?}",
                    flow.page()
                );
            }
        }
    }
    // The window can be shown before the helper has answered: the first page's
    // marker, with the toolkit's words, comes with the answer.
    for (name, page) in [
        ("generic", "welcome"),
        ("generic+attach-installing", "installing"),
    ] {
        let sim = Simulated::load(name)?;
        let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
        let Step::RunState { generation, .. } = flow.start() else {
            bail!("{name}: no state request");
        };
        flow.window_shown("renderer=gl scale=1");
        ensure!(
            flow.is_attaching() && flow.drain_markers().is_empty(),
            "{name}: a page was announced before the helper answered"
        );
        flow.state_finished(generation, sim.state());
        let lines = flow.drain_markers();
        let announced: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with("ILEX-INSTALLER page="))
            .collect();
        ensure!(
            announced.len() == 1
                && announced[0].starts_with(&format!("ILEX-INSTALLER page={page} edition=gnome"))
                && announced[0].contains(" renderer=gl scale=1 uptime="),
            "{name}: the first page's marker: {lines:?}"
        );
    }
    // Nothing under way (or the helper not answering): the first look begins.
    for name in ["generic", "helper-down"] {
        let sim = Simulated::load(name)?;
        let mut flow = Flow::new(sample_layouts()?);
        let Step::RunState { generation, .. } = flow.start() else {
            bail!("{name}: no state request");
        };
        let step = flow.state_finished(generation, sim.state());
        ensure!(
            matches!(step, Step::RunProbe { .. }) && flow.page() == PageId::Welcome,
            "{name}: the first look does not begin: {step:?}"
        );
    }
    let sim = Simulated::load("generic")?;
    let mut flow = Flow::new(sample_layouts()?);
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("no state request");
    };
    ensure!(
        flow.state_finished(generation + 1, sim.state()) == Step::Stay
            && flow.state_finished(
                generation,
                Err(ClientError::HelperUnavailable("not up".into()))
            ) != Step::Stay,
        "the start-up answer"
    );
    Ok(())
}

/// Saving the log to a stick: the dialog's states, and the helper's rules as
/// the simulation applies them.
fn log_dialog() -> Result<()> {
    let (mut flow, sim) = at_review("generic+fail-image+sticks")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    ensure!(flow.page() == PageId::Problem, "no problem page");
    ensure!(
        flow.log_state() == &LogSave::Closed
            && flow.save_log("usb-SanDisk_Cruzer_Blade_4C530001-0:0") == Step::Stay,
        "a stick was written to before the dialog"
    );
    let Step::RunLogProbe { generation } = flow.choose_log_device() else {
        bail!("Save the log does not look for sticks");
    };
    ensure!(
        flow.log_state() == &LogSave::Looking
            && !flow.can_save_log()
            && flow.log_probe_finished(generation + 1, sim.probe()) == Step::Stay
            && flow.log_state() == &LogSave::Looking,
        "while looking"
    );
    flow.log_probe_finished(generation, sim.probe());
    let LogSave::Choosing(choices) = flow.log_state().clone() else {
        bail!("no choices: {:?}", flow.log_state());
    };
    // Removable disks only, never the medium the installer started from; the
    // ones the log cannot go to say why.
    let ids: Vec<&str> = choices.iter().map(|c| c.id.as_str()).collect();
    ensure!(
        ids == [
            "usb-Generic_Flash_Disk_AA00000000000489-0:0",
            "usb-SanDisk_Cruzer_Blade_4C530001-0:0",
            "usb-Kingston_DataTraveler_0123-0:0"
        ],
        "the sticks offered: {ids:?}"
    );
    ensure!(
        choices[0]
            .problem
            .as_ref()
            .is_some_and(|m| m.title.contains("cannot write"))
            && choices[1].problem.is_none()
            && choices[2].problem.is_some(),
        "which of them the log can go to: {choices:?}"
    );
    ensure!(
        flow.save_log("usb-Kingston_DataTraveler_0123-0:0") == Step::Stay
            && flow.save_log("ata-WDC_WDS500G2B0A_214512800345") == Step::Stay
            && flow.save_log("nope") == Step::Stay
            && flow.log_state() == &LogSave::Choosing(choices.clone()),
        "a device that cannot take the log was written to"
    );
    let Step::RunFinish { generation, action } =
        flow.save_log("usb-SanDisk_Cruzer_Blade_4C530001-0:0")
    else {
        bail!("no write");
    };
    ensure!(
        action
            == FinishAction::SaveLog {
                device: "usb-SanDisk_Cruzer_Blade_4C530001-0:0".into()
            }
            && matches!(flow.log_state(), LogSave::Saving { .. })
            && !flow.can_finish()
            && !flow.can_retry_install()
            && flow.restart() == Step::Stay
            && flow.save_log("usb-SanDisk_Cruzer_Blade_4C530001-0:0") == Step::Stay,
        "while writing"
    );
    flow.close_log();
    ensure!(
        matches!(flow.log_state(), LogSave::Saving { .. }),
        "the dialog closed while the log was written"
    );
    flow.finish_finished(generation, sim.finish(&action));
    let LogSave::Saved(saved) = flow.log_state().clone() else {
        bail!("not saved: {:?}", flow.log_state());
    };
    ensure!(
        saved.file.starts_with("ilex-installer-log-5ca1ab1e-")
            && saved.file.ends_with(".tar")
            && saved.label == "Cruzer Blade 4C530001"
            && flow.can_finish(),
        "the saved log: {saved:?}"
    );
    flow.close_log();
    ensure!(
        flow.log_state() == &LogSave::Closed,
        "the dialog stays open"
    );

    // The helper's own rules, as the simulation applies the model's.
    for (device, code) in [
        ("ata-WDC_WDS500G2B0A_214512800345", ErrorCode::NotRemovable),
        ("nvme-eui.0025385c1140a2b3", ErrorCode::InstallTarget),
        (
            "usb-SanDisk_Ultra_Fit_4C530001230815113423-0:0",
            ErrorCode::LiveMedium,
        ),
        (
            "usb-Kingston_DataTraveler_0123-0:0",
            ErrorCode::NoFileSystem,
        ),
        ("usb-nothing-0:0", ErrorCode::UnknownDisk),
    ] {
        let Err(ClientError::Refused(error)) = sim.finish(&FinishAction::SaveLog {
            device: device.into(),
        }) else {
            bail!("{device} is accepted as a place for the log");
        };
        ensure!(
            error.code == code,
            "{device}: {:?}, expected {code:?}",
            error.code
        );
    }

    // A write that fails says so and leaves the dialog to try again.
    let (mut flow, sim) = at_review("generic+fail-image+sticks+log-fails")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    let step = flow.choose_log_device();
    let step = settle(&mut flow, &sim, step)?;
    ensure!(step == Step::Stay, "the dialog's look");
    let Step::RunFinish { generation, action } =
        flow.save_log("usb-SanDisk_Cruzer_Blade_4C530001-0:0")
    else {
        bail!("no write");
    };
    flow.finish_finished(generation, sim.finish(&action));
    ensure!(
        matches!(flow.log_state(), LogSave::Failed(m) if m.title == "The log was not saved"),
        "a failed write: {:?}",
        flow.log_state()
    );
    let Step::RunLogProbe { .. } = flow.choose_log_device() else {
        bail!("no way to try again after a failed write");
    };
    Ok(())
}

/// The serial-console lines of an installation, in the order their causes are
/// accepted.
fn install_markers() -> Result<()> {
    let sim = Simulated::load("generic+fail-image")?;
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
    flow.window_shown("renderer=test scale=1");
    let step = flow.start();
    settle(&mut flow, &sim, step)?;
    flow.advance();
    flow.advance();
    flow.advance();
    fill_account(&mut flow, "generic")?;
    let step = flow.advance();
    settle(&mut flow, &sim, step)?;
    ensure!(flow.can_install(), "no review page for the markers");
    flow.drain_markers();
    flow.install();
    flow.cancel_install();
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    let lines = flow.drain_markers();
    let wanted = [
        "ILEX-INSTALLER event=confirm state=shown",
        "ILEX-INSTALLER event=confirm state=cancelled",
        "ILEX-INSTALLER event=confirm state=shown",
        "ILEX-INSTALLER event=confirm state=accepted",
        "ILEX-INSTALLER event=install result=started",
        "ILEX-INSTALLER event=stage stage=partition status=running",
        "ILEX-INSTALLER page=installing edition=gnome stage=partition progress=",
        "ILEX-INSTALLER event=stage stage=partition status=done",
        "ILEX-INSTALLER event=stage stage=mount status=running",
        "ILEX-INSTALLER event=stage stage=image status=running",
        "ILEX-INSTALLER event=stage stage=image status=failed",
        "ILEX-INSTALLER event=outcome result=failed stage=image",
        "ILEX-INSTALLER page=problem edition=gnome kind=failed stage=image retry=true reattached=false",
    ];
    let mut at = 0;
    for want in wanted {
        let Some(found) = lines[at..].iter().position(|l| l.starts_with(want)) else {
            bail!("no `{want}` after line {at} of {lines:#?}");
        };
        at += found + 1;
    }
    ensure!(
        !lines.iter().any(|l| l.contains("horse")),
        "the password is in a marker"
    );
    // Restart, power off and the log: the action's word, never the device.
    let (flow, sim) = at_review("generic+fail-image+sticks")?;
    let mut flow = flow.with_markers(config(true, false));
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    flow.drain_markers();
    let step = flow.choose_log_device();
    settle(&mut flow, &sim, step)?;
    let step = flow.save_log("usb-SanDisk_Cruzer_Blade_4C530001-0:0");
    settle(&mut flow, &sim, step)?;
    let step = flow.restart();
    settle(&mut flow, &sim, step)?;
    let lines = flow.drain_markers();
    ensure!(
        lines
            == [
                "ILEX-INSTALLER event=log state=looking offered=0 usable=0",
                "ILEX-INSTALLER event=log state=choosing offered=3 usable=1",
                "ILEX-INSTALLER event=log state=saving offered=0 usable=0",
                "ILEX-INSTALLER event=finish action=save-log result=ok",
                "ILEX-INSTALLER event=log state=saved offered=0 usable=0",
                "ILEX-INSTALLER event=finish action=restart result=ok",
            ]
            && !lines.iter().any(|l| l.contains("usb-")),
        "the log's markers: {lines:#?}"
    );
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
