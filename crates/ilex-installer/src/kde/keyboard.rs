//! KWin's keyboard layouts, which the layout page changes. KWin 6.8 owns
//! `org.kde.keyboard` itself in this session (there is no `kded`): `/Layouts`
//! says which layouts it has (`getLayoutsList`, `getLayout`) and it reads them
//! from `kxkbrc`, the file System Settings writes.
//!
//! A running KWin re-reads `kxkbrc` on one signal: its `KeyboardLayout` holds a
//! `KConfigWatcher` for the file, which listens on the **session bus** for
//! `org.kde.kconfig.notify.ConfigChanged` on the path `/kxkbrc` (signature
//! `a{saay}`: group name to the list of changed key names, as byte arrays). A
//! `Layout` group in it makes KWin re-read `kxkbrc`, build the keymap again from
//! `LayoutList`, `VariantList` and `Options` (`ResetOldOptions` is honoured) and
//! emit `layoutListChanged` (`keyboard_layout.cpp` `handleXkbConfigChanged`,
//! `Xkb::reconfigure`, KWin 6.8). Nothing else does: there is no listener for
//! `org.kde.keyboard`'s `reloadConfig` on `/Layouts` (that is `/KWin`'s, which
//! reloads the whole workspace), and the slot only fires for exactly `a{saay}`,
//! on this user's session bus (`a{sas}`, root's bus and `kwriteconfig6` without
//! `--notify` are dropped silently).
//!
//! So [`Kwin::switch`] writes `kxkbrc` atomically, sends that signal from the
//! installer's own session-bus connection and asks `getLayoutsList` until KWin
//! says it has the layout. A layout KWin does not take in that time is an error
//! (the page shows it, with Try again), as for any compositor that refuses.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_installer_flow::keyboard::{Layout, SessionKeyboard};
use zbus::{Connection, Proxy};

const NAME: &str = "org.kde.keyboard";
const PATH: &str = "/Layouts";
const INTERFACE: &str = "org.kde.KeyboardLayouts";

/// How long KWin has to take a layout after it was told about the change.
const TAKE: Duration = Duration::from_secs(10);
/// How often it is asked in that time.
const ASK: Duration = Duration::from_millis(100);

pub struct Kwin;

/// `$XDG_CONFIG_HOME` or `$HOME/.config`.
fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|d| !d.is_empty()) {
        return Ok(dir.into());
    }
    let home = std::env::var_os("HOME").context("no HOME to keep the layout in")?;
    Ok(PathBuf::from(home).join(".config"))
}

/// The `kxkbrc` that makes `layout` the only (so the current) layout.
fn kxkbrc(layout: &Layout) -> String {
    format!(
        "[Layout]\nLayoutList={}\nVariantList={}\nUse=true\nResetOldOptions=true\nOptions=\n",
        layout.xkb_layout,
        layout.xkb_variant.as_deref().unwrap_or_default(),
    )
}

impl SessionKeyboard for Kwin {
    /// Blocks on D-Bus calls: run it from a worker.
    fn switch(&self, layout: &Layout) -> Result<()> {
        let connection = zbus::block_on(Connection::session()).context("the session bus")?;
        // For KWin's reading, and for a KWin that starts again.
        let dir = config_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let temporary = dir.join(".kxkbrc.ilex");
        fs::write(&temporary, kxkbrc(layout)).context("writing kxkbrc")?;
        fs::rename(&temporary, dir.join("kxkbrc")).context("replacing kxkbrc")?;
        zbus::block_on(announce(&connection))?;
        let started = Instant::now();
        loop {
            let have = zbus::block_on(current_layout(&connection)).context("asking KWin")?;
            if have.matches(layout) {
                return Ok(());
            }
            if started.elapsed() >= TAKE {
                bail!(
                    "KWin has the layout {:?} ({:?}): it did not take {}{}",
                    have.long,
                    have.short,
                    layout.xkb_layout,
                    layout
                        .xkb_variant
                        .as_deref()
                        .map(|variant| format!("({variant})"))
                        .unwrap_or_default()
                );
            }
            std::thread::sleep(ASK);
        }
    }
}

/// What KConfig sends when `kxkbrc`'s `Layout` group changed (`KConfigWatcher`
/// in KWin listens for exactly this).
async fn announce(connection: &Connection) -> Result<()> {
    let keys: Vec<Vec<u8>> = ["LayoutList", "VariantList", "Options", "ResetOldOptions"]
        .iter()
        .map(|key| key.as_bytes().to_vec())
        .collect();
    let changed: HashMap<String, Vec<Vec<u8>>> = HashMap::from([("Layout".to_owned(), keys)]);
    connection
        .emit_signal(
            None::<&str>,
            "/kxkbrc",
            "org.kde.kconfig.notify",
            "ConfigChanged",
            &changed,
        )
        .await
        .context("telling KWin that kxkbrc changed")
}

/// What KWin says about its current layout.
struct Current {
    short: String,
    long: String,
}

impl Current {
    /// Is this the layout asked for? KWin names a layout by its code
    /// (`de`, `short`) and, from the keymap, by its description (`German (no
    /// dead keys)`, `long`: the xkeyboard-config text the catalogue's names
    /// come from, so a variant is told from its plain layout by it). The
    /// image's own default layout has an empty short name.
    fn matches(&self, layout: &Layout) -> bool {
        match &layout.xkb_variant {
            Some(_) => self.long == layout.name,
            None => self.short == layout.xkb_layout || self.long == layout.name,
        }
    }
}

async fn current_layout(connection: &Connection) -> Result<Current> {
    let proxy = Proxy::new(connection, NAME, PATH, INTERFACE)
        .await
        .context("KWin's keyboard layouts")?;
    let index: u32 = proxy.call("getLayout", &()).await.context("getLayout")?;
    let list: Vec<(String, String, String)> = proxy
        .call("getLayoutsList", &())
        .await
        .context("getLayoutsList")?;
    let (short, _, long) = list
        .get(index as usize)
        .cloned()
        .context("KWin has no layouts")?;
    Ok(Current { short, long })
}
