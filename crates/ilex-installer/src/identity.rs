//! What the placeholder page shows: the edition (written into the image by
//! `ilexctl iso --next` from `installer/next/editions.toml`) and the machine's
//! DMI identity.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// Where the image build puts the edition.
pub const EDITION_FILE: &str = "/usr/share/ilex-installer/edition.json";
const DMI_DIR: &str = "/sys/class/dmi/id";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edition {
    pub schema: u32,
    /// Stable identifier (`gnome`): names the image stage and its files.
    pub edition: String,
    /// What a person reads. It lives in `installer/next/editions.toml` only.
    #[serde(rename = "display-name")]
    pub display_name: String,
}

/// The four world-readable DMI strings the machine match rules use.
#[derive(Debug, Serialize)]
pub struct Machine {
    pub sys_vendor: String,
    pub product_name: String,
    pub board_vendor: String,
    pub board_name: String,
}

#[derive(Debug, Serialize)]
pub struct Page {
    pub edition: Edition,
    pub machine: Machine,
}

impl Page {
    pub fn read() -> Result<Self> {
        Ok(Self {
            edition: read_edition(Path::new(EDITION_FILE))?,
            machine: Machine::read(Path::new(DMI_DIR)),
        })
    }
}

fn read_edition(path: &Path) -> Result<Edition> {
    let edition: Edition = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    if edition.schema != 1 || edition.edition.is_empty() || edition.display_name.is_empty() {
        bail!("{} is not a schema 1 edition record", path.display());
    }
    Ok(edition)
}

/// What the page says for a DMI string the firmware does not report.
const UNREPORTED: &str = "unknown";

impl Machine {
    /// The vendor and model of the whole system, as the firmware reports them.
    #[cfg(feature = "gui")]
    pub fn computer(&self) -> String {
        describe(&self.sys_vendor, &self.product_name)
    }

    /// The vendor and model of the board.
    #[cfg(feature = "gui")]
    pub fn board(&self) -> String {
        describe(&self.board_vendor, &self.board_name)
    }

    fn read(dir: &Path) -> Self {
        let value = |name: &str| {
            fs::read_to_string(dir.join(name))
                .map(|text| text.trim().to_owned())
                .ok()
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| UNREPORTED.into())
        };
        Self {
            sys_vendor: value("sys_vendor"),
            product_name: value("product_name"),
            board_vendor: value("board_vendor"),
            board_name: value("board_name"),
        }
    }
}

/// "Vendor Model", or one word when the firmware reports neither.
#[cfg(feature = "gui")]
fn describe(vendor: &str, model: &str) -> String {
    match (vendor == UNREPORTED, model == UNREPORTED) {
        (true, true) => "Not reported by the firmware".into(),
        (true, false) => model.into(),
        (false, true) => vendor.into(),
        (false, false) => format!("{vendor} {model}"),
    }
}
