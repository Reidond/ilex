//! The flow's share of `ilex-installer --self-check`: its rules exercised on
//! the simulation's fixtures, and the system's keyboard catalogue read. (The
//! repository has no unit tests; anything worth testing in a shipped binary is
//! observable from inside the VM.)

use crate::account::{self, Check};
use crate::backend::Backend;
use crate::flow::{
    Control, Flow, KeyboardStatus, LogSave, MarkerConfig, PageId, Phase, PlanCheck, ProbeState,
    ProblemIcon, ProblemKind, ReviewBody, Step, usable_disks,
};
use crate::keyboard::{self, Layouts, Sources};
use crate::language;
use crate::machine::{MachineView, Origin};
use crate::messages::{ClientError, plan_error};
use crate::progress::{Progress, Status};
use crate::recovery as page;
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
    walk().context("walk")?;
    generations().context("generations")?;
    probe_answers().context("probe_answers")?;
    keyboard_switches().context("keyboard_switches")?;
    retries().context("retries")?;
    gates().context("gates")?;
    marker_formats().context("marker_formats")?;
    bus_errors().context("bus_errors")?;
    progress().context("progress")?;
    disk_page().context("disk_page")?;
    encryption().context("encryption")?;
    installation().context("installation")?;
    install_markers().context("install_markers")?;
    failures().context("failures")?;
    attaching().context("attaching")?;
    log_dialog().context("log_dialog")?;
    starting_window().context("starting_window")?;
    refused_window().context("refused_window")?;
    other_installation().context("other_installation")?;
    interrupted_attach().context("interrupted_attach")?;
    unreachable_helper().context("unreachable_helper")?;
    chosen_disk().context("chosen_disk")?;
    wiped_secrets().context("wiped_secrets")?;
    page_views().context("page_views")?;
    recovery_key().context("recovery_key")?;
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
            && through_encryption(&mut flow),
        "{name}: welcome, machine, disk and encryption do not continue"
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

/// From the disk page, past the encryption page with encryption turned off (the
/// rules of the checks that are not about encryption are the same either way),
/// to the account page.
fn through_encryption(flow: &mut Flow) -> bool {
    flow.advance() == Step::Goto(PageId::Encryption)
        && flow.set_encrypt(false)
        && flow.can_continue()
        && flow.advance() == Step::Goto(PageId::Account)
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
            && flow.advance() == Step::Goto(PageId::Encryption),
        "machine -> disk -> encryption"
    );
    // The disk is encrypted unless the person says otherwise, and the page
    // names the layout the passphrase is typed in.
    ensure!(
        flow.encrypt()
            && !flow.can_continue()
            && flow.advance() == Step::Stay
            && flow
                .passphrase_layout_line()
                .contains("German (no dead keys)"),
        "encryption is on by default and waits for a passphrase"
    );
    ensure!(
        flow.set_passphrase(Secret::new("correct horse battery"))
            && !flow.can_continue()
            && flow.set_passphrase_confirmation(Secret::new("correct horse batter"))
            && flow.encryption_checks().confirmation.message().is_some()
            && !flow.can_continue()
            && flow.set_passphrase_confirmation(Secret::new("correct horse battery"))
            && flow.can_continue(),
        "the passphrase and its confirmation"
    );
    ensure!(
        flow.advance() == Step::Goto(PageId::Account),
        "encryption -> account"
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
            && request.encrypt
            && request.recovery_key
            && request.tpm == Some(ilex_installer_model::plan::TpmMode::Auto)
            && request.keyboard == "de-nodeadkeys"
            && request.language == "de_DE.UTF-8"
            && request.composition == "ms-7c35.gnome"
            && request.disk == "nvme-eui.0025385c1140a2b3",
        "the plan request: {request:?}"
    );
    ensure!(
        !serde_json::to_string(&*request)?.contains("horse"),
        "the password or the passphrase is in the plan request"
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
        flow.back() == Some(PageId::Encryption)
            && flow.back() == Some(PageId::Disk)
            && flow.back() == Some(PageId::Machine),
        "back to the machine page"
    );
    ensure!(flow.select_machine("ms-7c35.gnome"), "machine choice");
    ensure!(
        flow.advance() == Step::Goto(PageId::Disk) && through_encryption(&mut flow),
        "machine -> disk -> encryption -> account"
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
            && through_encryption(&mut flow)
            && flow.set_administrator("two words\nhere")
            && flow.back() == Some(PageId::Encryption)
            && flow.back() == Some(PageId::Disk)
            && flow.advance() == Step::Goto(PageId::Encryption)
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
            passphrase,
        } => flow.apply_finished(
            generation,
            sim.start_installation(&plan, &erase_disk, &password, passphrase.as_ref()),
        ),
        Step::RunState { generation, .. } => flow.state_finished(generation, sim.state()),
        Step::RunTakeKey { generation, plan } => {
            flow.take_key_finished(generation, sim.take_recovery_key(&plan))
        }
        Step::RunAcknowledge {
            generation,
            plan,
            answer,
        } => flow.confirm_finished(generation, sim.acknowledge_recovery_key(&plan, &answer)),
        Step::RunContinueWithout { generation, plan } => {
            flow.skip_finished(generation, sim.continue_without_recovery_key(&plan))
        }
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
                | Step::RunTakeKey { .. }
                | Step::RunAcknowledge { .. }
                | Step::RunContinueWithout { .. }
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

