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
//! Files are `<shot>-<light|dark>-<scale>x.png`.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use ilex_installer_flow::backend::Backend;
use ilex_installer_flow::flow::PageId;
use ilex_installer_flow::keyboard::{Layouts, NoSessionKeyboard, Sources};
use ilex_installer_flow::language;
use ilex_installer_flow::simulate::Simulated;
use relm4::adw::{self, prelude::*};
use relm4::gtk::{self, gdk, glib, graphene};
use relm4::prelude::*;

use super::app::{App, Init, Msg};
use crate::serial;

/// What a shot does after its window is up and the probe answered.
#[derive(Clone, Copy)]
enum Do {
    Continue,
    Back,
    Jump(PageId),
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
    steps: &'static [Do],
}

const PASSWORD: &str = "correct horse battery";

macro_rules! shots {
    ($( $name:literal, $scenario:literal, [ $($step:expr),* $(,)? ] );* $(;)?) => {
        &[ $( Shot { name: $name, scenario: $scenario, steps: &[ $($step),* ] } ),* ]
    };
}

#[rustfmt::skip]
const SHOTS: &[Shot] = shots![
    "welcome", "generic", [];
    "welcome-german", "generic", [Do::Keyboard("de-nodeadkeys"), Do::Language("de_DE.UTF-8")];
    "welcome-bios", "bios", [];
    "welcome-helper-down", "helper-down", [];
    "machine-not-recognised", "generic", [Do::Continue];
    "machine-detected", "ms-7c35", [Do::Continue];
    "machine-detected-board", "ms-7c35-board", [Do::Continue];
    "machine-chosen", "ms-7c35", [Do::Continue, Do::Machine("generic.gnome")];
    "machine-conflict", "conflict", [Do::Continue];
    "machine-no-composition", "no-composition", [Do::Continue];
    "machine-boot-argument", "boot-override", [Do::Continue];
    "machine-boot-argument-unknown", "bad-override", [Do::Continue];
    "machine-helper-down", "helper-down", [Do::Jump(PageId::Machine)];
    "account-empty", "generic", [Do::Continue, Do::Continue];
    "account-name-invalid", "generic", [Do::Continue, Do::Continue, Do::Name("Ilex Maint")];
    "account-name-reserved", "generic", [Do::Continue, Do::Continue, Do::Name("root")];
    "account-password-short", "generic", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password("abc"), Do::Confirm("abc")];
    "account-password-mismatch", "generic", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm("correct horse batter")];
    "account-valid", "generic", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD)];
    "account-refused-by-helper", "generic", [Do::Continue, Do::Continue, Do::Name("rpcuser"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready", "generic", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-machine", "ms-7c35", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-no-disk", "no-disk", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-registry-down", "registry-down", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue];
    "ready-back-to-account", "generic", [Do::Continue, Do::Continue, Do::Name("ilexmaint"), Do::Password(PASSWORD), Do::Confirm(PASSWORD), Do::Continue, Do::Back];
];

/// How long a step is given to take effect: the worker threads that answer a
/// probe or a plan, and the widgets' own updates.
const SETTLE: Duration = Duration::from_millis(250);

pub fn run(directory: &Path, dark: bool) -> Result<()> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    let layouts = Layouts::load(&Sources::default()).context("the keyboard layouts")?;
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
    serial::marker(&format!(
        "ILEX-INSTALLER-RENDER ok shots={} scheme={} scale={} dir={}",
        summary.files,
        if dark { "dark" } else { "light" },
        summary.scale,
        directory.display()
    ));
    Ok(())
}

struct Summary {
    files: usize,
    scale: f64,
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

async fn settle() {
    glib::timeout_future(SETTLE).await;
}

async fn shoot(job: &Job) -> Result<Summary> {
    let scheme = if job.dark { "dark" } else { "light" };
    let mut files = 0;
    let mut scale = 1.0;
    for shot in SHOTS {
        let backend: Arc<dyn Backend> = Arc::new(Simulated::load(shot.scenario)?);
        let controller = App::builder()
            .launch(Init {
                backend,
                keyboard: Arc::new(NoSessionKeyboard),
                layouts: job.layouts.clone(),
                fullscreen: false,
                edition: "gnome".to_owned(),
                markers: false,
                stats: false,
            })
            .detach();
        let window = controller.widget().clone();
        window.set_default_size(1280, 800);
        window.present();
        settle().await;
        settle().await;
        for step in shot.steps {
            let message = match *step {
                Do::Continue => Msg::Continue,
                Do::Back => {
                    // The way a person goes back: the navigation view pops.
                    if let Some(nav) = find_navigation(window.upcast_ref()) {
                        nav.pop();
                    }
                    settle().await;
                    continue;
                }
                Do::Jump(page) => Msg::Jump(page),
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
                Do::Password(text) => Msg::Password(text.to_owned()),
                Do::Confirm(text) => Msg::Confirmation(text.to_owned()),
            };
            controller.emit(message);
            settle().await;
        }
        settle().await;
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
        settle().await;
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
