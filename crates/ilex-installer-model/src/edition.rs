//! Editions: an installer ISO installs one shell, as Fedora ships Silverblue
//! and Kinoite. Edition names are not chosen yet, so the edition identifier is
//! the shell's name. What a person reads as its name lives in one place only,
//! `installer/next/editions.toml`, which the ISO build writes into the image as
//! `edition.json` (an [`EditionRecord`]).

use crate::catalogue::{Composition, Machine, Shell, Table, is_name};
use crate::detect::{Detection, Identity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The edition record of the live image (`/usr/share/ilex-installer/edition.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditionRecord {
    pub schema: u32,
    /// The edition identifier: the shell this ISO installs.
    pub edition: String,
    #[serde(rename = "display-name")]
    pub display_name: String,
}

impl EditionRecord {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes).context("reading the edition record")?;
        ensure!(
            record.schema == 1
                && is_name(&record.edition)
                && !record.display_name.is_empty()
                && !record.display_name.chars().any(char::is_control),
            "not a schema 1 edition record"
        );
        Ok(record)
    }

    pub fn edition(&self) -> Edition {
        Edition {
            shell: self.edition.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edition {
    /// The shell this ISO installs: also the edition identifier.
    pub shell: String,
}

impl Edition {
    pub fn new(shell: &str) -> Result<Self> {
        ensure!(is_name(shell), "invalid edition {shell:?}");
        Ok(Self {
            shell: shell.into(),
        })
    }

    /// The edition of a table written for the Anaconda path: the default
    /// shell of its default composition (the released ISO is the GNOME edition).
    pub fn of_default(table: &Table) -> Result<Self> {
        let default = table
            .composition(&table.default)
            .context("default composition is not offered")?;
        Self::new(default.shells.first().context("default has no shells")?)
    }

    pub fn id(&self) -> &str {
        &self.shell
    }

    /// The shell's catalogue label: what is shown when no edition record gives
    /// the edition's own name.
    pub fn fallback_name(&self, table: &Table) -> String {
        table
            .shells
            .get(&self.shell)
            .map_or_else(|| self.shell.clone(), |s| s.label.clone())
    }
}

impl Table {
    /// The compositions whose default (first) shell is the edition's shell,
    /// in table order: the candidates for the edition.
    fn first_shell_matches(&self, edition: &Edition) -> Vec<&Composition> {
        self.compositions
            .iter()
            .filter(|c| c.shells.first() == Some(&edition.shell))
            .collect()
    }

    /// What an edition installs on a machine nothing matches: the table's own
    /// default when it belongs to the edition, otherwise the first generic
    /// composition of the edition.
    pub fn edition_default(&self, edition: &Edition) -> Result<&Composition> {
        let offered = self.first_shell_matches(edition);
        offered
            .iter()
            .find(|c| c.tag == self.default)
            .or_else(|| offered.iter().find(|c| c.machine == "generic"))
            .copied()
            .with_context(|| {
                format!(
                    "edition {} has no generic composition; the catalogue offers: {}",
                    edition.shell,
                    self.offered()
                )
            })
    }

    /// The compositions the edition installs, in table order: those with
    /// exactly the shell list of the edition's default. A composition that
    /// merely starts with the edition's shell (`generic.gnome.ilex` for the
    /// GNOME edition) is another edition's, not an option of this one.
    pub fn edition_compositions(&self, edition: &Edition) -> Result<Vec<&Composition>> {
        let shells = &self.edition_default(edition)?.shells;
        Ok(self
            .first_shell_matches(edition)
            .into_iter()
            .filter(|c| &c.shells == shells)
            .collect())
    }

    /// The catalogue as this edition's installer sees it: only the edition's
    /// compositions, their machines and shells, and its own default. The
    /// result is itself a valid table, so the plan checks run on it unchanged.
    /// Detection goes through [`Table::detect_for_edition`] on the whole
    /// catalogue instead, so that machines of other editions still count.
    pub fn for_edition(&self, edition: &Edition) -> Result<Table> {
        let default = self.edition_default(edition)?.clone();
        let compositions: Vec<Composition> = self
            .edition_compositions(edition)?
            .into_iter()
            .cloned()
            .collect();
        let machines: BTreeMap<String, Machine> = self
            .machines
            .iter()
            .filter(|(name, _)| compositions.iter().any(|c| &c.machine == *name))
            .map(|(name, machine)| (name.clone(), machine.clone()))
            .collect();
        let used: BTreeSet<&String> = compositions.iter().flat_map(|c| &c.shells).collect();
        let shells: BTreeMap<String, Shell> = self
            .shells
            .iter()
            .filter(|(name, _)| used.contains(name))
            .map(|(name, shell)| (name.clone(), shell.clone()))
            .collect();
        let table = Table {
            schema: self.schema,
            repository: self.repository.clone(),
            default: default.tag,
            testing_tag: self.testing_tag.clone(),
            machines,
            shells,
            compositions,
            profiles: self.profiles.clone(),
            security_capabilities: self.security_capabilities.clone(),
        };
        table.validate()?;
        Ok(table)
    }

    /// The shells list the edition installs on any machine: the default's.
    /// An edition has no desktop choice; a machine gets this list or nothing.
    pub fn edition_shells(&self, edition: &Edition) -> Result<Vec<String>> {
        Ok(self.edition_default(edition)?.shells.clone())
    }

    /// Detect this computer for `edition`, on the whole catalogue (`self`), as
    /// the Anaconda chooser does: every machine's rules take part, so two
    /// machines matching the same computer are a conflict even when one has
    /// no composition in this edition, and the conflict is named. The result
    /// is then mapped into the edition's own table: the one composition of the
    /// detected machine, or the edition's default with the reason it is not
    /// the machine's.
    pub fn detect_for_edition(&self, edition: &Edition, identity: Identity) -> Result<Detection> {
        let view = self.for_edition(edition)?;
        let default = view
            .composition(&view.default)
            .context("default composition is not offered")?;
        let matches = self.matching_machines(&identity);
        let (selected, reason) = match matches.as_slice() {
            [] => (
                default,
                "no machine configuration matches this computer".to_string(),
            ),
            [machine] => match view.compositions.iter().find(|c| &c.machine == machine) {
                Some(composition) => (composition, "detected".to_string()),
                None => (
                    default,
                    format!(
                        "detected machine {machine} has no composition in the {} edition",
                        edition.shell
                    ),
                ),
            },
            several => (
                default,
                format!(
                    "matches more than one machine configuration: {}",
                    several.join(", ")
                ),
            ),
        };
        Ok(Detection {
            tag: selected.tag.clone(),
            reason,
            identity,
            matches,
        })
    }
}
