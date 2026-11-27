//! `ilex-installer --self-check --render DIR`: every page and every error
//! state drawn off-screen to a PNG file, from the simulation's fixtures, so
//! the interface can be looked at and checked without a real installation.
//! The pages are the application's own (`app::App`, driven with the same
//! messages the person's input sends); each is drawn with GTK's renderer from
//! the window's widget tree, at the surface's own scale, so under a compositor
//! that scales to 150% the files are 150%. It needs a display to create the
//! windows: a headless compositor is enough (the `installer::ui` trial runs it
//! under GNOME Kiosk's own headless mode).
//!
//! Files are `<shot>-<light|dark>-<scale>x.png`. A shot is taken only when the
//! application reports the page and the state the shot names (the page shown,
//! no probe, check or layout switch under way) and the navigation view really
//! shows that page, never after a fixed wait alone.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_installer_flow::backend::Backend;
use ilex_installer_flow::flow::{Flow, PageId};
use ilex_installer_flow::keyboard::{Layout, Layouts, SessionKeyboard, Sources};
use ilex_installer_flow::language;
use ilex_installer_flow::markers;
use ilex_installer_flow::selfcheck;
use ilex_installer_flow::simulate::Simulated;
use relm4::adw::{self, prelude::*};
use relm4::gtk::{self, gdk, glib, graphene};
use relm4::prelude::*;

use super::app::{App, Init, Msg, Observed};
use crate::serial;

