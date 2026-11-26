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

/// What a shot does after its window is up and the probe answered.
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
}

struct Shot {
    name: &'static str,
    scenario: &'static str,
    /// The page the shot ends on, and the word of its check (`Flow::check_word`)
    /// when it is the point of the shot (`""`: any, as long as none is running).
    page: PageId,
    check: &'static str,
    steps: &'static [Do],
}

const PASSWORD: &str = "correct horse battery";

macro_rules! shots {
    ($( $name:literal, $scenario:literal, $page:ident, $check:literal, [ $($step:expr),* $(,)? ] );* $(;)?) => {
        &[ $( Shot { name: $name, scenario: $scenario, page: PageId::$page, check: $check, steps: &[ $($step),* ] } ),* ]
    };
}

#[rustfmt::skip]
const SHOTS: &[Shot] = shots![
    "welcome", "generic", Welcome, "", [];
    "welcome-german", "generic", Welcome, "", [Do::Keyboard("de-nodeadkeys"), Do::Language("de_DE.UTF-8")];
    "welcome-bios", "bios", Welcome, "", [];
    "welcome-helper-down", "helper-down", Welcome, "", [];
    "welcome-keyboard-failed", "generic", Welcome, "", [Do::FailSwitches, Do::Keyboard("de-nodeadkeys")];
    "machine-not-recognised", "generic", Machine, "", [Do::Continue];
    "machine-detected", "ms-7c35", Machine, "", [Do::Continue];
    "machine-detected-board", "ms-7c35-board", Machine, "", [Do::Continue];
    "machine-chosen", "ms-7c35", Machine, "", [Do::Continue, Do::Machine("generic.gnome")];
    "machine-conflict", "conflict", Machine, "", [Do::Continue];
    "machine-no-composition", "no-composition", Machine, "", [Do::Continue];
    "machine-boot-argument", "boot-override", Machine, "", [Do::Continue];
    "machine-boot-argument-unknown", "bad-override", Machine, "", [Do::Continue];
    "machine-helper-down", "helper-down", Machine, "", [Do::Jump(PageId::Machine)];
    "machine-bios", "bios", Machine, "", [Do::Jump(PageId::Machine)];
    "account-empty", "generic", Account, "not-run", [Do::Continue, Do::Continue];
    "account-name-invalid", "generic", Account, "not-run", [Do::Continue, Do::Continue, Do::Name("Ilex Maint")];
    "account-name-reserved", "generic", Account, "not-run", [Do::Continue, Do::Continue, Do::Name("root")];
    "account-password-short", "generic", Account, "not-run", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password("abc"), Do::Confirm("abc")];
    "account-password-mismatch", "generic", Account, "not-run", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm("correct horse batter")];
    "account-valid", "generic", Account, "not-run", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD)];
    "account-refused-by-helper", "generic", Account, "not-run", [Do::Continue, Do::Continue, Do::Name("rpcuser"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready", "generic", Ready, "ok", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-machine", "ms-7c35", Ready, "ok", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-no-disk", "no-disk", Ready, "no-disk", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-registry-down", "registry-down", Ready, "failed", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-back-to-account", "generic", Account, "ok", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Back];
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
    for shot in SHOTS {
        let backend: Arc<dyn Backend> = Arc::new(Simulated::load(shot.scenario)?);
        let observer = Arc::new(Mutex::new(Observed {
            page: PageId::Welcome,
            check: "not-run",
            probe_pending: true,
            keyboard_switching: false,
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
            })
            .detach();
        let window = controller.widget().clone();
        window.set_default_size(1280, 800);
        window.present();
        // Every step starts from a settled page: the probe answered, no check
        // and no layout switch under way, and the first page shown.
        until(
            &format!("{}: the first page", shot.name),
            &window,
            &observer,
            Some(PageId::Welcome),
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
            |seen| shot.check.is_empty() || seen.check == shot.check,
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
            |seen| shot.check.is_empty() || seen.check == shot.check,
        )
        .await?;
        let path = job
            .directory
            .join(format!("{}-{scheme}-pending.png", shot.name));
        scale = capture(&window, &path)?;
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
/// scale (`1`, `1.5`).
fn capture(window: &adw::ApplicationWindow, path: &Path) -> Result<f64> {
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
    let node = snapshot.to_node().context("the window drew nothing")?;
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
    Ok(scale)
}
