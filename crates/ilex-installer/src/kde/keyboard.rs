//! KWin's keyboard layouts, which the layout page changes. KWin 6.8 reads its
//! layouts from `kxkbrc` (the file System Settings writes) when it starts, and
//! `org.kde.keyboard`'s `/Layouts` says which it has (`getLayoutsList`,
//! `getLayout`). Re-reading `kxkbrc` in a running session is Plasma's keyboard
//! daemon's job (kded's `keyboard` module owns `org.kde.keyboard` there and
//! sends KWin `reloadConfig`); this session has none, so KWin itself owns the
//! name and nothing it would listen to can reach it. Measured on KWin 6.8.3
//! with `--virtual`: after `kxkbrc` is rewritten, the `reloadConfig` signal, a
//! KConfig change notification (`org.kde.kconfig.notify`), `kwriteconfig6` and
//! `org.kde.KWin /KWin reconfigure` each leave the list as it was.
//!
//! So [`Kwin::switch`] succeeds when KWin already has the layout (the image's
//! own, US), and otherwise says so: the layout is written to `kxkbrc` (a KWin
//! that starts again, as it does after a crash, has it) and the page shows that
//! the session did not take it, with Try again, as it does for any compositor
//! that does not. Changing layouts in a running KDE session needs the daemon or
//! a compositor that can be restarted around the page: not part of this change.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use ilex_installer_flow::keyboard::{Layout, SessionKeyboard};
use zbus::{Connection, Proxy};

const NAME: &str = "org.kde.keyboard";
const PATH: &str = "/Layouts";
const INTERFACE: &str = "org.kde.KeyboardLayouts";

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
        let have = zbus::block_on(current_layout(&connection)).context("asking KWin")?;
        if have.matches(layout) {
            return Ok(());
        }
        // For a KWin that starts again, and for the next person to look.
        let dir = config_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let temporary = dir.join(".kxkbrc.ilex");
        fs::write(&temporary, kxkbrc(layout)).context("writing kxkbrc")?;
        fs::rename(&temporary, dir.join("kxkbrc")).context("replacing kxkbrc")?;
        bail!(
            "KWin has the layout {:?} and reads its layouts when it starts: it did not take {}",
            have.long,
            layout.xkb_layout
        )
    }
}

/// What KWin says about its current layout.
struct Current {
    short: String,
    long: String,
}

impl Current {
    /// Is this the layout asked for? KWin names the default layout with an
    /// empty short name and its long name (`English (US)`).
    fn matches(&self, layout: &Layout) -> bool {
        if self.short.is_empty() {
            layout.xkb_layout == "us" && layout.xkb_variant.is_none()
        } else {
            self.short == layout.xkb_layout
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