/// The encryption page: on unless turned off, a passphrase typed twice, what is
/// asked of the helper and what is not, and where the passphrase goes.
fn encryption() -> Result<()> {
    let (mut flow, sim) = flow_on("generic")?;
    flow.advance();
    flow.advance();
    ensure!(
        flow.advance() == Step::Goto(PageId::Encryption)
            && flow.page() == PageId::Encryption
            && PageId::Encryption.id() == "encryption"
            && PageId::Disk.next() == Some(PageId::Encryption)
            && PageId::Account.previous() == Some(PageId::Encryption)
            && flow.can_go_back()
            && flow.encrypt()
            && flow.marker_fields() == "encryption=on keyboard=us chip=unavailable",
        "the page after the disk page, with encryption on"
    );
    ensure!(
        flow.passphrase_layout_line()
            == "You type it in the English (US) layout: here, and every time the computer starts.",
        "the layout line: {}",
        flow.passphrase_layout_line()
    );
    // Every state of the two fields.
    let checks = |flow: &Flow| flow.encryption_checks();
    ensure!(
        checks(&flow).passphrase == Check::Empty
            && checks(&flow).confirmation == Check::Empty
            && !flow.can_continue()
            && flow.advance() == Step::Stay,
        "an empty page continues"
    );
    for (text, valid) in [
        ("a", true),
        ("correct horse battery staple", true),
        ("Zürich ß 日本語", true),
        (" spaces are part of it ", true),
        ("line\nbreak", false),
        ("tab\tbed", false),
        ("nul\0byte", false),
    ] {
        flow.set_passphrase(Secret::new(text));
        ensure!(
            checks(&flow).passphrase.is_valid() == valid
                && (valid || checks(&flow).passphrase.message().is_some()),
            "the rule for {text:?}"
        );
    }
    flow.set_passphrase(Secret::new(&"x".repeat(513)));
    ensure!(
        checks(&flow)
            .passphrase
            .message()
            .is_some_and(|m| m.title == "The passphrase is too long"),
        "a passphrase longer than the prompt takes"
    );
    flow.set_passphrase(Secret::new("short"));
    ensure!(
        checks(&flow).passphrase.is_valid()
            && flow
                .passphrase_advice()
                .is_some_and(|m| m.title == "A longer passphrase is safer"),
        "a short passphrase is advised against, not refused"
    );
    flow.set_passphrase(Secret::new("correct horse battery staple"));
    ensure!(
        flow.passphrase_advice().is_none()
            && !flow.can_continue()
            && flow.set_passphrase_confirmation(Secret::new("correct horse battery stapl"))
            && checks(&flow)
                .confirmation
                .message()
                .is_some_and(|m| m.title == "The passphrases do not match")
            && !flow.can_continue()
            && flow.advance() == Step::Stay,
        "a mismatch continues"
    );
    // The secrets are held, not printed.
    ensure!(
        format!("{:?}", flow.passphrase()) == "Secret(<redacted>)"
            && flow.passphrase_confirmation().expose() == "correct horse battery stapl",
        "the passphrase's debug output"
    );
    flow.set_passphrase_confirmation(Secret::new("correct horse battery staple"));
    ensure!(
        flow.can_continue() && flow.advance() == Step::Goto(PageId::Account),
        "a confirmed passphrase does not continue"
    );
    // Back keeps what was typed; turning encryption off on the page keeps it
    // too, and the page continues without it.
    ensure!(
        flow.back() == Some(PageId::Encryption)
            && flow.passphrase().expose() == "correct horse battery staple"
            && flow.set_encrypt(false)
            && !flow.encrypt()
            && flow.can_continue()
            && flow.marker_fields() == "encryption=off keyboard=us chip=none"
            && flow.set_encrypt(true)
            && flow.can_continue(),
        "going back and turning encryption off and on"
    );

    // What `Plan` is asked: encrypted, the security chip left alone (the
    // installation does not enrol it yet), and nothing of the passphrase.
    ensure!(
        flow.advance() == Step::Goto(PageId::Account),
        "encryption -> account"
    );
    fill_account(&mut flow, "generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    let text = serde_json::to_string(&*request)?;
    ensure!(
        request.encrypt
            && request.recovery_key
            && request.tpm == Some(ilex_installer_model::plan::TpmMode::Auto)
            && !text.contains("battery")
            && !text.contains("horse"),
        "the request of an encrypted installation: {text}"
    );
    let plan = sim.plan(&request)?;
    ensure!(
        plan.encryption.enabled
            && !plan.encryption.tpm.will_enroll
            && plan.warnings.iter().any(|w| w.contains("security chip")
                && w.contains("is not used")
                && w.contains("passphrase at every start"))
            && plan
                .partitions
                .iter()
                .filter(|p| p.encrypted)
                .map(|p| p.kind.as_str())
                .collect::<Vec<_>>()
                == ["root"],
        "the plan of an encrypted installation: {plan:?}"
    );
    let rows: Vec<Stage> = Progress::for_plan(&plan)
        .rows
        .iter()
        .map(|r| r.stage)
        .collect();
    ensure!(
        rows == Stage::ENCRYPTED,
        "the stages of an encrypted plan: {rows:?}"
    );
    flow.plan_finished(generation, Ok(plan));
    let review = flow.review().context("no review page")?;
    ensure!(
        review
            .choices
            .iter()
            .any(|r| r.title == "Encryption" && r.value.starts_with("On: ")),
        "the review page's choices: {:?}",
        review.choices
    );
    ensure!(
        review
            .partitions
            .iter()
            .any(|p| p.detail.ends_with("btrfs, encrypted")),
        "the review page's partitions: {:?}",
        review.partitions
    );

    // The helper gets the passphrase, and the password, and begins.
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply {
        passphrase,
        password,
        ..
    } = flow.confirm_install()
    else {
        bail!("the confirmation does not apply");
    };
    ensure!(
        passphrase.as_ref().map(Secret::expose) == Some("correct horse battery staple")
            && password.expose() == "correct horse",
        "what the helper is given"
    );
    // ...and no passphrase for a plan that does not encrypt, even if one was
    // typed while encryption was on.
    let (mut flow, sim) = at_account("generic")?;
    flow.show(PageId::Encryption);
    ensure!(
        flow.set_encrypt(true)
            && flow.set_passphrase(Secret::new("typed while it was on"))
            && flow.set_passphrase_confirmation(Secret::new("typed while it was on"))
            && flow.set_encrypt(false),
        "turning encryption off after typing"
    );
    flow.show(PageId::Account);
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        !request.encrypt
            && request.tpm.is_none()
            && !serde_json::to_string(&*request)?.contains("typed"),
        "the request of an unencrypted installation: {request:?}"
    );
    flow.plan_finished(generation, sim.plan(&request));
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { passphrase, .. } = flow.confirm_install() else {
        bail!("the confirmation does not apply");
    };
    ensure!(
        passphrase.is_none(),
        "a passphrase went to an unencrypted plan"
    );
    flow.cancel_install();

    // The simulated helper refuses what the real one does: a passphrase for a
    // plan that is not encrypted, an encrypted plan without one, one that
    // cannot be typed at the prompt.
    let (mut flow, sim) = flow_on("generic")?;
    flow.advance();
    flow.advance();
    flow.advance();
    flow.set_passphrase(Secret::new("correct horse battery staple"));
    flow.set_passphrase_confirmation(Secret::new("correct horse battery staple"));
    flow.advance();
    fill_account(&mut flow, "generic")?;
    let (generation, request) = run_plan(&mut flow)?;
    flow.plan_finished(generation, sim.plan(&request));
    let plan = match flow.plan_check() {
        PlanCheck::Checked(plan) => plan.id.clone(),
        other => bail!("the plan: {other:?}"),
    };
    let secret = Secret::new("correct horse battery staple");
    let refused = |result: Result<State, ClientError>, field: &str| match result {
        Err(ClientError::Refused(error)) if error.field.as_deref() == Some(field) => Ok(()),
        other => bail!("wanted a refusal naming {field}: {other:?}"),
    };
    refused(
        sim.start_installation(&plan, "nvme-eui.0025385c1140a2b3", &secret, None),
        "disk-passphrase",
    )?;
    ensure!(
        matches!(
            sim.set_secret(
                &plan,
                ilex_installer_model::plan::SecretKind::DiskPassphrase,
                &Secret::new("a\nb")
            ),
            Err(ClientError::Refused(_))
        ),
        "a passphrase with a line break was taken"
    );
    let began =
        sim.start_installation(&plan, "nvme-eui.0025385c1140a2b3", &secret, Some(&secret))?;
    ensure!(
        began.active
            && began.stages.iter().map(|r| r.stage).collect::<Vec<_>>() == Stage::ENCRYPTED
            && began.summary.as_ref().is_some_and(|s| s.encrypted),
        "an encrypted installation begins: {began:?}"
    );
    let (mut unencrypted, sim) = at_review("generic")?;
    let Step::Confirm(_) = unencrypted.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { plan, .. } = unencrypted.confirm_install() else {
        bail!("the confirmation does not apply");
    };
    ensure!(
        matches!(
            sim.set_secret(&plan, ilex_installer_model::plan::SecretKind::DiskPassphrase, &secret),
            Err(ClientError::Refused(e)) if e.field.as_deref() == Some("fd")
        ),
        "a passphrase for a plan that is not encrypted was taken"
    );

    // The done page of an encrypted installation says what to expect, and the
    // passphrase is wiped with the password.
    let (mut flow, sim) = at_review("generic+encrypted")?;
    flow.show(PageId::Encryption);
    flow.set_encrypt(true);
    flow.set_passphrase(Secret::new("correct horse battery staple"));
    flow.set_passphrase_confirmation(Secret::new("correct horse battery staple"));
    flow.show(PageId::Account);
    let (generation, request) = run_plan(&mut flow)?;
    flow.plan_finished(generation, sim.plan(&request));
    let step = start_install(&mut flow, &sim)?;
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Goto(PageId::Done) || flow.page() == PageId::Done,
        "the installation does not end: {step:?}"
    );
    let done = flow.done_view().context("no done page")?;
    ensure!(
        done.detail
            .contains("The computer asks for the passphrase at every start: Secure Boot is off")
            && flow.passphrase().is_empty()
            && flow.passphrase_confirmation().is_empty()
            && flow.password().is_empty()
            && flow.secrets_wiped() == 1,
        "the done page of an encrypted installation: {done:?}"
    );
    // A layout chosen after the passphrase and password were typed: what was
    // typed holds the old layout's characters, so the fields are emptied and
    // the pages ask for them again (the boot prompt would not unlock).
    let (mut flow, _) = flow_on("generic")?;
    flow.advance();
    flow.advance();
    ensure!(
        flow.advance() == Step::Goto(PageId::Encryption)
            && flow.set_passphrase(Secret::new("zebra-yoga"))
            && flow.set_passphrase_confirmation(Secret::new("zebra-yoga"))
            && flow.can_continue()
            && flow.advance() == Step::Goto(PageId::Account),
        "the encryption page before the layout change"
    );
    fill_account(&mut flow, "before the layout change")?;
    let wiped = flow.secrets_wiped();
    while flow.page() != PageId::Welcome {
        ensure!(flow.back().is_some(), "Back to the welcome page");
    }
    flow.set_keyboard("de-nodeadkeys");
    ensure!(
        flow.secrets_wiped() == wiped + 1
            && flow.passphrase().is_empty()
            && flow.passphrase_confirmation().is_empty()
            && flow.password().is_empty()
            && flow.confirmation().is_empty(),
        "a layout change keeps what was typed in the old layout"
    );
    // Choosing the same layout again, or one with nothing typed, wipes nothing.
    flow.set_keyboard("de-nodeadkeys");
    flow.set_keyboard("us");
    ensure!(
        flow.secrets_wiped() == wiped + 1,
        "a layout change with nothing typed counts as a wipe"
    );
    // Back at the start, encryption is on again and nothing is kept.
    let (flow, _) = flow_on("generic+attach-done+encrypted")?;
    ensure!(
        flow.done_view().is_none() && flow.encrypt(),
        "a restarted interface"
    );
    Ok(())
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
    // ...and goes into the request, whole-disk (unencrypted here: the
    // encryption page is turned off).
    ensure!(
        through_encryption(&mut flow),
        "disk -> encryption -> account"
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
        flow.back() == Some(PageId::Account)
            && flow.back() == Some(PageId::Encryption)
            && flow.back() == Some(PageId::Disk),
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
        ..
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
            sim.start_installation(&plan, &erase_disk, &password, None)
        ) == Step::Stay
            && flow.is_starting(),
        "a stale answer to Apply"
    );
    let started = sim.start_installation(&plan, &erase_disk, &password, None);
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
            && problem.retry == Control::shown(true)
            && problem.save_log == Control::shown(true)
            && problem.restart == Control::shown(true)
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
            && problem.retry.enabled,
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
            && problem.retry == Control::shown(true)
            && problem.save_log
                == (Control {
                    visible: false,
                    enabled: false,
                })
            && problem.restart == Control::shown(true),
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
        ..
    } = flow.confirm_install()
    else {
        bail!("no apply");
    };
    let began = sim.start_installation(&plan, &erase_disk, &password, None)?;
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
                        && problem.retry.enabled,
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
    // Nothing under way: the first look begins. (A helper that cannot be
    // asked is `unreachable_helper`'s.)
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
            ) == Step::Stay
            && flow.blocking_problem().is_some(),
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
    through_encryption(&mut flow);
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

// --- an Apply that is checking, a helper that cannot be asked, the chosen disk -------

