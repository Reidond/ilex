//! GNOME Kiosk's keyboard interface, `org.gnome.Kiosk.InputSources` on the
//! session bus: `SetInputSources(a(ss) sources, as options)` makes the
//! compositor type in the given layouts at once (the first is current), so
//! the person types the installer's own fields, and later the disk passphrase,
//! in the layout they chose. The same call is what a person would get from
//! GNOME's settings; nothing here needs a polkit action.
//!
//! Kiosk applies the sources and updates its properties a moment after the
//! call returns (it defers its D-Bus sync), so [`Kiosk::switch`] reads
//! `SelectedInputSource` back and reports success only once the selected
//! source is the one asked for.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_installer_flow::keyboard::{Layout, SessionKeyboard};
use zbus::{Connection, OwnedObjectPath, Proxy};

const NAME: &str = "org.gnome.Kiosk";
const PATH: &str = "/org/gnome/Kiosk/InputSources/Manager";
const INTERFACE: &str = "org.gnome.Kiosk.InputSources";
const SOURCE_INTERFACE: &str = "org.gnome.Kiosk.InputSources.InputSource";
/// How long Kiosk is given to apply the sources.
const APPLY_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Kiosk;

impl SessionKeyboard for Kiosk {
    /// Blocks on D-Bus calls: run it from a worker.
    fn switch(&self, layout: &Layout) -> Result<()> {
        let wanted = layout.gnome_source();
        let sources = vec![("xkb".to_owned(), wanted.clone())];
        let options: Vec<String> = Vec::new();
        let connection = zbus::block_on(Connection::session()).context("the session bus")?;
        zbus::block_on(async {
            let manager = Proxy::new(&connection, NAME, PATH, INTERFACE)
                .await
                .context("GNOME Kiosk's input sources")?;
            manager
                .call::<_, _, ()>("SetInputSources", &(sources, options))
                .await
                .context("SetInputSources")
        })?;
        let started = Instant::now();
        loop {
            if zbus::block_on(selected_source(&connection))? == wanted {
                return Ok(());
            }
            if started.elapsed() > APPLY_TIMEOUT {
                bail!("GNOME Kiosk did not select {wanted} within {APPLY_TIMEOUT:?}");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// The backend id (`de+nodeadkeys`) of the source Kiosk has selected, or an
/// empty string while it has none.
async fn selected_source(connection: &Connection) -> Result<String> {
    let manager = Proxy::new(connection, NAME, PATH, INTERFACE)
        .await
        .context("GNOME Kiosk's input sources")?;
    let selected: OwnedObjectPath = manager
        .get_property("SelectedInputSource")
        .await
        .context("SelectedInputSource")?;
    if selected.as_str() == "/" {
        return Ok(String::new());
    }
    let source = Proxy::new(connection, NAME, selected.as_str(), SOURCE_INTERFACE)
        .await
        .context("the selected input source")?;
    source.get_property("BackendId").await.context("BackendId")
}
