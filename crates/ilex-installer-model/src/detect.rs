//! Machine detection and the order in which a composition is chosen. This is a
//! port of the Anaconda chooser's `detect` and `apply` (installer/chooser/
//! ilex-installer-choice): exact, case-sensitive SMBIOS comparison; a conflict
//! falls back to the default and is named; an unknown tag is an error that
//! lists the offers; the serial line keeps its format.

use crate::catalogue::{Composition, Table, py_strip, validate_tag};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};

/// The four SMBIOS strings machines are matched on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    pub sys_vendor: String,
    pub product_name: String,
    pub board_vendor: String,
    pub board_name: String,
}

impl Identity {
    /// Read `<dir>/<field>` (normally `/sys/class/dmi/id`). A field the
    /// firmware does not provide, or that cannot be read, is empty.
    pub fn read(dir: &Path) -> Self {
        let get = |field: &str| {
            fs::read(dir.join(field))
                .map(|bytes| py_strip(&String::from_utf8_lossy(&bytes)).to_owned())
                .unwrap_or_default()
        };
        Self {
            sys_vendor: get("sys_vendor"),
            product_name: get("product_name"),
            board_vendor: get("board_vendor"),
            board_name: get("board_name"),
        }
    }

    pub fn field(&self, name: &str) -> Option<&str> {
        match name {
            "sys_vendor" => Some(&self.sys_vendor),
            "product_name" => Some(&self.product_name),
            "board_vendor" => Some(&self.board_vendor),
            "board_name" => Some(&self.board_name),
            _ => None,
        }
    }

    /// "<vendor>, <model>" for the page that says what this computer is.
    pub fn summary(&self) -> String {
        let vendor = [&self.sys_vendor, &self.board_vendor]
            .into_iter()
            .find(|v| !v.is_empty())
            .map_or("unknown vendor", String::as_str);
        let model = [&self.product_name, &self.board_name]
            .into_iter()
            .filter(|v| !v.is_empty())
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" / ");
        format!(
            "{vendor}, {}",
            if model.is_empty() {
                "unknown model"
            } else {
                &model
            }
        )
    }
}

/// What the firmware identity selects, before any override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detection {
    pub tag: String,
    pub reason: String,
    pub identity: Identity,
    /// Every machine configuration whose rules matched, sorted.
    pub matches: Vec<String>,
}

impl Table {
    /// Every machine with a rule that matches `identity`, sorted by name.
    pub fn matching_machines(&self, identity: &Identity) -> Vec<String> {
        let mut names: Vec<String> = self
            .machines
            .iter()
            .filter(|(_, machine)| {
                machine.matches.iter().any(|rule| {
                    rule.iter()
                        .all(|(field, value)| identity.field(field) == Some(value.as_str()))
                })
            })
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Detect this computer. `self` is the catalogue the installer offers (for
    /// an edition, the filtered table): its default decides the shells a
    /// recognised machine gets.
    pub fn detect(&self, identity: Identity) -> Result<Detection> {
        let matches = self.matching_machines(&identity);
        let default = self
            .composition(&self.default)
            .context("default composition is not offered")?;
        let mut selected = default;
        let mut reason = "no machine configuration matches this computer".to_string();
        if matches.len() > 1 {
            reason = format!(
                "matches more than one machine configuration: {}",
                matches.join(", ")
            );
        } else if let Some(machine) = matches.first() {
            match self
                .compositions
                .iter()
                .find(|c| &c.machine == machine && c.shells == default.shells)
            {
                Some(candidate) => {
                    selected = candidate;
                    reason = "detected".into();
                }
                None => {
                    reason =
                        "detected machine has no published composition with the default desktops"
                            .into();
                }
            }
        }
        Ok(Detection {
            tag: selected.tag.clone(),
            reason,
            identity,
            matches,
        })
    }

    /// What the machine choice offers: one composition per machine, with the
    /// default's shells, in table order.
    pub fn machine_offers(&self) -> Result<Vec<&Composition>> {
        let default = self
            .composition(&self.default)
            .context("default composition is not offered")?;
        Ok(self
            .compositions
            .iter()
            .filter(|c| c.shells == default.shells)
            .collect())
    }
}

/// The public profile a choice record names (`None`: no profile asked for).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProfileChoice {
    /// A custom local daily user, named after installation.
    Custom,
    /// A public profile of the catalogue.
    Public(String),
}

/// A composition decision and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selected {
    pub tag: String,
    pub reason: String,
    /// `ghcr.io/reidond/ilex:<tag>`: what the installed system tracks.
    pub target: String,
    pub profile: Option<ProfileChoice>,
}

impl Selected {
    /// The line the selection trials read on the serial console.
    pub fn serial_line(&self) -> String {
        format!(
            "ILEX-COMPOSITION {} ({}) installs {}",
            self.tag, self.reason, self.target
        )
    }
}

/// Choose the composition: the last `ilex.composition=<tag>` kernel argument,
/// then the choice file (a plain tag or the schema-1 JSON record), then the
/// detection. `cmdline` is `/proc/cmdline`'s text; `choice` the choice file's
/// text when it exists.
pub fn select(
    table: &Table,
    detection: &Detection,
    cmdline: &str,
    choice: Option<&str>,
) -> Result<Selected> {
    let mut chosen: Option<(String, &str)> = None;
    let mut profile = None;
    for argument in cmdline.split_whitespace() {
        if let Some(tag) = argument.strip_prefix("ilex.composition=") {
            chosen = Some((tag.to_owned(), "chosen at boot"));
        }
    }
    if chosen.is_none()
        && let Some(text) = choice
    {
        let text = py_strip(text);
        let tag = if text.starts_with('{') {
            let record: BTreeMap<String, serde_json::Value> =
                serde_json::from_str(text).context("reading the chooser's choice record")?;
            let keys: Vec<&str> = record.keys().map(String::as_str).collect();
            if record.get("schema") != Some(&serde_json::json!(1))
                || !(keys == ["composition", "schema"]
                    || keys == ["composition", "profile", "schema"])
            {
                bail!("unsupported chooser choice record");
            }
            profile = match record.get("profile") {
                None => None,
                Some(serde_json::Value::Null) => Some(ProfileChoice::Custom),
                Some(serde_json::Value::String(name)) if table.profiles.contains_key(name) => {
                    Some(ProfileChoice::Public(name.clone()))
                }
                Some(_) => bail!("chosen profile is not in the installer catalogue"),
            };
            record["composition"]
                .as_str()
                .context("chosen composition must be a string")?
                .to_owned()
        } else {
            text.to_owned()
        };
        chosen = Some((tag, "chosen in the installer"));
    }
    let (tag, reason) = match chosen {
        Some((tag, reason)) => (tag, reason.to_owned()),
        None => (detection.tag.clone(), detection.reason.clone()),
    };
    if table.composition(&tag).is_none() {
        bail!("unknown composition '{tag}'; offered: {}", table.offered());
    }
    // A tag is also a registry reference; the table has already vouched for it.
    validate_tag(table.testing_tag.as_deref().unwrap_or(&tag))?;
    Ok(Selected {
        target: table.image_reference(&tag),
        tag,
        reason,
        profile,
    })
}