/// The helper's `State` says `starting` while an `Apply` checks the image's
/// signature and claims the disk: the rest of its record is the previous
/// installation's. The installing page, never an old outcome or "did not start".
fn starting_window() -> Result<()> {
    // The interface that asked: the answer to `Apply` is lost, the helper goes
    // on. It keeps asking while `State` says `starting`, says nothing of a
    // failure to start, and follows the installation to its end.
    let (mut first, sim) = at_review("generic+apply-slow")?;
    let Step::Confirm(_) = first.install() else {
        bail!("no dialog");
    };
    let step = first.confirm_install();
    let Step::RunApply {
        generation,
        plan,
        erase_disk,
        password,
        ..
    } = step
    else {
        bail!("no apply: {step:?}");
    };
    let lost = sim.start_installation(&plan, &erase_disk, &password, None);
    ensure!(lost.is_err(), "the simulated Apply answered");
    let mut step = first.apply_finished(generation, lost);
    ensure!(
        matches!(step, Step::RunState { delay_ms: 400, .. }),
        "a lost answer does not ask where things stand: {step:?}"
    );
    // Three questions say `starting`: still the review page's own wait, no
    // sentence about not starting, and the check is not shown again.
    for round in 0..3 {
        let Step::RunState { generation, .. } = step else {
            bail!("round {round}: no state request: {step:?}");
        };
        step = first.state_finished(generation, sim.state());
        ensure!(
            matches!(step, Step::RunState { delay_ms: 1000, .. })
                && first.phase() == Phase::Starting
                && first.page() == PageId::Review
                && first.is_starting()
                && matches!(
                    first.review_page().body,
                    ReviewBody::Waiting(ref m) if m.title == "Starting the installation"
                ),
            "round {round}: while the helper checks: {step:?} {:?} {:?}",
            first.phase(),
            first.page()
        );
    }
    let step = settle(&mut first, &sim, step)?;
    ensure!(
        step == Step::Goto(PageId::Done)
            && first.page() == PageId::Done
            && first.phase() == Phase::Succeeded
            && first.installing_view().is_some(),
        "the installation did not go on after the answer was lost: {step:?} {:?}",
        first.page()
    );

    // An interface that was killed and started again in that window (a new
    // flow on the same helper): the installing page, every stage waiting, no
    // log, the previous failure not shown; the installation after it.
    let (mut earlier, sim) = at_review("generic+apply-slow+attach-failed")?;
    let Step::Confirm(_) = earlier.install() else {
        bail!("no dialog");
    };
    let Step::RunApply {
        plan,
        erase_disk,
        password,
        ..
    } = earlier.confirm_install()
    else {
        bail!("no apply");
    };
    ensure!(
        sim.start_installation(&plan, &erase_disk, &password, None)
            .is_err(),
        "the simulated Apply answered"
    );
    drop(earlier);
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
    flow.window_shown("renderer=test scale=1");
    flow.drain_markers();
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("no state request");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        flow.page() == PageId::Installing
            && flow.phase() == Phase::Running
            && matches!(step, Step::RunState { delay_ms: 1000, .. }),
        "an interface restarted during Apply is on {:?} ({:?}): {step:?}",
        flow.page(),
        flow.phase()
    );
    let view = flow.installing_view().context("no installing view")?;
    ensure!(
        view.rows.len() == 6
            && view.rows.iter().all(|r| r.status == Status::Pending)
            && view.overall == 0.0
            && view.message == "Starting the installation"
            && view.log.is_empty()
            && view.waiting.is_none(),
        "the installing page before the helper has begun: {view:?}"
    );
    let lines = flow.drain_markers();
    ensure!(
        lines[0] == "ILEX-INSTALLER event=attach result=installing"
            && lines.iter().any(|l| l.starts_with("ILEX-INSTALLER page=installing edition=gnome stage=starting progress=0 reattached=true")),
        "the markers: {lines:?}"
    );
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Goto(PageId::Done)
            && flow.page() == PageId::Done
            && flow.phase() == Phase::Succeeded
            && flow.done_view().is_some_and(|d| d.rows.len() == 4),
        "the restarted interface did not see the outcome: {step:?} {:?}",
        flow.page()
    );
    ensure!(
        flow.drain_markers()
            .iter()
            .any(|l| l == "ILEX-INSTALLER event=outcome result=success"),
        "no outcome marker after the restart"
    );

    // The same, as the screenshots have it: a restarted interface that finds
    // `starting` over an old failure.
    let sim = Simulated::load("generic+attach-starting")?;
    let mut flow = Flow::new(sample_layouts()?);
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("no state request");
    };
    flow.state_finished(generation, sim.state());
    let view = flow.installing_view().context("no installing view")?;
    ensure!(
        flow.page() == PageId::Installing
            && flow.problem_view().is_none()
            && view.rows.iter().all(|r| r.status == Status::Pending)
            && view.message == "Starting the installation",
        "an old failure was read as the outcome of a starting Apply: {view:?}"
    );

    // A refusal of Probe or Plan that says an installation is active asks where
    // it is, on whichever page the person is.
    let active = || {
        ClientError::Refused(PlanError::new(
            ErrorCode::InstallationActive,
            None,
            "an installation is running; wait for it to finish",
        ))
    };
    let sim = Simulated::load("generic+attach-installing")?;
    let (mut flow, _) = flow_on("generic")?;
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no probe");
    };
    let step = flow.probe_finished(generation, Err(active()));
    ensure!(
        matches!(step, Step::RunState { delay_ms: 0, .. }) && flow.is_attaching(),
        "a probe refused as active does not ask where it is: {step:?}"
    );
    let Step::RunState { generation, .. } = step else {
        bail!("no state request");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        flow.page() == PageId::Installing
            && flow.phase() == Phase::Running
            && matches!(step, Step::RunState { delay_ms: 1000, .. }),
        "the flow is on {:?} after the refusal",
        flow.page()
    );
    let (mut flow, _) = at_account("generic")?;
    let (generation, _request) = run_plan(&mut flow)?;
    let step = flow.plan_finished(generation, Err(active()));
    ensure!(
        matches!(step, Step::RunState { delay_ms: 0, .. })
            && flow.is_attaching()
            && !flow.is_checking(),
        "a plan refused as active does not ask where it is: {step:?}"
    );
    let Step::RunState { generation, .. } = step else {
        bail!("no state request");
    };
    flow.state_finished(generation, sim.state());
    ensure!(
        flow.page() == PageId::Installing && flow.phase() == Phase::Running,
        "the flow is on {:?} after the plan was refused",
        flow.page()
    );
    // Apply itself refused as active (an earlier answer was lost).
    let (mut flow, _) = at_review("generic")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { generation, .. } = flow.confirm_install() else {
        bail!("no apply");
    };
    let step = flow.apply_finished(generation, Err(active()));
    ensure!(
        matches!(step, Step::RunState { delay_ms: 0, .. }) && flow.is_attaching(),
        "an Apply refused as active does not ask where it is: {step:?}"
    );
    Ok(())
}

fn helper_down() -> ClientError {
    ClientError::HelperUnavailable(
        "org.freedesktop.DBus.Error.NoReply: the helper did not answer".into(),
    )
}

/// The first page a restarted interface shows: the one `State` puts it on, with
/// the markers it printed (the window was shown first).
fn restarted(sim: &Simulated) -> Result<(Flow, Step)> {
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
    flow.window_shown("renderer=test scale=1");
    flow.drain_markers();
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("a restarted interface does not ask where the installation is");
    };
    let step = flow.state_finished(generation, sim.state());
    Ok((flow, step))
}

