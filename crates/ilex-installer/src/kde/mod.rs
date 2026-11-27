//! The KDE edition's interface: Qt 6 Qt Quick with Kirigami 6 (Breeze style,
//! KDE HIG). The pages are QML (`qml/`); what they show and what they ask for
//! comes from Rust QObjects (cxx-qt, [`installer`]) over `ilex-installer-flow`,
//! which holds every rule, as it does for the GNOME edition's GTK interface.
//!
//! This is the live image's first Kirigami page: it shows the edition's welcome
//! from the edition record and prints the same marker lines the GNOME interface
//! prints for its first page, so the harness waits on both the same way. The
//! installation's pages come next, over the same flow.
//!
//! It runs as the unprivileged `ilex-installer` user under KWin (see
//! `installer/next/editions/kde/`): KWin as the compositor with this window as
//! its only client, no Plasma shell, panel or other program.

mod installer;
mod platform;

use anyhow::{Context, Result};
use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QQuickStyle, QString, QUrl};
use ilex_installer_flow::flow::{Flow, MarkerConfig};
use ilex_installer_flow::keyboard::{Layouts, Sources};

use crate::identity::Edition;

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

/// What `main` decided.
pub struct Launch {
    pub fullscreen: bool,
    /// A fixture scenario instead of the helper: marks the markers as
    /// simulated (the first page does not talk to the helper).
    pub simulate: Option<String>,
    /// The image's edition record.
    pub edition: Option<Edition>,
}

pub fn run(launch: Launch) -> Result<()> {
    let (edition, display_name) = match &launch.edition {
        Some(record) => (record.edition.clone(), record.display_name.clone()),
        None => {
            eprintln!("ilex-installer: no edition record; a development run");
            ("kde".to_owned(), UNNAMED.to_owned())
        }
    };
    let layouts = Layouts::load(&Sources::default()).context("the keyboard layouts")?;
    // The flow decides which marker lines exist and when; the page prints what
    // it is handed.
    let flow = Flow::new(layouts).with_markers(MarkerConfig::session(
        edition.clone(),
        launch.simulate.is_some(),
    ));
    installer::hand_over(installer::Handover {
        edition,
        display_name,
        fullscreen: launch.fullscreen,
        flow,
    });

    let mut app = QGuiApplication::new();
    QGuiApplication::set_desktop_file_name(&QString::from("org.ilex.Installer"));
    // Before the engine registers the Controls types: the style cannot change
    // afterwards.
    if std::env::var_os("QT_QUICK_CONTROLS_STYLE").is_none() {
        QQuickStyle::set_style(&QString::from(STYLE));
    }
    platform::set_icon_theme(ICON_THEME);

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
