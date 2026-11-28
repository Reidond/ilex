//! `ilex-installer --self-check --render DIR` for the KDE edition: every page
//! and every error state drawn to a PNG file from the simulation's fixtures,
//! the same states the GTK renderer draws (`ilex_installer_flow::shots::SHOTS`),
//! so that a trial can compare the two interfaces' counts. The window is the
//! installer's own (`Main.qml` with `rendering` on); a driver thread plays the
//! person: it sends the events the shot's steps mean, asks the window to type
//! into a field or go back (the only things that are the window's, not the
//! flow's), waits until the flow says the page and state the shot names are
//! there and the window really shows that page, and has the window grab itself
//! (`QQuickWindow::grabWindow`, at the surface's own scale).
//!
//! Files are `<shot>-<light|dark>-<scale>x.png`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use cxx_qt::CxxQtThread;
use ilex_installer_flow::backend::Backend;
use ilex_installer_flow::events::{Event, Observed};
use ilex_installer_flow::flow::PageId;
use ilex_installer_flow::keyboard::{Layout, Layouts, SessionKeyboard};
use ilex_installer_flow::language;
use ilex_installer_flow::runner::Services;
use ilex_installer_flow::shots::{Do, PASSPHRASE, SHOTS};
use ilex_installer_flow::simulate::Simulated;

use super::installer::qobject::Installer;

/// How often the application's state is looked at while a shot waits for it.
const POLL: Duration = Duration::from_millis(20);
/// How long a shot waits for the page and state it names before it fails.
const DEADLINE: Duration = Duration::from_secs(30);
/// Time given to the window to lay out and draw a page it already shows.
const SETTLE: Duration = Duration::from_millis(250);

/// What the window thread and the driver thread share.
pub struct Shared {
    observed: Mutex<Observed>,
    thread: Mutex<Option<CxxQtThread<Installer>>>,
    /// The window's answers, by kind, with a counter that says a new one came.
    reports: Mutex<BTreeMap<String, (u64, String)>>,
    changed: Condvar,
    scale: Mutex<f64>,
    /// From now on the compositor refuses a layout switch.
    failing: Arc<AtomicBool>,
}