/// The helper's `Apply` was checking when the interface was restarted, and it
/// then ended without beginning (the plan expired, the disk changed, the
/// signature; or the helper stopped while it checked). The record under `State`
/// is an earlier installation's failure, or none: it is not this attempt's
/// outcome and the helper is not lost. The welcome page says nothing was
/// written, and the next walk-through installs.
fn refused_window() -> Result<()> {
    for (name, recorded) in [
        ("generic+attach-starting-refused", true),
        ("generic+attach-starting-refused-idle", false),
    ] {
        let sim = Simulated::load(name)?;
        let (mut flow, mut step) = restarted(&sim)?;
        // While `State` says `starting`: the installing page, every stage waiting.
        let view = flow.installing_view().context("no installing view")?;
        ensure!(
            flow.page() == PageId::Installing
                && flow.phase() == Phase::Running
                && view.rows.iter().all(|r| r.status == Status::Pending)
                && view.message == "Starting the installation"
                && flow.problem_view().is_none(),
            "{name}: a restarted interface in the window: {:?} {:?}",
            flow.page(),
            flow.phase()
        );
        let mut asked = 1;
        while let Step::RunState { generation, .. } = step {
            step = flow.state_finished(generation, sim.state());
            asked += 1;
            ensure!(asked < 20, "{name}: the flow keeps asking State");
        }
        ensure!(
            matches!(step, Step::RunProbe { .. })
                && flow.page() == PageId::Welcome
                && flow.phase() == Phase::Idle
                && !flow.is_attaching()
                && flow.problem_view().is_none()
                && flow.done_view().is_none()
                && flow.installing_view().is_none(),
            "{name}: after the Apply ended without beginning the flow is on {:?} ({:?}): {step:?}",
            flow.page(),
            flow.phase()
        );
        let notice = flow
            .start_notice()
            .context("no notice on the welcome page")?;
        ensure!(
            notice.title == "The installation did not start"
                && notice.detail.contains("Nothing was written to the disk")
                && notice.detail.contains("go through the pages again")
                && notice.detail.contains("16 minutes old") == recorded
                && (recorded || notice.detail.contains("stopped while it checked")),
            "{name}: the notice: {notice:?}"
        );
        let lines = flow.drain_markers();
        ensure!(
            lines
                .iter()
                .any(|l| l == "ILEX-INSTALLER event=install result=not-started")
                && lines.iter().any(
                    |l| l.starts_with("ILEX-INSTALLER page=welcome edition=gnome")
                        && l.ends_with("notice=not-started")
                )
                && !lines.iter().any(|l| l.contains("event=outcome")
                    || l.contains("page=problem")
                    || l.contains("event=lost")
                    || l.contains("page=done")),
            "{name}: the markers: {lines:?}"
        );
        // The first look at the computer follows, and Continue is the person's.
        let step = settle(&mut flow, &sim, step)?;
        ensure!(
            step == Step::Stay
                && flow.probe().is_some()
                && flow.can_continue()
                && flow.start_notice().is_some(),
            "{name}: the welcome page after the look: {step:?}"
        );
        // The next walk-through installs: the earlier failure under it is not
        // in its way and not its outcome.
        ensure!(
            flow.advance() == Step::Goto(PageId::Machine) && flow.start_notice().is_none(),
            "{name}: Continue does not dismiss the notice"
        );
        ensure!(
            flow.advance() == Step::Goto(PageId::Disk) && through_encryption(&mut flow),
            "{name}: machine, disk and encryption do not continue"
        );
        fill_account(&mut flow, name)?;
        let (generation, request) = run_plan(&mut flow)?;
        ensure!(
            flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review)
                && flow.can_install(),
            "{name}: the check after the refusal"
        );
        let step = start_install(&mut flow, &sim)?;
        let step = settle(&mut flow, &sim, step)?;
        ensure!(
            step == Step::Goto(PageId::Done)
                && flow.page() == PageId::Done
                && flow.phase() == Phase::Succeeded,
            "{name}: the next installation did not finish: {step:?} {:?}",
            flow.page()
        );
    }

    // The installation did begin and ended before the interface looked again:
    // that record is this plan's, and is its outcome.
    let sim = Simulated::load("generic+attach-starting")?;
    let (mut flow, step) = restarted(&sim)?;
    let Step::RunState { generation, .. } = step else {
        bail!("no state request in the window");
    };
    let ended = Simulated::load("generic+attach-failed")?.state()?;
    let step = flow.state_finished(generation, Ok(ended));
    ensure!(
        step == Step::Goto(PageId::Problem)
            && flow.phase() == Phase::Failed
            && flow.problem_view().is_some_and(|p| p.kind
                == ProblemKind::Failed {
                    stage: Stage::Image
                })
            && flow.start_notice().is_none(),
        "an installation that began and failed is not its outcome: {step:?} {:?}",
        flow.page()
    );

    // A helper that stopped answering after this interface asked `Apply`
    // (the problem page for it) and then answers with no installation begun:
    // not the problem page for ever. The interface has its choices, so it is
    // the review page, with the sentence and Try again.
    let (mut flow, sim) = at_review("generic+apply-lost")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply {
        generation,
        plan,
        erase_disk,
        password,
        ..
    } = flow.confirm_install()
    else {
        bail!("no apply");
    };
    let lost = sim.start_installation(&plan, &erase_disk, &password, None);
    ensure!(lost.is_err(), "the simulated Apply answered");
    let mut step = flow.apply_finished(generation, lost);
    for round in 0..8 {
        let Step::RunState { generation, .. } = step else {
            bail!("round {round}: no state request: {step:?}");
        };
        step = flow.state_finished(generation, Err(helper_down()));
    }
    ensure!(
        flow.phase() == Phase::Lost && flow.page() == PageId::Problem,
        "eight silent answers: {:?} {:?}",
        flow.phase(),
        flow.page()
    );
    let Step::RunState { generation, .. } = step else {
        bail!("the lost helper is not asked again: {step:?}");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        step == Step::Goto(PageId::Review)
            && flow.phase() == Phase::Idle
            && flow.page() == PageId::Review
            && flow.problem_view().is_none()
            && flow
                .check_problem()
                .is_some_and(|m| m.title == "The installation did not start"
                    && !m.detail.contains("choices are gone"))
            && flow.can_retry(),
        "a helper that came back with nothing begun: {step:?} {:?} {:?}",
        flow.page(),
        flow.phase()
    );
    Ok(())
}

/// After a lost answer to `Apply`, `State` says another plan's installation is
/// active or being checked: the interface follows it on the installing page and
/// does not send the person back to the review while a disk is written.
fn other_installation() -> Result<()> {
    let lose_apply = |name: &str| -> Result<(Flow, Simulated, Step)> {
        let (mut flow, sim) = at_review(name)?;
        let Step::Confirm(_) = flow.install() else {
            bail!("no dialog");
        };
        let Step::RunApply {
            generation,
            plan,
            erase_disk,
            password,
            ..
        } = flow.confirm_install()
        else {
            bail!("no apply");
        };
        let lost = sim.start_installation(&plan, &erase_disk, &password, None);
        ensure!(lost.is_err(), "{name}: the simulated Apply answered");
        let step = flow.apply_finished(generation, lost);
        ensure!(
            matches!(step, Step::RunState { delay_ms: 400, .. }) && flow.phase() == Phase::Starting,
            "{name}: a lost answer: {step:?}"
        );
        Ok((flow, sim, step))
    };
    // Another plan is active.
    let (mut flow, sim, step) = lose_apply("generic+apply-lost-other")?;
    let Step::RunState { generation, .. } = step else {
        bail!("no state request");
    };
    let state = sim.state()?;
    ensure!(
        state.active && state.applied.as_deref() == Some("fedcba9876543210fedcba9876543210"),
        "the simulated helper is not installing another plan: {state:?}"
    );
    let step = flow.state_finished(generation, Ok(state));
    let view = flow.installing_view().context("no installing view")?;
    ensure!(
        flow.page() == PageId::Installing
            && flow.phase() == Phase::Running
            && matches!(step, Step::RunState { delay_ms: 1000, .. })
            && flow.check_problem().is_none()
            && view.rows.len() == 6
            && view.rows[2].status == Status::Running,
        "another plan is active and the flow is on {:?} ({:?}): {step:?}",
        flow.page(),
        flow.phase()
    );
    // Another plan's `Apply` is checking.
    let (mut flow, _sim, step) = lose_apply("generic+apply-lost")?;
    let Step::RunState { generation, .. } = step else {
        bail!("no state request");
    };
    let mut checking = State::idle(1);
    checking.starting = Some("f".repeat(32));
    let step = flow.state_finished(generation, Ok(checking));
    let view = flow.installing_view().context("no installing view")?;
    ensure!(
        flow.page() == PageId::Installing
            && flow.phase() == Phase::Running
            && matches!(step, Step::RunState { delay_ms: 1000, .. })
            && view.rows.iter().all(|r| r.status == Status::Pending)
            && view.message == "Starting the installation",
        "another plan's Apply is checking and the flow is on {:?} ({:?})",
        flow.page(),
        flow.phase()
    );
    // That `Apply` is then refused: nothing began. The interface has its
    // choices, so it is the review page with the sentence and Try again (not
    // the lost-helper page, and never the previous installation's outcome).
    let Step::RunState { generation, .. } = step else {
        bail!("no state request");
    };
    let mut old = Simulated::load("generic+attach-failed")?.state()?;
    old.refusal = None;
    let step = flow.state_finished(generation, Ok(old));
    ensure!(
        step == Step::Goto(PageId::Review)
            && flow.phase() == Phase::Idle
            && flow
                .check_problem()
                .is_some_and(|m| m.title == "The installation did not start"
                    && !m.detail.contains("choices are gone"))
            && flow.can_retry()
            && flow.problem_view().is_none(),
        "another plan's Apply was refused: {step:?} {:?} {:?}",
        flow.page(),
        flow.phase()
    );
    Ok(())
}

