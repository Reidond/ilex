//! Keyboard layouts the installed system's console and the live session can
//! both use. The catalogue is what the live image itself ships: the console
//! keymaps converted from xkeyboard-config (`/usr/lib/kbd/keymaps/xkb/
//! <layout>[-<variant>].map.gz`) named by xkeyboard-config's own rules list
//! (`/usr/share/X11/xkb/rules/evdev.lst`). So each entry is one console keymap
//! name (what `Plan` takes as `keyboard` and the initramfs prompt will use)
//! and one XKB layout (what the session switches to), and the helper's check
//! that the keymap exists can never refuse an entry offered here.
//!
//! Applying a layout to the running session is the edition's compositor's
//! business (GNOME Kiosk's `SetInputSources`, KWin's keyboard layout D-Bus
//! interface): the flow only says which layout, through [`SessionKeyboard`].

use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Where the live image keeps what the catalogue is built from.
#[derive(Clone, Debug)]
pub struct Sources {
    /// kbd's keymap tree; its `xkb/` directory holds the converted maps.
    pub keymaps: PathBuf,
    /// xkeyboard-config's rules list: layout and variant names.
    pub rules: PathBuf,
}

impl Default for Sources {
    fn default() -> Self {
        Self {
            keymaps: "/usr/lib/kbd/keymaps".into(),
            rules: "/usr/share/X11/xkb/rules/evdev.lst".into(),
        }
    }
}

/// One keyboard layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The console keymap name: `us`, `de-nodeadkeys`, `fr-oss`.
    pub id: String,
    /// What a person reads: `English (US)`, `German (no dead keys)`.
    pub name: String,
    /// The XKB layout code: `us`, `de`, `fr`.
    pub xkb_layout: String,
    pub xkb_variant: Option<String>,
}

impl Layout {
    /// The identifier GNOME uses for the XKB source: `layout` or
    /// `layout+variant`.
    pub fn gnome_source(&self) -> String {
        match &self.xkb_variant {
            Some(variant) => format!("{}+{variant}", self.xkb_layout),
            None => self.xkb_layout.clone(),
        }
    }
}

/// The identifier of the layout selected at the start (the live image's own
/// default).
pub const DEFAULT: &str = "us";

#[derive(Clone, Debug, Default)]
pub struct Layouts {
    entries: Vec<Layout>,
}

impl Layouts {
    /// Read the live image's own files.
    pub fn load(sources: &Sources) -> Result<Self> {
        let rules = fs::read_to_string(&sources.rules)
            .with_context(|| format!("reading {}", sources.rules.display()))?;
        let dir = sources.keymaps.join("xkb");
        let mut maps = BTreeSet::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("listing {}", dir.display()))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if let Some(id) = name
                .strip_suffix(".map.gz")
                .or_else(|| name.strip_suffix(".map"))
            {
                maps.insert(id.to_owned());
            }
        }
        Self::build(&rules, &maps)
    }

    /// Build the catalogue from the text of the rules list and the names of
    /// the converted keymaps.
    pub fn build(rules: &str, keymaps: &BTreeSet<String>) -> Result<Self> {
        let names = parse_rules(rules);
        if names.layouts.is_empty() {
            bail!("the xkeyboard-config rules list names no layout");
        }
        let mut entries = Vec::new();
        for id in keymaps {
            // `<layout>` or `<layout>-<variant>`; a layout code has no hyphen.
            let (layout, variant) = match id.split_once('-') {
                Some((layout, variant)) => (layout, Some(variant)),
                None => (id.as_str(), None),
            };
            let Some(layout_name) = names.layouts.get(layout) else {
                continue;
            };
            let name = match variant {
                None => layout_name.clone(),
                Some(variant) => match names.variants.get(&(layout.to_owned(), variant.to_owned())) {
                    Some(name) => name.clone(),
                    None => continue,
                },
            };
            entries.push(Layout {
                id: id.clone(),
                name,
                xkb_layout: layout.to_owned(),
                xkb_variant: variant.map(str::to_owned),
            });
        }
        entries.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        if entries.is_empty() {
            bail!("no console keymap matches a layout of xkeyboard-config");
        }
        Ok(Self { entries })
    }

    pub fn all(&self) -> &[Layout] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&Layout> {
        self.entries.iter().find(|l| l.id == id)
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.entries.iter().position(|l| l.id == id)
    }

    /// The layout the page starts on: `us` when the image has it, else the
    /// first.
    pub fn default_id(&self) -> &str {
        self.get(DEFAULT)
            .or_else(|| self.entries.first())
            .map_or(DEFAULT, |l| l.id.as_str())
    }

    /// Does `layout` match what the person typed into the search field? Every
    /// word must be found in the name or the keymap id, in any case.
    pub fn matches(layout: &Layout, query: &str) -> bool {
        let name = layout.name.to_lowercase();
        let id = layout.id.to_lowercase();
        query
            .to_lowercase()
            .split_whitespace()
            .all(|word| name.contains(word) || id.contains(word))
    }
}

struct Names {
    layouts: BTreeMap<String, String>,
    /// `(layout, variant)` to the full name (`German (no dead keys)`).
    variants: BTreeMap<(String, String), String>,
}

/// xkeyboard-config's `evdev.lst`: `! layout` lines are `  code   Name`,
/// `! variant` lines are `  variant   layout: Full name`.
fn parse_rules(text: &str) -> Names {
    let mut names = Names {
        layouts: BTreeMap::new(),
        variants: BTreeMap::new(),
    };
    let mut section = "";
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("! ") {
            section = header.trim();
            continue;
        }
        let mut words = line.split_whitespace();
        let Some(code) = words.next() else { continue };
        let rest = line.trim_start()[code.len()..].trim();
        match section {
            "layout" => {
                names.layouts.insert(code.to_owned(), rest.to_owned());
            }
            "variant" => {
                if let Some((layout, name)) = rest.split_once(": ") {
                    names
                        .variants
                        .insert((layout.trim().to_owned(), code.to_owned()), name.trim().to_owned());
                }
            }
            _ => {}
        }
    }
    names
}

/// Make the running session type in `layout`. Each edition's interface
/// implements it with its own compositor's interface; the flow never talks to
/// a compositor.
pub trait SessionKeyboard {
    fn switch(&self, layout: &Layout) -> Result<()>;
}

/// A session with no layout switching (the self-check, simulations).
pub struct NoSessionKeyboard;

impl SessionKeyboard for NoSessionKeyboard {
    fn switch(&self, _layout: &Layout) -> Result<()> {
        Ok(())
    }
}

/// Where a catalogue's own file lists are checked: used by the self-check to
/// say which directory it read.
pub fn describe(sources: &Sources) -> String {
    format!(
        "{} and {}",
        Path::new(&sources.keymaps).join("xkb").display(),
        sources.rules.display()
    )
}