/// What a shot does after its window is up and the helper has said where an
/// installation is.
#[derive(Clone, Copy)]
enum Do {
    Continue,
    Back,
    Jump(PageId),
    /// From now on the compositor refuses a layout switch.
    FailSwitches,
    Keyboard(&'static str),
    Language(&'static str),
    Machine(&'static str),
    Name(&'static str),
    Password(&'static str),
    Confirm(&'static str),
    /// A disk row (its id).
    Disk(&'static str),
    /// The review page's destructive button, and the dialog's two answers.
    Install,
    Accept,
    Dismiss,
    /// The installing page's log, opened.
    ExpandLog,
    /// Until the installation (which goes on by itself) has reached this page.
    Wait(PageId),
    Restart,
    RetryInstall,
    ChooseLog,
    SaveLog(&'static str),
}

struct Shot {
    name: &'static str,
    scenario: &'static str,
    /// The page the shot ends on, and the word of its check (`Flow::check_word`)
    /// when it is the point of the shot (`""`: any, as long as none is running).
    page: PageId,
    check: &'static str,
    /// The dialog the shot ends with open (`confirm`, `log`), if any.
    dialog: &'static str,
    steps: &'static [Do],
}

const PASSWORD: &str = "correct horse battery";

macro_rules! shots {
    ($( $name:literal, $scenario:literal, $page:ident, $check:literal, $dialog:literal, [ $($step:expr),* $(,)? ] );* $(;)?) => {
        &[ $( Shot { name: $name, scenario: $scenario, page: PageId::$page, check: $check, dialog: $dialog, steps: &[ $($step),* ] } ),* ]
    };
}

#[rustfmt::skip]
const SHOTS: &[Shot] = shots![
    "welcome", "generic", Welcome, "", "", [];
    "welcome-german", "generic", Welcome, "", "", [Do::Keyboard("de-nodeadkeys"), Do::Language("de_DE.UTF-8")];
    "welcome-bios", "bios", Welcome, "", "", [];
    "welcome-helper-down", "helper-down", Welcome, "", "", [];
    "welcome-keyboard-failed", "generic", Welcome, "", "", [Do::FailSwitches, Do::Keyboard("de-nodeadkeys")];
    "machine-not-recognised", "generic", Machine, "", "", [Do::Continue];
    "machine-detected", "ms-7c35", Machine, "", "", [Do::Continue];
    "machine-detected-board", "ms-7c35-board", Machine, "", "", [Do::Continue];
    "machine-chosen", "ms-7c35", Machine, "", "", [Do::Continue, Do::Machine("generic.gnome")];
    "machine-conflict", "conflict", Machine, "", "", [Do::Continue];
    "machine-no-composition", "no-composition", Machine, "", "", [Do::Continue];
    "machine-boot-argument", "boot-override", Machine, "", "", [Do::Continue];
    "machine-boot-argument-unknown", "bad-override", Machine, "", "", [Do::Continue];
    "machine-helper-down", "helper-down", Machine, "", "", [Do::Jump(PageId::Machine)];
    "machine-bios", "bios", Machine, "", "", [Do::Jump(PageId::Machine)];
    "disk", "generic", Disk, "", "", [Do::Continue, Do::Continue];
    "disk-chosen", "generic", Disk, "", "", [Do::Continue, Do::Continue, Do::Disk("ata-WDC_WDS500G2B0A_214512800345")];
    "disk-none", "no-disk", Disk, "", "", [Do::Continue, Do::Continue];
    "disk-helper-down", "helper-down", Disk, "", "", [Do::Jump(PageId::Disk)];
    "disk-bios", "bios", Disk, "", "", [Do::Jump(PageId::Disk)];
    "account-empty", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue];
    "account-name-invalid", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("Ilex Maint")];
    "account-name-reserved", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("root")];
    "account-password-short", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password("abc"), Do::Confirm("abc")];
    "account-password-mismatch", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm("correct horse batter")];
    "account-valid", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD)];
    "account-refused-by-helper", "generic", Account, "not-run", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("rpcuser"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review", "generic", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-machine", "ms-7c35", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-other-disk", "generic", Review, "ok", "", [Do::Continue, Do::Continue, Do::Disk("ata-WDC_WDS500G2B0A_214512800345"), Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-no-disk", "no-disk", Review, "no-disk", "", [Do::Continue, Do::Continue, Do::Jump(PageId::Account), Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-registry-down", "registry-down", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "review-back-to-account", "generic", Account, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Back];
    "review-erase-dialog", "generic", Review, "ok", "confirm", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install];
    "review-erase-cancelled", "generic", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Dismiss];
    "review-apply-refused", "generic+apply-refused", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "review-apply-not-started", "generic+apply-lost", Review, "failed", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "installing-early", "generic+early", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "installing", "generic+stalled", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "installing-log", "generic+stalled", Installing, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::ExpandLog];
    "installing-reattached", "generic+attach-installing", Installing, "", "", [];
    "done", "generic", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "done-restarting", "generic", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Done), Do::Restart];
    "done-restart-refused", "generic+finish-fails", Done, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Done), Do::Restart];
    "done-reattached", "generic+attach-done", Done, "", "", [];
    "problem-image", "generic+fail-image", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-partition", "generic+fail-partition", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-helper-stopped", "generic+helper-stopped", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-lost", "generic+lost", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept];
    "problem-reattached", "generic+attach-failed", Problem, "", "", [];
    "problem-restart-refused", "generic+fail-image+finish-fails", Problem, "", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::Restart];
    "problem-retry", "generic+fail-image", Review, "ok", "", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::RetryInstall];
    "problem-save-log", "generic+fail-image+sticks", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog];
    "problem-save-log-no-stick", "generic+fail-image+bare", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog];
    "problem-save-log-saved", "generic+fail-image+sticks", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog, Do::SaveLog("usb-SanDisk_Cruzer_Blade_4C530001-0:0")];
    "problem-save-log-failed", "generic+fail-image+sticks+log-fails", Problem, "", "log", [Do::Continue, Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Install, Do::Accept, Do::Wait(PageId::Problem), Do::ChooseLog, Do::SaveLog("usb-SanDisk_Cruzer_Blade_4C530001-0:0")];
];

/// How often the application's state is looked at while a shot waits for it.
const POLL: Duration = Duration::from_millis(20);
/// How long a shot waits for the page and state it names before it fails.
const DEADLINE: Duration = Duration::from_secs(20);
/// Frames given to GTK to lay out and draw a page it already shows.
const FRAMES: u32 = 3;

pub fn run(directory: &Path, dark: bool) -> Result<()> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    // The live image's own catalogue when it has one; otherwise the
    // self-check's sample (the same rule `--self-check --render` follows in
    // `main`: a missing catalogue is reported, not fatal), so that a machine
    // without xkeyboard-config draws the same pages.
    let layouts = match Layouts::load(&Sources::default()) {
        Ok(layouts) => layouts,
        Err(error) => {
            eprintln!("ilex-installer: drawing with the sample keyboard layouts: {error:#}");
            selfcheck::sample_layouts().context("the sample keyboard layouts")?
        }
    };
    let outcome: Rc<RefCell<Option<Result<Summary, String>>>> = Rc::new(RefCell::new(None));
    let job = Job {
        directory: directory.to_path_buf(),
        dark,
        layouts,
        outcome: Rc::clone(&outcome),
    };
    RelmApp::new("org.ilex.Installer.Render")
        .with_args(Vec::new())
        .visible_on_activate(false)
        .run::<Driver>(job);
    let summary = outcome
        .borrow_mut()
        .take()
        .context("the renderer ended without a result")?
        .map_err(|e| anyhow::anyhow!(e))?;
    serial::marker(&markers::render_ok(
        summary.files,
        dark,
        summary.scale,
        directory,
    ));
    Ok(())
}