/// A refusal that says an installation is active sends the flow to ask `State`
/// from the review page. Whatever it answers, the page is not left on an
/// endless "Checking your choices" or on a grey button with nothing said.
fn interrupted_attach() -> Result<()> {
    let active = || {
        ClientError::Refused(PlanError::new(
            ErrorCode::InstallationActive,
            None,
            "an installation is running; wait for it to finish",
        ))
    };
    // The look at the computer that Try again makes is refused as active and
    // nothing is: the check is made again (it is not left unrun).
    let (mut flow, sim) = at_review("generic+probe-active")?;
    let Step::RunProbe { generation } = flow.retry() else {
        bail!("Try again does not look at the computer");
    };
    let step = flow.probe_finished(generation, sim.probe());
    ensure!(
        matches!(step, Step::RunState { delay_ms: 0, .. }) && flow.is_attaching(),
        "a look refused as active does not ask where it is: {step:?}"
    );
    let Step::RunState { generation, .. } = step else {
        bail!("no state request");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        matches!(step, Step::RunProbe { .. })
            && flow.is_checking()
            && flow.page() == PageId::Review,
        "an idle helper does not resume the check: {step:?} {:?}",
        flow.plan_check()
    );
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Stay
            && flow.can_install()
            && matches!(flow.review_page().body, ReviewBody::Checked(_)),
        "the check after an idle answer: {step:?} {:?}",
        flow.plan_check()
    );
    // The same, and the helper cannot be asked: the review page says so, with
    // Try again, and Try again asks `State` first and then checks.
    let (mut flow, sim) = at_review("generic+probe-active+state-down")?;
    let Step::RunProbe { generation } = flow.retry() else {
        bail!("Try again does not look at the computer");
    };
    let Step::RunState { generation, .. } = flow.probe_finished(generation, sim.probe()) else {
        bail!("no state request");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Review
            && flow.check_word() == "failed"
            && !flow.is_checking()
            && !flow.can_install()
            && matches!(
                flow.review_page().body,
                ReviewBody::Problem { retry, .. } if retry.enabled
            ),
        "a State that fails leaves the review page with no word: {step:?} {:?}",
        flow.review_page().body
    );
    let step = flow.retry();
    ensure!(
        matches!(step, Step::RunState { delay_ms: 0, .. }) && flow.is_attaching(),
        "Try again after the helper could not be asked does not ask State: {step:?}"
    );
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Stay && flow.can_install(),
        "the check after Try again: {step:?} {:?}",
        flow.plan_check()
    );
    // `Apply` itself refused as active, and the helper cannot be asked: the
    // Erase button is not left grey with nothing said.
    let (mut flow, sim) = at_review("generic+state-down")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { generation, .. } = flow.confirm_install() else {
        bail!("no apply");
    };
    let Step::RunState { generation, .. } = flow.apply_finished(generation, Err(active())) else {
        bail!("an Apply refused as active does not ask where it is");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        step == Step::Stay
            && flow.check_word() == "failed"
            && !flow.can_install()
            && matches!(
                flow.review_page().body,
                ReviewBody::Problem { retry, .. } if retry.enabled
            ),
        "an Apply refused as active and a State that fails: {step:?} {:?}",
        flow.review_page().body
    );
    let retried = flow.retry();
    let step = settle(&mut flow, &sim, retried)?;
    ensure!(
        step == Step::Stay && flow.can_install(),
        "Try again after the refusal: {step:?} {:?}",
        flow.plan_check()
    );
    // `Apply` refused as active, and `State` then answers idle: while it is
    // asked, Erase is not offered on the refused plan, and the idle answer
    // makes the check again rather than leaving the old plan.
    let (mut flow, sim) = at_review("generic")?;
    let Step::Confirm(_) = flow.install() else {
        bail!("no dialog");
    };
    let Step::RunApply { generation, .. } = flow.confirm_install() else {
        bail!("no apply");
    };
    let Step::RunState { generation, .. } = flow.apply_finished(generation, Err(active())) else {
        bail!("an Apply refused as active does not ask where it is");
    };
    ensure!(
        !flow.can_install() && flow.is_attaching(),
        "Erase is offered while State is asked after an active refusal: {:?}",
        flow.plan_check()
    );
    // Nothing else starts beside that `State` either: no Back, no Try again,
    // no Continue, no look at the computer.
    ensure!(
        flow.is_frozen()
            && !flow.can_go_back()
            && !flow.can_retry()
            && !flow.can_continue()
            && flow.retry() == Step::Stay
            && flow.start_probe() == Step::Stay
            && flow.back().is_none()
            && flow.page() == PageId::Review,
        "the review page is not frozen while State is asked: {:?}",
        flow.plan_check()
    );
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        matches!(step, Step::RunProbe { .. })
            && flow.is_checking()
            && flow.page() == PageId::Review,
        "an idle helper after an Apply refused as active does not check again: {step:?} {:?}",
        flow.plan_check()
    );
    let step = settle(&mut flow, &sim, step)?;
    ensure!(
        step == Step::Stay && flow.can_install(),
        "the check after an Apply refused as active: {step:?} {:?}",
        flow.plan_check()
    );
    Ok(())
}

/// `State` could not be asked at start-up: nothing is known of an
/// installation, so the computer is not looked at, and Try again asks `State`
/// again until it has been answered.
fn unreachable_helper() -> Result<()> {
    // A helper that restarts: the first question fails, and an installation
    // that failed before is what the second finds.
    let sim = Simulated::load("generic+state-down+attach-failed")?;
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
    flow.window_shown("renderer=test scale=1");
    flow.drain_markers();
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("no state request");
    };
    let step = flow.state_finished(generation, sim.state());
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Welcome
            && !flow.is_attaching()
            && flow.start_probe_is_idle()
            && flow.blocking_problem().is_some()
            && flow.can_retry_probe()
            && !flow.can_continue(),
        "an unanswered State starts a look at the computer: {step:?}"
    );
    let lines = flow.drain_markers();
    ensure!(
        lines[0] == "ILEX-INSTALLER event=attach result=unavailable"
            && !lines.iter().any(|l| l.contains("event=probe")),
        "the markers: {lines:?}"
    );
    // Try again asks State, not the probe, and the outcome nobody read is shown.
    let step = flow.start_probe();
    let Step::RunState {
        generation,
        delay_ms: 0,
    } = step
    else {
        bail!("Try again does not ask State again: {step:?}");
    };
    ensure!(
        flow.start_probe() == Step::Stay && flow.is_attaching(),
        "a second Try again while State is asked"
    );
    flow.state_finished(generation, sim.state());
    ensure!(
        flow.page() == PageId::Problem
            && flow.phase() == Phase::Failed
            && flow.problem_view().is_some_and(|p| p.kind
                == ProblemKind::Failed {
                    stage: Stage::Image
                }),
        "the outcome the restarted helper recorded is lost: {:?} {:?}",
        flow.page(),
        flow.phase()
    );
    // Once State has been answered, Try again is the probe's again.
    let sim = Simulated::load("generic+state-down")?;
    let mut flow = Flow::new(sample_layouts()?);
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("no state request");
    };
    flow.state_finished(generation, sim.state());
    let Step::RunState { generation, .. } = flow.start_probe() else {
        bail!("Try again does not ask State");
    };
    let step = flow.state_finished(generation, sim.state());
    let Step::RunProbe { generation } = step else {
        bail!("an idle helper does not begin the first look: {step:?}");
    };
    flow.probe_finished(generation, sim.probe());
    ensure!(
        flow.blocking_problem().is_none() && flow.probe().is_some() && flow.can_continue(),
        "the first look after the helper came back"
    );
    let Step::RunProbe { .. } = flow.start_probe() else {
        bail!("Try again is not a look once State was answered");
    };
    Ok(())
}

/// The disk the person accepted stays theirs: a later look at the computer
/// never swaps it silently.
fn chosen_disk() -> Result<()> {
    // Accepting the disk that was preselected is choosing it: a disk that
    // appears in front of it later changes nothing.
    let (mut flow, sim) = flow_on("generic")?;
    flow.advance();
    flow.advance();
    ensure!(
        flow.page() == PageId::Disk && flow.disk() == Some("nvme-eui.0025385c1140a2b3"),
        "the preselected disk"
    );
    ensure!(
        flow.advance() == Step::Goto(PageId::Encryption),
        "the disk page does not continue"
    );
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no look");
    };
    let mut probe = sim.probe()?;
    let mut newcomer = probe.disks[0].clone();
    newcomer.id = "nvme-eui.0000000000000001".into();
    newcomer.path = "/dev/nvme1n1".into();
    probe.disks.insert(0, newcomer);
    flow.probe_finished(generation, Ok(probe));
    ensure!(
        flow.disk() == Some("nvme-eui.0025385c1140a2b3") && flow.disk_notice().is_none(),
        "a disk that appeared took the accepted one's place: {:?}",
        flow.disk()
    );

    // The accepted disk is unplugged after the check: Try again does not plan
    // another one; the disk page says so, and the person chooses.
    for (chosen, instead) in [
        (None, "ata-WDC_WDS500G2B0A_214512800345"),
        (
            Some("ata-WDC_WDS500G2B0A_214512800345"),
            "nvme-eui.0025385c1140a2b3",
        ),
    ] {
        let (mut flow, sim) = flow_on("generic+vanish")?;
        flow.advance();
        flow.advance();
        if let Some(id) = chosen {
            ensure!(flow.set_disk(id), "choosing {id}");
        }
        through_encryption(&mut flow);
        fill_account(&mut flow, "generic")?;
        let (generation, request) = run_plan(&mut flow)?;
        flow.plan_finished(generation, sim.plan(&request));
        ensure!(flow.can_install(), "the review page cannot install");
        let planned = flow.disk().map(str::to_owned);
        let mut flow = std::mem::replace(&mut flow, Flow::new(sample_layouts()?))
            .with_markers(config(true, false));
        flow.window_shown("renderer=test scale=1");
        flow.drain_markers();
        let step = flow.retry();
        let Step::RunProbe { generation } = step else {
            bail!("Try again does not look first: {step:?}");
        };
        let step = flow.probe_finished(generation, sim.probe());
        let notice = flow.disk_notice();
        ensure!(
            step == Step::Goto(PageId::Disk)
                && flow.page() == PageId::Disk
                && !flow.is_checking()
                && matches!(flow.plan_check(), PlanCheck::NotRun)
                && flow.disk() == Some(instead)
                && flow.disk() != planned.as_deref()
                && notice
                    .as_ref()
                    .is_some_and(|n| n.title == "The disk you chose is no longer available"
                        && n.detail.contains("Choose a disk")),
            "the chosen disk {planned:?} vanished: {step:?} on {:?}, disk {:?}, {notice:?}",
            flow.page(),
            flow.disk()
        );
        let lines = flow.drain_markers();
        ensure!(
            lines.iter().filter(|l| l.contains("page=disk")).count() == 1
                && lines.iter().any(|l| l
                    == &format!(
                        "ILEX-INSTALLER event=disk-gone disk={}",
                        planned.as_deref().unwrap_or("")
                    )),
            "the markers: {lines:?}"
        );
        // A toolkit that echoes the selected row back changes nothing and does
        // not clear it; nothing is planned until the person goes on.
        ensure!(
            flow.set_disk(instead) && flow.disk_notice().is_some(),
            "an echoed selection cleared the notice"
        );
        ensure!(
            flow.can_continue() && flow.advance() == Step::Goto(PageId::Encryption),
            "the disk page does not go on"
        );
        ensure!(flow.disk_notice().is_none(), "the notice stays");
    }
    // A chosen disk that goes while the person is still on the disk page:
    // the notice, no page change.
    let (mut flow, sim) = flow_on("generic+vanish")?;
    flow.advance();
    flow.advance();
    ensure!(
        flow.set_disk("ata-WDC_WDS500G2B0A_214512800345"),
        "choosing the second disk"
    );
    let probe = sim.probe()?;
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("no look");
    };
    let mut probe = probe;
    probe
        .disks
        .retain(|d| d.id != "ata-WDC_WDS500G2B0A_214512800345");
    let step = flow.probe_finished(generation, Ok(probe));
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Disk
            && flow.disk_notice().is_some()
            && flow.disk() == Some("nvme-eui.0025385c1140a2b3"),
        "the disk page after its disk vanished: {step:?}"
    );
    Ok(())
}