impl Shared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            observed: Mutex::new(Observed::initial()),
            thread: Mutex::new(None),
            reports: Mutex::new(BTreeMap::new()),
            changed: Condvar::new(),
            scale: Mutex::new(1.0),
            failing: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn attach(&self, thread: CxxQtThread<Installer>) {
        *lock(&self.thread) = Some(thread);
        self.changed.notify_all();
    }

    pub fn observe(&self, observed: Observed) {
        *lock(&self.observed) = observed;
    }

    pub fn set_scale(&self, scale: f64) {
        *lock(&self.scale) = scale;
    }

    /// The window answered a request.
    pub fn report(&self, kind: &str, value: &str) {
        let mut reports = lock(&self.reports);
        let count = reports.get(kind).map_or(0, |(n, _)| *n) + 1;
        reports.insert(kind.to_owned(), (count, value.to_owned()));
        self.changed.notify_all();
    }

    fn current(&self, kind: &str) -> (u64, String) {
        lock(&self.reports)
            .get(kind)
            .cloned()
            .unwrap_or((0, String::new()))
    }

    /// Wait for a report of `kind` newer than `after`.
    fn wait_for(&self, kind: &str, after: u64) -> Result<String> {
        let started = Instant::now();
        let mut reports = lock(&self.reports);
        loop {
            if let Some((n, value)) = reports.get(kind)
                && *n > after
            {
                return Ok(value.clone());
            }
            if started.elapsed() > DEADLINE {
                bail!("the window did not report {kind} within {DEADLINE:?}");
            }
            reports = self
                .changed
                .wait_timeout(reports, POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Run `f` on the Qt thread, in order with everything posted before it.
    fn post(
        &self,
        f: impl FnOnce(std::pin::Pin<&mut Installer>) + Send + 'static,
    ) -> Result<()> {
        let thread = lock(&self.thread)
            .clone()
            .context("the window has not started")?;
        thread
            .queue(f)
            .map_err(|e| anyhow::anyhow!("the window is gone: {e:?}"))
    }

    /// Everything posted so far has been handled.
    fn flush(&self, what: &str) -> Result<()> {
        let (before, _) = self.current("flush");
        self.post(|installer| installer.ask_window("flush", "", ""))?;
        self.wait_for("flush", before)
            .with_context(|| format!("{what}: flushing"))?;
        Ok(())
    }

    fn observed(&self) -> Observed {
        lock(&self.observed).clone()
    }

    /// Wait until nothing is under way (`want` says what else must hold), the
    /// window shows the flow's page, and that is `page` when given.
    fn until(
        &self,
        what: &str,
        page: Option<PageId>,
        want: impl Fn(&Observed) -> bool,
    ) -> Result<()> {
        let started = Instant::now();
        loop {
            let seen = self.observed();
            let shown = self.current("page").1;
            // The window shows the page the flow is on (its page row has
            // followed), whatever page that is.
            if seen.settled()
                && want(&seen)
                && shown == seen.page.id()
                && page.is_none_or(|p| seen.page == p)
            {
                return Ok(());
            }
            if started.elapsed() > DEADLINE {
                bail!(
                    "{what}: after {DEADLINE:?} the application reports {seen:?} and shows {shown:?}, expected {}",
                    page.map_or("any page".to_owned(), |p| p.id().to_owned())
                );
            }
            std::thread::sleep(POLL);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// A session whose layout switches work until a shot says they do not.
struct RenderKeyboard(Arc<AtomicBool>);

impl SessionKeyboard for RenderKeyboard {
    fn switch(&self, _layout: &Layout) -> anyhow::Result<()> {
        if self.0.load(Ordering::SeqCst) {
            bail!("KWin is not answering");
        }
        Ok(())
    }
}

/// What the driver reports when it is done.
pub struct Summary {
    pub files: usize,
    pub scale: f64,
}

/// The services of a shot (its helper is the scenario's fixture, set when the
/// flow is replaced).
fn services(shared: &Shared) -> Result<Services> {
    Ok(Services {
        backend: Arc::new(Simulated::load("generic")?) as Arc<dyn Backend>,
        keyboard: Arc::new(RenderKeyboard(Arc::clone(&shared.failing))),
        poll_ms: Some(20),
    })
}

fn send(shared: &Shared, event: Event) -> Result<()> {
    shared.post(move |installer| installer.dispatch(event))
}

/// A person types into a field of the page that is shown: the field's own
/// change reaches the flow.
fn type_into(shared: &Shared, shot: &str, field: &'static str, text: &str) -> Result<()> {
    let text = text.to_owned();
    let (before, _) = shared.current("typed");
    shared.post(move |installer| installer.ask_window("type", field, &text))?;
    let typed = shared.wait_for("typed", before)?;
    if typed != "ok" {
        bail!("{shot}: typing into {field}: {typed}");
    }
    shared.flush(&format!("{shot}: {field}"))?;
    shared.until(&format!("{shot}: {field}"), None, |_| true)
}

/// A person flips a check box of the page that is shown: its own toggle
/// reaches the flow.
fn flip(shared: &Shared, shot: &str, name: &'static str, on: bool) -> Result<()> {
    let (before, _) = shared.current("typed");
    shared.post(move |installer| {
        installer.ask_window("switch", name, if on { "on" } else { "off" })
    })?;
    let flipped = shared.wait_for("typed", before)?;
    if flipped != "ok" {
        bail!("{shot}: flipping {name}: {flipped}");
    }
    shared.flush(&format!("{shot}: {name}"))?;
    shared.until(&format!("{shot}: {name}"), None, |_| true)
}

/// Play every shot; the window quits at the end, whatever happens.
pub fn drive(
    shared: Arc<Shared>,
    layouts: Layouts,
    directory: PathBuf,
    dark: bool,
    outcome: Sender<Result<Summary, String>>,
) {
    let result = shoot(&shared, &layouts, &directory, dark).map_err(|e| format!("{e:#}"));
    let _ = shared.post(|installer| installer.ask_window("quit", "", ""));
    let _ = outcome.send(result);
}

fn shoot(shared: &Shared, layouts: &Layouts, directory: &Path, dark: bool) -> Result<Summary> {
    let scheme = if dark { "dark" } else { "light" };
    let mut files = 0;
    let mut scale = 1.0;
    // The window starts its `Installer` when its QML is complete.
    let started = Instant::now();
    while lock(&shared.thread).is_none() {
        if started.elapsed() > DEADLINE {
            bail!("the window did not start within {DEADLINE:?}");
        }
        std::thread::sleep(POLL);
    }
    // `ILEX_RENDER_ONLY=<text>` draws only the shots whose name has it (a
    // development aid: the self-check always draws them all).
    let only = std::env::var("ILEX_RENDER_ONLY").unwrap_or_default();
    for shot in SHOTS.iter().filter(|s| s.name.contains(&only)) {
        shared.failing.store(false, Ordering::SeqCst);
        let services = services(shared)?;
        let scenario = shot.scenario;
        shared.observe(Observed::initial());
        shared.post(move |installer| installer.reset_for_shot(scenario, services))?;
        // Every step starts from a settled page: the helper has said where an
        // installation is, the probe answered, no check and no layout switch
        // under way.
        shared.until(&format!("{}: the first page", shot.name), None, |_| true)?;
        for step in shot.steps {
            let event = match *step {
                Do::Continue => Event::Continue,
                Do::Back => {
                    // The way a person goes back: the page stack pops.
                    shared.post(|installer| installer.ask_window("back", "", ""))?;
                    shared.flush(&format!("{}: back", shot.name))?;
                    shared.until(&format!("{}: back", shot.name), None, |_| true)?;
                    continue;
                }
                Do::Jump(page) => Event::Jump(page),
                Do::FailSwitches => {
                    shared.failing.store(true, Ordering::SeqCst);
                    continue;
                }
                Do::Keyboard(id) => Event::Layout(
                    layouts
                        .index_of(id)
                        .with_context(|| format!("{}: no keyboard layout {id}", shot.name))?,
                ),
                Do::Language(locale) => Event::Language(
                    language::index_of(locale)
                        .with_context(|| format!("{}: no language {locale}", shot.name))?,
                ),
                Do::Machine(tag) => Event::Machine(tag.to_owned()),
                Do::Disk(id) => Event::Disk(id.to_owned()),
                Do::Mode(id) => Event::Mode(id.to_owned()),
                Do::Install => Event::Install,
                Do::Accept => Event::Confirmed(true),
                Do::Dismiss => Event::Confirmed(false),
                Do::Retry => Event::Retry,
                Do::LookAgain => Event::Probe,
                Do::PasswordsWiped => {
                    shared.flush(&format!("{}: the passwords", shot.name))?;
                    let (before, _) = shared.current("fields");
                    shared.post(|installer| installer.ask_window("fields", "", ""))?;
                    let lengths = shared.wait_for("fields", before)?;
                    if lengths != "0,0,0,0" {
                        bail!(
                            "{}: the password fields still hold text after the flow wiped them (lengths {lengths})",
                            shot.name
                        );
                    }
                    continue;
                }
                Do::Restart => Event::Restart,
                Do::RetryInstall => Event::RetryInstall,
                Do::ChooseLog => Event::ChooseLog,
                Do::SaveLog(id) => Event::SaveLog(id.to_owned()),
                Do::Wait(page) => {
                    shared.until(
                        &format!("{}: waiting for the {} page", shot.name, page.id()),
                        Some(page),
                        |_| true,
                    )?;
                    continue;
                }
                Do::ExpandLog => {
                    // The log's disclosure is the page's own control: opened
                    // the way a person does (the flow knows nothing of it).
                    shared.post(|installer| installer.ask_window("expand-log", "", ""))?;
                    shared.flush(&format!("{}: the log", shot.name))?;
                    continue;
                }
                Do::Name(text) => Event::Name(text.to_owned()),
                Do::Daily(id) => Event::Daily(id.to_owned()),
                Do::DailyName(text) => Event::DailyName(text.to_owned()),
                Do::DailySize(text) => Event::DailySize(text.to_owned()),
                Do::Encrypt(on) => {
                    flip(shared, shot.name, "encrypt", on)?;
                    continue;
                }
                Do::Chip(on) => {
                    flip(shared, shot.name, "chip", on)?;
                    continue;
                }
                Do::Passphrase(text) => {
                    type_into(shared, shot.name, "passphrase", text)?;
                    continue;
                }
                Do::PassphraseConfirm(text) => {
                    type_into(shared, shot.name, "passphrase-confirmation", text)?;
                    continue;
                }
                Do::Encrypted | Do::Unencrypted => {
                    if matches!(step, Do::Encrypted) {
                        type_into(shared, shot.name, "passphrase", PASSPHRASE)?;
                        type_into(shared, shot.name, "passphrase-confirmation", PASSPHRASE)?;
                    } else {
                        flip(shared, shot.name, "encrypt", false)?;
                    }
                    Event::Continue
                }
                Do::KeyRight | Do::KeyWrong | Do::KeyShort | Do::KeyNotInKey => {
                    let text = step.key_text().unwrap_or_default();
                    type_into(shared, shot.name, "recovery-key", &text)?;
                    continue;
                }
                Do::ConfirmRecovery => Event::ConfirmRecovery,
                Do::SkipRecovery => Event::ContinueWithoutKey,
                Do::Password(text) => {
                    type_into(shared, shot.name, "password", text)?;
                    continue;
                }
                Do::Confirm(text) => {
                    type_into(shared, shot.name, "confirmation", text)?;
                    continue;
                }
            };
            send(shared, event)?;
            // Each step's effect is complete before the next starts: the
            // queue has run, and no probe, check or layout switch it began is
            // still under way.
            shared.flush(&format!("{}: step", shot.name))?;
            shared.until(&format!("{}: step", shot.name), None, |_| true)?;
        }
        let at_end = |seen: &Observed| {
            (shot.check.is_empty() || seen.check == shot.check) && seen.dialog == shot.dialog
        };
        shared.until(
            &format!("{}: the final state", shot.name),
            Some(shot.page),
            at_end,
        )?;
        // `ILEX_RENDER_SETTLE_MS` lengthens it (a development aid).
        std::thread::sleep(
            std::env::var("ILEX_RENDER_SETTLE_MS")
                .ok()
                .and_then(|ms| ms.parse().ok())
                .map_or(SETTLE, Duration::from_millis),
        );
        // The page, the state and the window are what the shot names: the
        // application still agrees after the frames (nothing moved).
        shared.until(
            &format!("{}: the state after drawing", shot.name),
            Some(shot.page),
            at_end,
        )?;
        let pending = directory.join(format!("{}-{scheme}-pending.png", shot.name));
        let (before, _) = shared.current("grab");
        let path = pending.to_string_lossy().into_owned();
        shared.post(move |installer| installer.grab_to(&path))?;
        let grabbed = shared.wait_for("grab", before)?;
        if grabbed != "ok" {
            bail!("{}: drawing the page: {grabbed}", shot.name);
        }
        scale = *lock(&shared.scale);
        let named = directory.join(format!("{}-{scheme}-{scale}x.png", shot.name));
        std::fs::rename(&pending, &named)
            .with_context(|| format!("naming {}", named.display()))?;
        files += 1;
    }
    if files == 0 {
        bail!("no shots");
    }
    Ok(Summary { files, scale })
}