struct Summary {
    files: usize,
    scale: f64,
}

/// A session whose layout switches work until a shot says they do not (a
/// compositor that is gone or refuses).
struct RenderKeyboard(Arc<AtomicBool>);

impl SessionKeyboard for RenderKeyboard {
    fn switch(&self, _layout: &Layout) -> anyhow::Result<()> {
        if self.0.load(Ordering::SeqCst) {
            bail!("GNOME Kiosk is not answering");
        }
        Ok(())
    }
}

struct Job {
    directory: PathBuf,
    dark: bool,
    layouts: Layouts,
    outcome: Rc<RefCell<Option<Result<Summary, String>>>>,
}

/// The component the application loop runs: it never shows a window of its
/// own, only starts the shots.
struct Driver;

impl SimpleComponent for Driver {
    type Init = Job;
    type Input = ();
    type Output = ();
    type Root = gtk::Window;
    type Widgets = ();

    fn init_root() -> Self::Root {
        gtk::Window::new()
    }

    fn init(job: Job, _root: Self::Root, _sender: ComponentSender<Self>) -> ComponentParts<Self> {
        if let Some(settings) = gtk::Settings::default() {
            settings.set_gtk_enable_animations(false);
        }
        adw::StyleManager::default().set_color_scheme(if job.dark {
            adw::ColorScheme::ForceDark
        } else {
            adw::ColorScheme::ForceLight
        });
        let app = relm4::main_application();
        let hold = app.hold();
        let outcome = Rc::clone(&job.outcome);
        glib::MainContext::default().spawn_local(async move {
            let result = shoot(&job).await.map_err(|e| format!("{e:#}"));
            *outcome.borrow_mut() = Some(result);
            drop(hold);
            relm4::main_application().quit();
        });
        ComponentParts {
            model: Driver,
            widgets: (),
        }
    }

    fn update(&mut self, _message: (), _sender: ComponentSender<Self>) {}
}

async fn pause(time: Duration) {
    glib::timeout_future(time).await;
}

/// The page the navigation view shows.
fn visible_page(window: &adw::ApplicationWindow) -> Option<PageId> {
    find_navigation(window.upcast_ref())?
        .visible_page()
        .and_then(|p| p.tag())
        .and_then(|t| PageId::from_id(&t))
}

/// Wait until the application reports that nothing is under way (`want` says
/// what else must hold) and the navigation view shows `page`.
async fn until(
    what: &str,
    window: &adw::ApplicationWindow,
    observer: &Arc<Mutex<Observed>>,
    page: Option<PageId>,
    want: impl Fn(&Observed) -> bool,
) -> Result<()> {
    let started = Instant::now();
    loop {
        let seen = observer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let shown = visible_page(window);
        if seen.settled() && want(&seen) && page.is_none_or(|p| shown == Some(p) && seen.page == p)
        {
            return Ok(());
        }
        if started.elapsed() > DEADLINE {
            bail!(
                "{what}: after {DEADLINE:?} the application reports {seen:?} and shows {shown:?}, \
                 expected {}",
                page.map_or("any page".to_owned(), |p| p.id().to_owned())
            );
        }
        pause(POLL).await;
    }
}

/// Wait until the application has handled everything sent to it so far (its
/// queue is in order), then until nothing it started is under way.
async fn flush(
    what: &str,
    controller: &Controller<App>,
    window: &adw::ApplicationWindow,
    observer: &Arc<Mutex<Observed>>,
) -> Result<()> {
    let handled = Arc::new(AtomicBool::new(false));
    controller.emit(Msg::Ping(Arc::clone(&handled)));
    let started = Instant::now();
    while !handled.load(Ordering::SeqCst) {
        if started.elapsed() > DEADLINE {
            bail!("{what}: the application did not handle its messages within {DEADLINE:?}");
        }
        pause(POLL).await;
    }
    until(what, window, observer, None, |_| true).await
}