/// The passwords the flow holds are wiped when they have done their work, and
/// the interface is told (it clears the fields it holds them in).
fn wiped_secrets() -> Result<()> {
    let (mut flow, sim) = at_review("generic")?;
    ensure!(
        flow.secrets_wiped() == 0 && !flow.password().is_empty(),
        "nothing wiped yet"
    );
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    ensure!(
        flow.page() == PageId::Done
            && flow.secrets_wiped() == 1
            && flow.password().is_empty()
            && flow.confirmation().is_empty(),
        "the password after the installation"
    );
    // A failure keeps them for Try again; leaving from the problem page does
    // not.
    let (mut flow, sim) = at_review("generic+fail-image")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    ensure!(
        flow.page() == PageId::Problem && flow.secrets_wiped() == 0 && !flow.password().is_empty(),
        "a failure keeps the password for Try again"
    );
    let Step::RunFinish { generation, .. } = flow.restart() else {
        bail!("no finish");
    };
    flow.finish_finished(generation, sim.finish(&FinishAction::Restart));
    ensure!(
        flow.secrets_wiped() == 1 && flow.password().is_empty() && flow.confirmation().is_empty(),
        "Restart from the problem page leaves the password in the flow"
    );
    // A refused Restart does not wipe: the person is still here.
    let (mut flow, sim) = at_review("generic+fail-image+finish-fails")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    let Step::RunFinish { generation, .. } = flow.restart() else {
        bail!("no finish");
    };
    flow.finish_finished(generation, sim.finish(&FinishAction::Restart));
    ensure!(
        flow.secrets_wiped() == 0 && !flow.password().is_empty(),
        "a Restart that failed wiped the password"
    );
    Ok(())
}

/// What the views say, so that an interface only draws: the buttons' state, the
/// words of a wait, a silent helper and a note.
fn page_views() -> Result<()> {
    // The stack a navigation view holds for each page.
    ensure!(
        PageId::Review.stack() == PageId::ORDER
            && PageId::Machine.stack() == [PageId::Welcome, PageId::Machine]
            && PageId::ALL
                .iter()
                .filter(|p| p.is_closing())
                .all(|p| p.stack() == [*p]),
        "the navigation stacks"
    );
    // The review page.
    let (flow, _) = at_review("generic")?;
    let page = flow.review_page();
    ensure!(
        page.button == Control::shown(true)
            && matches!(&page.body, ReviewBody::Checked(review)
                if review.notes.len() == review.systems.len() + review.warnings.len()
                    && review.notes.iter().take(review.systems.len()).all(|n| n.title == "Found on the disk")
                    && review.notes.iter().skip(review.systems.len()).all(|n| n.title == "Note")
                    && review.systems.len() == 1),
        "the review page: {page:?}"
    );
    let (mut flow, _) = flow_on("generic")?;
    flow.advance();
    flow.advance();
    through_encryption(&mut flow);
    fill_account(&mut flow, "generic")?;
    flow.advance();
    let page = flow.review_page();
    ensure!(
        page.button == Control::HIDDEN
            && matches!(&page.body, ReviewBody::Waiting(m) if m.title == "Checking your choices"),
        "the review page while the helper checks: {page:?}"
    );
    let (mut flow, sim) = flow_on("registry-down")?;
    flow.advance();
    flow.advance();
    through_encryption(&mut flow);
    fill_account(&mut flow, "registry-down")?;
    let (generation, request) = run_plan(&mut flow)?;
    flow.plan_finished(generation, sim.plan(&request));
    let page = flow.review_page();
    ensure!(
        page.button == Control::HIDDEN
            && matches!(&page.body, ReviewBody::Problem { icon: ProblemIcon::Error, retry, .. }
                if *retry == Control::shown(true)),
        "the review page after a failed check: {page:?}"
    );
    // The disk page's lines say why a disk is not offered.
    let (flow, _) = flow_on("generic")?;
    let view = flow.disk_view().context("no disk view")?;
    let away = &view.choices[2];
    ensure!(
        away.lines()
            .last()
            .is_some_and(|l| l.starts_with("Not available: it is the medium") && l.ends_with('.'))
            && view.choices[0]
                .lines()
                .iter()
                .all(|l| !l.starts_with("Not available")),
        "the disk rows' lines: {:?}",
        away.lines()
    );
    // A helper that stops answering: the banner's own sentence, said once.
    let (mut flow, sim) = at_review("generic+lost")?;
    let mut step = start_install(&mut flow, &sim)?;
    while flow.installing_view().is_none_or(|v| v.waiting.is_none()) {
        let Step::RunState { generation, .. } = step else {
            bail!("the helper never goes quiet: {step:?}");
        };
        step = flow.state_finished(generation, sim.state());
    }
    let waiting = flow
        .installing_view()
        .and_then(|v| v.waiting)
        .context("no waiting message")?;
    ensure!(
        waiting.title == "The installer's helper is not answering"
            && !waiting.detail.contains("is not running")
            && !waiting.detail.contains("not answering"),
        "the silent helper: {waiting:?}"
    );
    // The problem page's buttons stay where they are while a Restart or the
    // log is under way: disabled, not hidden.
    let (mut flow, sim) = at_review("generic+fail-image+sticks")?;
    let step = start_install(&mut flow, &sim)?;
    settle(&mut flow, &sim, step)?;
    let Step::RunFinish { generation, .. } = flow.restart() else {
        bail!("no finish");
    };
    ensure!(
        flow.problem_view()
            .is_some_and(|p| p.retry == Control::shown(false)
                && p.save_log == Control::shown(false)
                && p.restart == Control::shown(false)),
        "the problem page's buttons while Restart is asked: {:?}",
        flow.problem_view()
    );
    flow.finish_finished(generation, sim.finish(&FinishAction::Restart));
    ensure!(
        flow.problem_view()
            .is_some_and(|p| p.finishing.is_some() && p.retry == Control::shown(false)),
        "the problem page after Restart was accepted"
    );
    Ok(())
}

/// A flow on the review page of an encrypted installation of `name`, with
/// markers queued.
fn encrypted_review(name: &str, chip: Option<bool>) -> Result<(Flow, Simulated)> {
    let sim = Simulated::load(name)?;
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
    let Step::RunProbe { generation } = flow.start_probe() else {
        bail!("a new flow does not ask for the probe");
    };
    flow.probe_finished(generation, sim.probe());
    flow.window_shown("renderer=none");
    ensure!(
        flow.advance() == Step::Goto(PageId::Machine)
            && flow.advance() == Step::Goto(PageId::Disk)
            && flow.advance() == Step::Goto(PageId::Encryption),
        "{name}: to the encryption page"
    );
    if let Some(on) = chip {
        ensure!(flow.set_chip(on), "{name}: the chip switch is refused");
    }
    ensure!(
        flow.set_passphrase(Secret::new("correct horse battery"))
            && flow.set_passphrase_confirmation(Secret::new("correct horse battery"))
            && flow.advance() == Step::Goto(PageId::Account),
        "{name}: past the encryption page"
    );
    fill_account(&mut flow, name)?;
    let (generation, request) = run_plan(&mut flow)?;
    ensure!(
        flow.plan_finished(generation, sim.plan(&request)) == Step::Goto(PageId::Review),
        "{name}: the review page"
    );
    Ok((flow, sim))
}

/// Carry steps out until `done` holds (at most eighty steps).
fn drive(
    flow: &mut Flow,
    sim: &Simulated,
    mut step: Step,
    done: impl Fn(&Flow) -> bool,
) -> Result<Step> {
    for _ in 0..80 {
        if done(flow) {
            return Ok(step);
        }
        step = carry_out(flow, sim, step)?;
    }
    bail!("the flow never got there: it is on {:?}", flow.page())
}

/// A flow that was started the way a restarted interface is.
fn restarted_on(name: &str) -> Result<(Flow, Simulated, Step)> {
    let sim = Simulated::load(name)?;
    let mut flow = Flow::new(sample_layouts()?).with_markers(config(true, false));
    flow.window_shown("renderer=none");
    let Step::RunState { generation, .. } = flow.start() else {
        bail!("a new flow does not ask where the installation is");
    };
    let step = flow.state_finished(generation, sim.state());
    Ok((flow, sim, step))
}

