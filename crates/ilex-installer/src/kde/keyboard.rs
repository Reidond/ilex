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
//! So [`Kwin::switch`] reads what KWin has now, writes `kxkbrc` atomically,
//! sends that signal from the installer's own session-bus connection and asks
//! `getLayoutsList` until KWin says it has the layout. A layout KWin does not
//! take in that time is an error (the page shows it, with Try again), as for
//! any compositor that refuses. zbus waits for a reply as long as its peer
//! takes, so every D-Bus call has a deadline of its own ([`within`]): a hung
//! KWin ends the switch inside the same bound instead of leaving the page on
//! "Switching" for good.

use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_installer_flow::keyboard::{Layout, SessionKeyboard};
use zbus::{Connection, Proxy};

const NAME: &str = "org.kde.keyboard";
const PATH: &str = "/Layouts";
const INTERFACE: &str = "org.kde.KeyboardLayouts";

/// How long the whole switch may take: KWin has this long to answer every
/// call and to take the layout after it was told about the change.
const TAKE: Duration = Duration::from_secs(10);
/// How often it is asked in that time.
const ASK: Duration = Duration::from_millis(100);
/// The least a call gets to answer, even right at the end of [`TAKE`] (a
/// healthy KWin answers in milliseconds), so the last poll is not cut off.
const LEAST: Duration = Duration::from_millis(500);

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
        let deadline = Instant::now() + TAKE;
        let connection = within(deadline, "the session bus", async {
            Connection::session().await.context("the session bus")
        })?;
        // What KWin has before it is told anything: a layout that looks like
        // the requested one only by its code (`de` for `de(nodeadkeys)` as
        // for `de`) is KWin's own earlier state until this changes.
        let before = ask(&connection, deadline)?;
        // For KWin's reading, and for a KWin that starts again.
        let dir = config_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let temporary = dir.join(".kxkbrc.ilex");
        fs::write(&temporary, kxkbrc(layout)).context("writing kxkbrc")?;
        fs::rename(&temporary, dir.join("kxkbrc")).context("replacing kxkbrc")?;
        let signal = connection.clone();
        within(deadline, "the change of kxkbrc", async move {
            announce(&signal).await
        })?;
        loop {
            let have = ask(&connection, deadline)?;
            if have.took(layout, &before) {
                return Ok(());
            }
            if Instant::now() >= deadline {
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

/// Wait for `call` (a D-Bus call) until `deadline`, at least [`LEAST`]. zbus
/// has no timeout of its own, so the call runs on its own thread and a hung
/// peer costs the switch an error, not its worker: the thread ends when the
/// peer finally answers or the connection drops.
fn within<T, F>(deadline: Instant, what: &str, call: F) -> Result<T>
where
    T: Send + 'static,
    F: Future<Output = Result<T>> + Send + 'static,
{
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        // The receiver is gone when the wait ran out: nothing wants the answer.
        let _ = send.send(zbus::block_on(call));
    });
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .max(LEAST);
    match receive.recv_timeout(wait) {
        Ok(answer) => answer,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            bail!("KWin did not answer ({what}) in {} s", TAKE.as_secs())
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the call to KWin ({what}) was lost"),
    }
}

/// What KWin has now, within `deadline`.
fn ask(connection: &Connection, deadline: Instant) -> Result<Current> {
    let connection = connection.clone();
    within(deadline, "its layout", async move {
        current_layout(&connection).await.context("asking KWin")
    })
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
#[derive(Clone, Debug, PartialEq, Eq)]
struct Current {
    short: String,
    long: String,
}

impl Current {
    /// Did KWin take `layout`, as it was `before` the switch? KWin names a
    /// layout by its code (`de`, `short`) and, from the keymap, by its
    /// description (`German (no dead keys)`, `long`: the xkeyboard-config text
    /// the catalogue's names come from). The description tells a variant from
    /// its plain layout (and from another variant): `de(nodeadkeys)` and `de`
    /// both have the code `de`, so the code says nothing about which of them
    /// KWin has. A plain layout whose description is not the catalogue's is
    /// taken by its code, but only once KWin's answer is not what it was before
    /// the switch (the image's own default layout has an empty code).
    fn took(&self, layout: &Layout, before: &Current) -> bool {
        self.long == layout.name
            || (layout.xkb_variant.is_none() && self.short == layout.xkb_layout && self != before)
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