/// A person types into a password field: find it by its widget name and set
/// its text, so the field's own change reaches the flow.
fn type_into(window: &adw::ApplicationWindow, name: &str, text: &str) -> Result<()> {
    fn find(widget: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
        if widget.widget_name() == name {
            return Some(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(c) = child {
            if let Some(found) = find(&c, name) {
                return Some(found);
            }
            child = c.next_sibling();
        }
        widget
            .downcast_ref::<adw::ApplicationWindow>()
            .and_then(|w| w.content())
            .and_then(|content| find(&content, name))
    }
    let field = find(window.upcast_ref(), name).with_context(|| format!("no field {name}"))?;
    field
        .downcast_ref::<gtk::Editable>()
        .with_context(|| format!("{name} is not a text field"))?
        .set_text(text);
    Ok(())
}

async fn shoot(job: &Job) -> Result<Summary> {
    let scheme = if job.dark { "dark" } else { "light" };
    let mut files = 0;
    let mut scale = 1.0;
    // `ILEX_RENDER_ONLY=<text>` draws only the shots whose name has it (a
    // development aid: the self-check always draws them all).
    let only = std::env::var("ILEX_RENDER_ONLY").unwrap_or_default();
    for shot in SHOTS.iter().filter(|s| s.name.contains(&only)) {
        let backend: Arc<dyn Backend> = Arc::new(Simulated::load(shot.scenario)?);
        let observer = Arc::new(Mutex::new(Observed {
            page: PageId::Welcome,
            check: "not-run",
            probe_pending: true,
            keyboard_switching: false,
            attaching: true,
            dialog: "",
        }));
        let failing = Arc::new(AtomicBool::new(false));
        let controller = App::builder()
            .launch(Init {
                backend,
                keyboard: Arc::new(RenderKeyboard(Arc::clone(&failing))),
                // No markers: the renderer prints nothing but its own result.
                flow: Flow::new(job.layouts.clone()),
                fullscreen: false,
                stats: false,
                observer: Some(Arc::clone(&observer)),
                // A simulated installation does not take its seconds.
                poll_ms: Some(20),
            })
            .detach();
        let window = controller.widget().clone();
        window.set_default_size(1280, 800);
        window.present();
        // Every step starts from a settled page: the helper has said where an
        // installation is, the probe answered, no check and no layout switch
        // under way. (An installation that is under way, or ended, is the page
        // the flow starts on.)
        until(
            &format!("{}: the first page", shot.name),
            &window,
            &observer,
            None,
            |_| true,
        )
        .await?;
        for step in shot.steps {
            let message = match *step {
                Do::Continue => Msg::Continue,
                Do::Back => {
                    // The way a person goes back: the navigation view pops.
                    if let Some(nav) = find_navigation(window.upcast_ref()) {
                        nav.pop();
                    }
                    flush(
                        &format!("{}: back", shot.name),
                        &controller,
                        &window,
                        &observer,
                    )
                    .await?;
                    continue;
                }
                Do::Jump(page) => Msg::Jump(page),
                Do::FailSwitches => {
                    failing.store(true, Ordering::SeqCst);
                    continue;
                }
                Do::Keyboard(id) => Msg::Layout(
                    job.layouts
                        .index_of(id)
                        .with_context(|| format!("{}: no keyboard layout {id}", shot.name))?
                        as u32,
                ),
                Do::Language(locale) => Msg::Language(
                    language::index_of(locale)
                        .with_context(|| format!("{}: no language {locale}", shot.name))?
                        as u32,
                ),
                Do::Machine(tag) => Msg::Machine(tag.to_owned()),
                Do::Disk(id) => Msg::Disk(id.to_owned()),
                Do::Install => Msg::Install,
                Do::Accept => Msg::Confirmed(true),
                Do::Dismiss => Msg::Confirmed(false),
                Do::Restart => Msg::Restart,
                Do::RetryInstall => Msg::RetryInstall,
                Do::ChooseLog => Msg::ChooseLog,
                Do::SaveLog(id) => Msg::SaveLog(id.to_owned()),
                Do::Wait(page) => {
                    until(
                        &format!("{}: waiting for the {} page", shot.name, page.id()),
                        &window,
                        &observer,
                        Some(page),
                        |_| true,
                    )
                    .await?;
                    continue;
                }
                Do::ExpandLog => {
                    // The expander is the page's own widget: opened the way a
                    // person does (the flow knows nothing of it).
                    let expander = find_expander(window.upcast_ref())
                        .with_context(|| format!("{}: no log expander", shot.name))?;
                    expander.set_expanded(true);
                    flush(
                        &format!("{}: the log", shot.name),
                        &controller,
                        &window,
                        &observer,
                    )
                    .await?;
                    continue;
                }
                Do::Name(text) => Msg::Name(text.to_owned()),
                Do::Password(text) => {
                    type_into(&window, "password", text)?;
                    flush(
                        &format!("{}: password", shot.name),
                        &controller,
                        &window,
                        &observer,
                    )
                    .await?;
                    continue;
                }
                Do::Confirm(text) => {
                    type_into(&window, "confirmation", text)?;
                    flush(
                        &format!("{}: confirmation", shot.name),
                        &controller,
                        &window,
                        &observer,
                    )
                    .await?;
                    continue;
                }
            };
            controller.emit(message);
            // Each step's effect is complete before the next starts: the
            // queue has run, and no probe, check or layout switch it began is
            // still under way.
            flush(
                &format!("{}: step", shot.name),
                &controller,
                &window,
                &observer,
            )
            .await?;
        }
        until(
            &format!("{}: the final state", shot.name),
            &window,
            &observer,
            Some(shot.page),
            |seen| (shot.check.is_empty() || seen.check == shot.check) && seen.dialog == shot.dialog,
        )
        .await?;
        for _ in 0..FRAMES {
            pause(Duration::from_millis(40)).await;
        }
        // The page, the state and the view are what the shot names: the
        // application still agrees after the frames (nothing moved).
        until(
            &format!("{}: the state after drawing", shot.name),
            &window,
            &observer,
            Some(shot.page),
            |seen| (shot.check.is_empty() || seen.check == shot.check) && seen.dialog == shot.dialog,
        )
        .await?;
        let path = job
            .directory
            .join(format!("{}-{scheme}-pending.png", shot.name));
        // A page that was just shown can have nothing to draw for a frame or
        // two (the frame clock has not run since it changed): ask again.
        let mut tries = 0;
        scale = loop {
            if let Some(scale) = capture(&window, &path)
                .with_context(|| format!("{}: drawing the page", shot.name))?
            {
                break scale;
            }
            tries += 1;
            if tries > 40 {
                bail!(
                    "{}: the window drew nothing in 40 tries (mapped {}, visible {}, {}x{})",
                    shot.name,
                    window.is_mapped(),
                    window.is_visible(),
                    window.width(),
                    window.height()
                );
            }
            pause(Duration::from_millis(50)).await;
        };
        let named = job
            .directory
            .join(format!("{}-{scheme}-{scale}x.png", shot.name));
        std::fs::rename(&path, &named).with_context(|| format!("naming {}", named.display()))?;
        files += 1;
        window.destroy();
        pause(POLL).await;
    }
    if files == 0 {
        bail!("no shots");
    }
    Ok(Summary { files, scale })
}

/// The installing page's log expander.
fn find_expander(widget: &gtk::Widget) -> Option<gtk::Expander> {
    if let Some(expander) = widget.downcast_ref::<gtk::Expander>() {
        return Some(expander.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_expander(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    widget
        .downcast_ref::<adw::ApplicationWindow>()
        .and_then(|w| w.content())
        .and_then(|content| find_expander(&content))
}

/// The window's navigation view, to go back the way the person does.
fn find_navigation(widget: &gtk::Widget) -> Option<adw::NavigationView> {
    if let Some(nav) = widget.downcast_ref::<adw::NavigationView>() {
        return Some(nav.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_navigation(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    // An `AdwApplicationWindow` keeps its content apart from its children.
    widget
        .downcast_ref::<adw::ApplicationWindow>()
        .and_then(|w| w.content())
        .and_then(|content| find_navigation(&content))
}

/// Draw the window's widget tree to a PNG at the surface's scale; returns the
/// scale (`1`, `1.5`), or `None` when the tree has nothing to draw yet.
fn capture(window: &adw::ApplicationWindow, path: &Path) -> Result<Option<f64>> {
    let native = window.native().context("the window has no native")?;
    let surface = native.surface().context("the window has no surface")?;
    let renderer = native.renderer().context("the window has no renderer")?;
    let scale = surface.scale();
    let (width, height) = (window.width(), window.height());
    if width <= 0 || height <= 0 {
        bail!("the window has no size ({width}x{height})");
    }
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    snapshot.scale(scale as f32, scale as f32);
    gdk::prelude::PaintableExt::snapshot(
        &paintable,
        &snapshot,
        f64::from(width),
        f64::from(height),
    );
    let Some(node) = snapshot.to_node() else {
        return Ok(None);
    };
    let viewport = graphene::Rect::new(
        0.0,
        0.0,
        (f64::from(width) * scale).ceil() as f32,
        (f64::from(height) * scale).ceil() as f32,
    );
    let texture = renderer.render_texture(&node, Some(&viewport));
    texture
        .save_to_png(path)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(Some(scale))
}