/// The security chip's row, the recovery key page and what the done page says
/// of the unlocking: every state, and that the key is nowhere it must not be.
fn recovery_key() -> Result<()> {
    use ilex_installer_model::recovery;
    use ilex_installer_model::selfcheck::TEST_KEY;
    let key_groups: Vec<String> = recovery::groups(TEST_KEY)
        .into_iter()
        .map(str::to_owned)
        .collect();

    // The row: nothing to turn on where the computer cannot, a switch where it
    // can, and the request follows it.
    let (mut flow, _) = flow_on("generic")?;
    flow.advance();
    flow.advance();
    flow.advance();
    let row = flow.chip_view();
    ensure!(
        !row.sensitive
            && !row.on
            && row.line.starts_with("Not available: Secure Boot is off")
            && !flow.set_chip(true)
            && flow.marker_fields().ends_with("chip=unavailable"),
        "the row of a computer that cannot: {row:?}"
    );
    flow.set_encrypt(false);
    ensure!(
        flow.chip_view().line == page::CHIP_NEEDS_ENCRYPTION
            && flow.marker_fields().ends_with("chip=none"),
        "the row without encryption"
    );
    let (mut flow, _) = flow_on("generic+chip")?;
    flow.advance();
    flow.advance();
    flow.advance();
    ensure!(
        flow.chip_view().sensitive
            && flow.chip_view().on
            && flow.chip_view().line == page::CHIP_ON_LINE
            && flow.marker_fields().ends_with("chip=on")
            && flow.set_chip(false)
            && !flow.chip_view().on
            && flow.marker_fields().ends_with("chip=off")
            && flow.set_chip(true),
        "the row of a computer that can: {:?}",
        flow.chip_view()
    );

    // The request: a recovery key whenever encrypted, the chip unless it was
    // turned off, and the review says which.
    let (flow, _) = encrypted_review("generic+recovery+chip", None)?;
    let review = flow.review().context("no review")?;
    ensure!(
        review
            .choices
            .iter()
            .any(|row| row.title == "Automatic unlock"
                && row
                    .value
                    .starts_with("On: the security chip unlocks the disk")),
        "the review's rows: {:?}",
        review.choices
    );
    let (flow, _) = encrypted_review("generic+recovery+chip", Some(false))?;
    let review = flow.review().context("no review")?;
    ensure!(
        review
            .choices
            .iter()
            .any(|row| row.title == "Automatic unlock"
                && row.value.contains("automatic unlock was turned off")),
        "the review of a chip turned off: {:?}",
        review.choices
    );
    let (flow, _) = encrypted_review("generic+recovery", None)?;
    let review = flow.review().context("no review")?;
    ensure!(
        review
            .choices
            .iter()
            .any(|row| row.title == "Automatic unlock" && row.value.contains("Secure Boot is off")),
        "the review of a computer that cannot: {:?}",
        review.choices
    );

    // The whole way: the key is taken, shown, wrongly confirmed, rightly
    // confirmed, and the installation ends with the chip enrolled.
    let (mut flow, sim) = encrypted_review("generic+recovery+chip", None)?;
    let step = start_install(&mut flow, &sim)?;
    let step = drive(&mut flow, &sim, step, |f| f.page() == PageId::Recovery)?;
    let Step::RunTakeKey { .. } = &step else {
        bail!("the recovery page without the key being asked for: {step:?}");
    };
    ensure!(
        flow.recovery_view().is_some_and(|v| v.groups.is_none()
            && v.no_key
                .as_ref()
                .is_some_and(|m| m.title == page::WAITING_KEY)),
        "the key is being fetched: {:?}",
        flow.recovery_view()
    );
    let poll = carry_out(&mut flow, &sim, step)?;
    ensure!(
        matches!(poll, Step::RunState { .. }),
        "the poll does not go on after the key: {poll:?}"
    );
    let view = flow.recovery_view().context("no recovery page")?;
    ensure!(
        view.groups.as_ref() == Some(&key_groups)
            && view.warning == Some(page::WRITE_IT_DOWN)
            && view.no_key.is_none()
            // The key is on this screen: there is nothing to give up on.
            && view.skip.is_none()
            && view.typed.label == page::KEY_LABEL
            && view.typed.hint == page::KEY_HINT
            && view.confirm == Control::shown(false)
            && flow.marker_fields() == "key=shown skip=false reattached=false",
        "the key page: {view:?}"
    );
    // The key's shape is checked before anything is sent: a part of it is not
    // refused (it is not finished), a letter no key has is, and two right
    // groups are not a key.
    flow.set_recovery_key(Secret::new("cdefghij-klnr"));
    let view = flow.recovery_view().context("no view")?;
    ensure!(
        view.typed.check == Check::Empty
            && view.typed.hint == "12 of 64 letters: 1 of 8 groups are complete."
            && flow.confirm_recovery() == Step::Stay,
        "a part of the key: {view:?}"
    );
    flow.set_recovery_key(Secret::new(&TEST_KEY.replacen('v', "w", 6)));
    let view = flow.recovery_view().context("no view")?;
    ensure!(
        view.typed
            .check
            .message()
            .is_some_and(|m| m.title == "A recovery key has no such letter"
                && m.detail.starts_with("Group 2 has a letter no key has"))
            && flow.confirm_recovery() == Step::Stay,
        "wrong letters: {view:?}"
    );
    flow.set_recovery_key(Secret::new(&format!("{TEST_KEY}-cdefghij")));
    ensure!(
        flow.recovery_view()
            .and_then(|v| v.typed.check.message().map(|m| m.title.clone()))
            .as_deref()
            == Some("That is more than a recovery key"),
        "too much"
    );
    flow.set_recovery_key(Secret::new(&format!("{} {}", key_groups[1], key_groups[5])));
    ensure!(
        flow.confirm_recovery() == Step::Stay,
        "two right groups of eight are not a key"
    );
    // The right shape, the wrong key (the last letter of the last group): the
    // helper refuses, the field empties.
    flow.set_recovery_key(Secret::new(&format!(
        "{}u",
        &TEST_KEY[..TEST_KEY.len() - 1]
    )));
    let Step::RunAcknowledge {
        generation,
        plan,
        answer,
    } = flow.confirm_recovery()
    else {
        bail!("the right shape was not sent");
    };
    ensure!(
        flow.recovery_view().is_some_and(
            |v| v.confirm_label == page::CONFIRMING_LABEL && v.confirm == Control::shown(false)
        ) && !flow.set_recovery_key(Secret::new("x")),
        "while the helper compares"
    );
    let wiped = flow.secrets_wiped();
    let step = flow.confirm_finished(generation, sim.acknowledge_recovery_key(&plan, &answer));
    ensure!(
        step == Step::Stay
            && flow.page() == PageId::Recovery
            && flow.secrets_wiped() == wiped + 1
            && flow.recovery_typed().is_empty()
            && flow.recovery_view().is_some_and(|v| v
                .problem
                .as_ref()
                .is_some_and(|m| m.title == "That is not the recovery key")
                && v.groups.is_some()
                // A wrong key is the person's to retype, not the volume's:
                // no way out is offered while the key is on screen.
                && v.skip.is_none()),
        "a wrong answer: {:?}",
        flow.recovery_view()
    );
    // Typing again clears the refusal; the right key (with the paper's spaces and
    // capitals) ends the page, and the helper is given it as the paper has it.
    flow.set_recovery_key(Secret::new(&TEST_KEY.to_uppercase().replace('-', " ")));
    ensure!(
        flow.recovery_view().is_some_and(|v| v.problem.is_none()),
        "typing does not clear the refusal"
    );
    let Step::RunAcknowledge {
        generation,
        plan,
        answer,
    } = flow.confirm_recovery()
    else {
        bail!("the right key was not sent");
    };
    ensure!(
        answer.expose() == TEST_KEY,
        "the key is not sent as the paper has it"
    );
    let step = flow.confirm_finished(generation, sim.acknowledge_recovery_key(&plan, &answer));
    ensure!(
        step == Step::Goto(PageId::Installing)
            && flow.page() == PageId::Installing
            && flow.recovery_view().is_none()
            && flow.recovery_typed().is_empty(),
        "the confirmed key: {step:?}"
    );
    // The key is handed over once.
    ensure!(
        matches!(
            sim.take_recovery_key(&plan),
            Err(ClientError::Refused(e)) if e.code == ErrorCode::RecoveryKeyTaken
        ),
        "the key was handed over twice"
    );
    // The poll goes on to the end; the chip is enrolled.
    drive(&mut flow, &sim, poll, |f| f.page() == PageId::Done)?;
    let done = flow.done_view().context("no done page")?;
    ensure!(
        done.detail.contains("The disk unlocks automatically")
            && done
                .rows
                .iter()
                .any(|r| r.title == "Disk unlock"
                    && r.value == "Automatically, with the security chip"),
        "the done page of an enrolled chip: {done:?}"
    );
    // Nothing the flow printed carries a group of the key.
    let markers = flow.drain_markers().join("\n");
    ensure!(
        key_groups.iter().all(|g| !markers.contains(g)) && !markers.contains(TEST_KEY),
        "the key is in the markers"
    );

    // A passphrase-only ending says why. (No key is needed to say it: the
    // installation ends the same way without one.)
    let (mut flow, sim) = encrypted_review("generic+recovery", None)?;
    let step = start_install(&mut flow, &sim)?;
    let step = drive(&mut flow, &sim, step, |f| f.page() == PageId::Recovery)?;
    let poll = carry_out(&mut flow, &sim, step)?;
    flow.set_recovery_key(Secret::new(TEST_KEY));
    let Step::RunAcknowledge {
        generation,
        plan,
        answer,
    } = flow.confirm_recovery()
    else {
        bail!("the key was not sent");
    };
    flow.confirm_finished(generation, sim.acknowledge_recovery_key(&plan, &answer));
    drive(&mut flow, &sim, poll, |f| f.page() == PageId::Done)?;
    let done = flow.done_view().context("no done page")?;
    ensure!(
        done.detail.contains(
            "The computer asks for the passphrase at every start: Secure Boot is off, or in setup mode, on this computer."
        ) && done.rows.iter().any(|r| r.title == "Disk unlock"
            && r.value
                == "Passphrase at every start: Secure Boot is off, or in setup mode, on this computer"),
        "the done page of a passphrase-only ending: {done:?}"
    );

    // A restarted interface. The key waits: it takes it.
    let (flow, sim, step) = restarted_on("generic+attach-key+chip")?;
    ensure!(
        flow.page() == PageId::Recovery && matches!(step, Step::RunTakeKey { .. }),
        "a restarted interface does not take the waiting key: {step:?}"
    );
    let mut flow = flow;
    let poll = carry_out(&mut flow, &sim, step)?;
    ensure!(
        matches!(poll, Step::RunState { .. })
            && flow
                .recovery_view()
                .is_some_and(|v| v.groups.as_ref() == Some(&key_groups))
            && flow.marker_fields().ends_with("reattached=true"),
        "the key of a restarted interface"
    );
    // The key was taken by an interface that is gone: it is not shown again,
    // and the whole key from the paper confirms it all the same.
    let (mut flow, sim, step) = restarted_on("generic+attach-key-shown+chip")?;
    ensure!(
        flow.page() == PageId::Recovery && matches!(step, Step::RunState { .. }),
        "a restarted interface does not go to the recovery page: {step:?} on {:?}",
        flow.page()
    );
    let view = flow.recovery_view().context("no recovery page")?;
    ensure!(
        view.groups.is_none()
            && view.warning.is_none()
            && view
                .no_key
                .as_ref()
                .is_some_and(|m| m.title == page::GONE_TITLE && m.detail == page::GONE_DETAIL)
            // Nothing to show and nothing to type from: the way out is offered,
            // with what the disk is then and how to get a key later.
            && view.skip.as_ref().is_some_and(|s| s.title == page::SKIP_TITLE
                && s.detail.contains("passphrase only")
                && s.detail.contains("ilex-enroll complete --root / --reissue-recovery")
                && s.detail.contains("Ctrl+Alt+F8")
                && s.button == Control::shown(true))
            && flow.marker_fields() == "key=gone skip=true reattached=true",
        "the page of a key that cannot be shown: {view:?}"
    );
    ensure!(
        matches!(
            sim.take_recovery_key(&"x".repeat(32)),
            Err(ClientError::Refused(_))
        ),
        "the simulated helper hands a key to another plan"
    );
    flow.set_recovery_key(Secret::new(&TEST_KEY.replace('-', "")));
    let Step::RunAcknowledge {
        generation,
        plan,
        answer,
    } = flow.confirm_recovery()
    else {
        bail!("the key was not sent");
    };
    ensure!(
        flow.confirm_finished(generation, sim.acknowledge_recovery_key(&plan, &answer))
            == Step::Goto(PageId::Installing),
        "the key of a key shown before was not accepted"
    );
    drive(&mut flow, &sim, step, |f| f.page() == PageId::Done)?;
    ensure!(
        flow.done_view()
            .is_some_and(|d| d.detail.contains("The disk unlocks automatically")),
        "the done page after a restart"
    );
    // Confirmed already: the restarted interface is on the installing page, and
    // the key is not asked for again.
    let (flow, _, step) = restarted_on("generic+attach-key-acked+chip")?;
    ensure!(
        flow.page() == PageId::Installing
            && matches!(step, Step::RunState { .. })
            && flow.recovery_view().is_none(),
        "a restarted interface after the confirmation: on {:?}",
        flow.page()
    );

    // Going on without a recovery key. (1) The key was shown to an interface that
    // is gone and the paper is lost: the page offers it, the helper is told, the
    // installation ends passphrase-only and the done page says how to get a key
    // later.
    let (mut flow, sim, step) = restarted_on("generic+attach-key-shown+chip")?;
    ensure!(
        flow.recovery_view().is_some_and(|v| v.skip.is_some()),
        "a key that cannot be shown does not offer going on without it"
    );
    let skipped = skip_and_finish(&mut flow, &sim, step)?;
    ensure!(
        flow.page() == PageId::Done && flow.recovery_view().is_none(),
        "going on without a key does not end on the done page: {skipped:?}"
    );
    let done = flow.done_view().context("no done page")?;
    ensure!(
        done.detail.contains("The computer asks for the passphrase at every start: no recovery key was made and confirmed.")
            && done.detail.contains("The passphrase opens the disk, in the keyboard layout you chose.")
            && done
                .detail
                .contains("sudo ilex-enroll complete --root / --reissue-recovery")
            && done.detail.contains("Ctrl+Alt+F8")
            && done.rows.iter().any(|r| r.title == "Disk unlock"
                && r.value == "Passphrase at every start: no recovery key was made and confirmed"),
        "the done page after going on without a key: {done:?}"
    );
    let markers = flow.drain_markers().join("\n");
    ensure!(
        markers.contains("event=recovery state=declining")
            && markers.contains("event=recovery state=declined")
            && !markers.contains(TEST_KEY),
        "the markers of going on without a key: {markers}"
    );
    // (2) The helper cannot hand the key over: three tries, then the way out.
    let (mut flow, sim) = encrypted_review("generic+recovery+chip+take-fails", None)?;
    let step = start_install(&mut flow, &sim)?;
    let step = drive(&mut flow, &sim, step, |f| {
        f.recovery_view().is_some_and(|v| v.skip.is_some())
    })?;
    let view = flow.recovery_view().context("no recovery page")?;
    ensure!(
        view.groups.is_none()
            && view
                .no_key
                .as_ref()
                .is_some_and(|m| m.title == "The recovery key could not be fetched")
            && flow.marker_fields().starts_with("key=gone skip=true"),
        "three failed takes: {view:?}"
    );
    skip_and_finish(&mut flow, &sim, step)?;
    ensure!(
        flow.done_view()
            .is_some_and(|d| d.detail.contains("no recovery key was made and confirmed")),
        "the done page after failed takes"
    );
    // (3) The volume refuses the right key: not a typing mistake, so the way out
    // is offered with the key still on the screen, and the key is kept typed.
    let (mut flow, sim) = encrypted_review("generic+recovery+chip+slot-refuses", None)?;
    let step = start_install(&mut flow, &sim)?;
    let step = drive(&mut flow, &sim, step, |f| f.page() == PageId::Recovery)?;
    let poll = carry_out(&mut flow, &sim, step)?;
    ensure!(
        flow.recovery_view().is_some_and(|v| v.skip.is_none()),
        "going on without a key is offered before anything went wrong"
    );
    flow.set_recovery_key(Secret::new(TEST_KEY));
    let Step::RunAcknowledge {
        generation,
        plan,
        answer,
    } = flow.confirm_recovery()
    else {
        bail!("the key was not sent");
    };
    let step = flow.confirm_finished(generation, sim.acknowledge_recovery_key(&plan, &answer));
    let view = flow.recovery_view().context("no recovery page")?;
    ensure!(
        step == Step::Stay
            && view.groups.is_some()
            && view
                .problem
                .as_ref()
                .is_some_and(|m| m.title == "The recovery key could not be confirmed"
                    && m.detail.contains("does not open its keyslot"))
            && view.skip.is_some()
            && !flow.recovery_typed().is_empty(),
        "a key its keyslot refused: {view:?}"
    );
    skip_and_finish(&mut flow, &sim, poll)?;
    // (4) The helper does not answer: the page says so and the button is back.
    let (mut flow, sim, _) = restarted_on("generic+attach-key-shown+chip+skip-fails")?;
    let step = flow.continue_without_recovery_key();
    let Step::RunContinueWithout { generation, plan } = step else {
        bail!("going on without a key was not sent: {step:?}");
    };
    ensure!(
        flow.recovery_view()
            .is_some_and(|v| v.skip.as_ref().is_some_and(|s| s.skipping
                && s.label == page::SKIPPING_LABEL
                && s.button == Control::shown(false)))
            && flow.continue_without_recovery_key() == Step::Stay,
        "the page while the helper is asked"
    );
    flow.skip_finished(generation, sim.continue_without_recovery_key(&plan));
    ensure!(
        flow.page() == PageId::Recovery
            && flow
                .recovery_view()
                .is_some_and(|v| v.skip.as_ref().is_some_and(|s| !s.skipping
                    && s.button == Control::shown(true)
                    && s.problem
                        .as_ref()
                        .is_some_and(|m| m.title
                            == "The installation could not go on without a recovery key"))),
        "an unanswered request is not said: {:?}",
        flow.recovery_view()
    );
    // Nothing to give up once the key is confirmed.
    let (mut flow, _, _) = restarted_on("generic+attach-key-acked+chip")?;
    ensure!(
        flow.continue_without_recovery_key() == Step::Stay,
        "a confirmed key was given up"
    );
    Ok(())
}

/// Press "Continue without a recovery key", carry the request out and drive the
/// installation to its done page.
fn skip_and_finish(flow: &mut Flow, sim: &Simulated, poll: Step) -> Result<Step> {
    let step = flow.continue_without_recovery_key();
    ensure!(
        matches!(step, Step::RunContinueWithout { .. }),
        "going on without a key was not sent: {step:?}"
    );
    let wiped = flow.secrets_wiped();
    let after = carry_out(flow, sim, step)?;
    ensure!(
        after == Step::Goto(PageId::Installing)
            && flow.page() == PageId::Installing
            && flow.secrets_wiped() == wiped + 1
            && flow.recovery_view().is_none()
            && flow.recovery_typed().is_empty(),
        "the page after going on without a key: {after:?}"
    );
    drive(flow, sim, poll, |f| f.page() == PageId::Done)
}
