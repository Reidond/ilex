//! The GNOME edition's interface: GTK 4 + libadwaita through relm4, rendering
//! `ilex-installer-flow`.

use std::sync::Arc;

use anyhow::{Context, Result};
use ilex_installer_flow::backend::{Backend, DbusBackend};
use ilex_installer_flow::flow::{Flow, MarkerConfig};
use ilex_installer_flow::keyboard::{Layouts, SessionKeyboard, Sources};
use ilex_installer_flow::simulate::Simulated;
use relm4::prelude::*;

pub mod app;
pub mod kiosk;
pub mod render;

const APP_ID: &str = "org.ilex.Installer";

/// What `main` decided.
pub struct Launch {
    pub fullscreen: bool,
    /// A fixture scenario instead of the helper.
    pub simulate: Option<String>,
    /// The edition identifier from the image's record, for the markers.
    pub edition: Option<String>,
}

pub fn run(launch: Launch) -> Result<()> {
    let backend: Arc<dyn Backend> = match &launch.simulate {
        Some(name) => Arc::new(Simulated::load(name)?),
        None => Arc::new(DbusBackend::new()),
    };
    let layouts = Layouts::load(&Sources::default()).context("the keyboard layouts")?;
    let keyboard: Arc<dyn SessionKeyboard + Send + Sync> = Arc::new(kiosk::Kiosk);
    // The flow decides what the session prints and when, and whether the
    // layout test field's text may be (only for a test: the fixed credential,
    // or a simulated session where no real person types).
    let flow = Flow::new(layouts).with_markers(MarkerConfig::session(
        launch.edition.unwrap_or_else(|| "gnome".to_owned()),
        launch.simulate.is_some(),
    ));
    let init = app::Init {
        backend,
        keyboard,
        flow,
        fullscreen: launch.fullscreen,
        stats: true,
        observer: None,
    };
    // The command line is ours, not GTK's.
    RelmApp::new(APP_ID)
        .with_args(Vec::new())
        .run::<app::App>(init);
    Ok(())
}
