//! The KDE edition's interface: Qt 6 Qt Quick with Kirigami 6 (Breeze style,
//! KDE HIG). The pages are QML (`qml/`); what they show and what they ask for
//! comes from one Rust QObject (cxx-qt, [`installer`]) over
//! `ilex-installer-flow`, which holds every rule, every sentence and every
//! decision about what is shown (`Flow::view`), as it does for the GNOME
//! edition's GTK interface. The QML draws the view and sends the person's
//! inputs as events; it decides nothing.
//!
//! It runs as the unprivileged `ilex-installer` user under KWin (see
//! `installer/next/editions/kde/`): KWin as the compositor with this window as
//! its only client, no Plasma shell, panel or other program.

mod installer;
mod keyboard;
mod platform;
pub mod render;

use std::sync::Arc;
use std::sync::mpsc;

use anyhow::{Context, Result};
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQuickStyle, QString, QUrl};
use ilex_installer_flow::backend::{Backend, DbusBackend};
use ilex_installer_flow::flow::{Flow, MarkerConfig};
use ilex_installer_flow::keyboard::{Layouts, SessionKeyboard, Sources};
use ilex_installer_flow::markers;
use ilex_installer_flow::runner::Services;
use ilex_installer_flow::selfcheck;
use ilex_installer_flow::simulate::Simulated;

use crate::identity::Edition;
use crate::serial;

/// The Qt Quick Controls style: qqc2-breeze-style, Kirigami's own style
/// without the Plasma platform theme (a developer's `QT_QUICK_CONTROLS_STYLE`
/// still wins).
const STYLE: &str = "org.kde.breeze";
/// The icon theme Kirigami's icons are found in (breeze-icon-theme).
const ICON_THEME: &str = "breeze";
/// The window's QML, in the resources `build.rs` registers.
const MAIN: &str = "qrc:/qt/qml/org/ilex/installer/qml/Main.qml";
/// What the window says when the image has no edition record (a development
/// run outside the live image): the edition record is the only place the name
/// lives.
const UNNAMED: &str = "ilex";

/// Breeze Dark's colours, for the renderer's dark pictures (`--dark`).
const DARK_COLORS: &str = include_str!("../../kde-dark/kdeglobals");

/// What `main` decided.
pub struct Launch {
    pub fullscreen: bool,
    /// A fixture scenario instead of the helper: marks the markers as
    /// simulated (the first page does not talk to the helper).
    pub simulate: Option<String>,
    /// The image's edition record.
    pub edition: Option<Edition>,
}

/// The Qt application and its window, until the window quits.
fn exec(window: impl FnOnce()) -> Result<()> {
    let mut app = QGuiApplication::new();
    QGuiApplication::set_desktop_file_name(&QString::from("org.ilex.Installer"));
    // Before the engine registers the Controls types: the style cannot change
    // afterwards.
    if std::env::var_os("QT_QUICK_CONTROLS_STYLE").is_none() {
        QQuickStyle::set_style(&QString::from(STYLE));
    }
    platform::set_icon_theme(ICON_THEME);
    window();
    let mut engine = QQmlApplicationEngine::new();
    if let Some(mut engine) = engine.as_mut() {
        // A window that cannot be built is a failed session, not an installer
        // with no window: the session script stops and the unit restarts it.
        engine
            .as_mut()
            .on_object_creation_failed(|_, url| {
                eprintln!("ilex-installer: {url} could not be loaded");
                std::process::exit(1);
            })
            .release();
        engine.load(&QUrl::from(MAIN));
    }
    let status = app.as_mut().map_or(1, |app| app.exec());
    anyhow::ensure!(status == 0, "the Qt event loop ended with status {status}");
    Ok(())
}

pub fn run(launch: Launch) -> Result<()> {
    let (edition, display_name) = match &launch.edition {
        Some(record) => (record.edition.clone(), record.display_name.clone()),
        None => {
            eprintln!("ilex-installer: no edition record; a development run");
            ("kde".to_owned(), UNNAMED.to_owned())
        }
    };
    let backend: Arc<dyn Backend> = match &launch.simulate {
        Some(name) => Arc::new(Simulated::load(name)?),
        None => Arc::new(DbusBackend::new()),
    };
    let layouts = Layouts::load(&Sources::default()).context("the keyboard layouts")?;
    let keyboard: Arc<dyn SessionKeyboard + Send + Sync> = Arc::new(keyboard::Kwin);
    // The flow decides which marker lines exist and when; the page prints what
    // it is handed.
    let flow = Flow::new(layouts.clone())
        .with_markers(MarkerConfig::session(
            edition.clone(),
            launch.simulate.is_some(),
        ))
        .with_network_check();
    exec(|| {
        installer::hand_over(installer::Handover {
            edition,
            display_name,
            fullscreen: launch.fullscreen,
            flow,
            services: Services {
                backend,
                keyboard,
                poll_ms: None,
            },
            render: None,
            layouts,
        });
    })
}

/// Every page and error state drawn to files (`--self-check --render`): the
/// window runs under the session's compositor (KWin, headless in the trial)
/// and a driver thread plays the shots.
pub fn render_all(directory: &std::path::Path, dark: bool) -> Result<()> {
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    // The live image's own catalogue when it has one; otherwise the
    // self-check's sample (a missing catalogue is reported, not fatal), so
    // that a machine without xkeyboard-config draws the same pages.
    let layouts = match Layouts::load(&Sources::default()) {
        Ok(layouts) => layouts,
        Err(error) => {
            eprintln!("ilex-installer: drawing with the sample keyboard layouts: {error:#}");
            selfcheck::sample_layouts().context("the sample keyboard layouts")?
        }
    };
    let mut kept_config = None;
    if dark {
        // The image has no Plasma and so no colour schemes: the light Breeze
        // is what Qt draws. The dark pictures give it Breeze Dark's colours in
        // the file KDE's colour classes read, in a directory of this run's own.
        let config = std::env::temp_dir().join(format!("ilex-render-{}", std::process::id()));
        std::fs::create_dir_all(&config)
            .with_context(|| format!("creating {}", config.display()))?;
        std::fs::write(config.join("kdeglobals"), DARK_COLORS).context("writing kdeglobals")?;
        // SAFETY: nothing else is running yet (the driver thread and the Qt
        // application start below), so no other thread reads the environment.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
        kept_config = Some(config);
    }
    let shared = render::Shared::new();
    let (outcome, result) = mpsc::channel();
    {
        let shared = Arc::clone(&shared);
        let layouts = layouts.clone();
        let directory = directory.to_path_buf();
        std::thread::spawn(move || render::drive(shared, layouts, directory, dark, outcome));
    }
    let services = Services {
        backend: Arc::new(Simulated::load("generic")?),
        keyboard: Arc::new(keyboard::Kwin),
        poll_ms: Some(20),
    };
    exec(|| {
        installer::hand_over(installer::Handover {
            edition: "kde".to_owned(),
            display_name: UNNAMED.to_owned(),
            fullscreen: false,
            // No markers: the renderer prints nothing but its own result.
            flow: Flow::new(layouts.clone()),
            services,
            render: Some(shared),
            layouts,
        });
    })?;
    if let Some(config) = kept_config {
        let _ = std::fs::remove_dir_all(config);
    }
    let summary = result
        .recv()
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
