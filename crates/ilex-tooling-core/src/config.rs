//! Loose manifest access for host tooling. `build/compose.py` owns the schema.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The image's composition, also recorded in its three configuration labels.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    pub shells: Vec<String>,
    pub default_shell: Option<String>,
    pub machine: String,
}

pub const LABELS: [&str; 3] = ["ilex.shells", "ilex.default-shell", "ilex.machine"];

impl Selection {
    pub fn read(root: &Path) -> Result<Self> {
        let image = manifest(&root.join("image.toml"))?;
        Self::from_manifest(&image)
    }

    fn from_manifest(image: &toml::Table) -> Result<Self> {
        Ok(Self {
            shells: string_list(image, "shells")?,
            default_shell: optional_string(image, "default-shell")?,
            machine: "generic".into(),
        })
    }

    pub fn labels(&self) -> BTreeMap<String, String> {
        LABELS
            .into_iter()
            .map(str::to_string)
            .zip([
                self.shells.join(","),
                self.default_shell.clone().unwrap_or_default(),
                self.machine.clone(),
            ])
            .collect()
    }

    /// Diagnostics can describe a manifest before compose has validated it.
    pub fn summary(&self) -> String {
        format!(
            "shells=[{}] default-shell={} machine={}",
            self.shells.join(","),
            self.default_shell.as_deref().unwrap_or("none"),
            self.machine
        )
        .escape_debug()
        .to_string()
    }

    /// Check selection names before invoking podman. Manifest contents remain
    /// compose.py's responsibility, including a selected directory's manifest.
    pub fn validate_shells(&self, root: &Path) -> Result<()> {
        let available = available(root, "shells")?;
        for (index, name) in self.shells.iter().enumerate() {
            validate_name(name)?;
            if name == "none" || !available.contains(name) {
                bail!(
                    "unknown shell `{name}`; available shells: {} (use `none` for no shell)",
                    available
                        .iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            if self.shells[..index].contains(name) {
                bail!("shell selection contains duplicate `{name}`");
            }
        }
        Ok(())
    }

    pub fn from_labels(labels: &BTreeMap<String, String>) -> Result<Self> {
        let get = |key: &str| {
            labels.get(key).with_context(|| format!(
            "image has no ilex.* configuration labels (missing {key}; built before configurations existed, or built without configuration labels)"
        ))
        };
        let list = |key| -> Result<Vec<String>> {
            let value = get(key)?;
            let names = if value.is_empty() {
                vec![]
            } else {
                value.split(',').map(str::to_string).collect()
            };
            for name in &names {
                validate_name(name)?;
            }
            Ok(names)
        };
        let optional = |key| -> Result<Option<String>> {
            Ok(Some(get(key)?.clone()).filter(|s| !s.is_empty()))
        };
        let selected = Self {
            shells: list("ilex.shells")?,
            default_shell: optional("ilex.default-shell")?,
            machine: get("ilex.machine")?.clone(),
        };
        validate_name(&selected.machine)?;
        if selected.shells.iter().any(|name| name == "none") {
            bail!("ilex.shells: `none` is a selection, not a shell name");
        }
        if !match &selected.default_shell {
            Some(name) => selected.shells.contains(name),
            None => selected.shells.is_empty(),
        } {
            bail!("ilex.default-shell must name a member of its non-empty list");
        }
        Ok(selected)
    }

    pub fn default_target(&self) -> &'static str {
        if self.shells.is_empty() {
            "multi-user.target"
        } else {
            "graphical.target"
        }
    }

    pub fn default_shell_manifest(&self, root: &Path) -> Result<Option<toml::Table>> {
        self.default_shell
            .as_ref()
            .map(|name| manifest(&root.join("shells").join(name).join("shell.toml")))
            .transpose()
    }
}

pub fn manifest(path: &Path) -> Result<toml::Table> {
    fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?
        .parse()
        .with_context(|| format!("parsing {}", path.display()))
}

/// Registry directories only; stray files (including .gitkeep) are ignored.
pub fn available(root: &Path, directory: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root.join(directory))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

pub fn validate_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase())
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("invalid configuration name {name:?}: expected [a-z][a-z0-9-]*");
    }
    Ok(())
}

pub fn validate_machine_name(name: &str) -> Result<()> {
    validate_name(name)?;
    if [
        "latest",
        "candidate",
        "main",
        "rechunked",
        "upgrade",
        "base",
    ]
    .contains(&name)
        || name.starts_with("candidate")
        || name.starts_with("sha")
        || name.starts_with("wt-")
    {
        bail!("machine name `{name}` is reserved");
    }
    Ok(())
}

/// Validate names before using a manifest basename in a shell loop or tag.
/// Full composition validation remains in compose.py at build time.
pub fn machine_manifest(root: &Path, name: &str) -> Result<toml::Table> {
    validate_machine_name(name)?;
    let table = manifest(&root.join("machines").join(name).join("machine.toml"))?;
    if table.get("name").and_then(|v| v.as_str()) != Some(name) {
        bail!("machines/{name}/machine.toml: name must equal directory name {name:?}");
    }
    for key in table.keys() {
        if !["name", "description", "packages", "apps", "match"].contains(&key.as_str()) {
            bail!("machines/{name}/machine.toml: `{key}` is an unknown key");
        }
    }
    machine_matches(name, &table)?;
    Ok(table)
}

pub type DmiMatch = BTreeMap<String, String>;

/// Installer-only SMBIOS rules. Keep in step with build/compose.py.
pub fn machine_matches(name: &str, table: &toml::Table) -> Result<Vec<DmiMatch>> {
    let Some(value) = table.get("match") else {
        return Ok(vec![]);
    };
    if name == "generic" || name == "vm" {
        bail!(
            "machine {name}: match is forbidden (generic is the fallback; vm is never installed on hardware)"
        );
    }
    let mut rules = Vec::new();
    for value in value
        .as_array()
        .context("match must be an array of tables")?
    {
        let table = value.as_table().context("match entries must be tables")?;
        let mut rule = DmiMatch::new();
        for (key, value) in table {
            if !["sys_vendor", "product_name", "board_vendor", "board_name"].contains(&key.as_str())
            {
                bail!("machine {name}: match.{key} is an unknown key");
            }
            let value = value.as_str().context("match values must be strings")?;
            if value.is_empty()
                || value.chars().count() > 128
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                bail!(
                    "machine {name}: match.{key} must have 1–128 characters, no control characters or surrounding whitespace"
                );
            }
            if [
                "to be filled by o.e.m.",
                "default string",
                "system manufacturer",
                "system product name",
                "base board manufacturer",
                "base board product name",
                "not applicable",
                "not specified",
                "none",
                "o.e.m.",
                "type1productconfigid",
                "invalid",
            ]
            .contains(&value.to_ascii_lowercase().as_str())
            {
                bail!(
                    "machine {name}: match.{key} {value:?} is a firmware placeholder; it would match other machines"
                );
            }
            rule.insert(key.clone(), value.to_owned());
        }
        if !((rule.contains_key("sys_vendor") && rule.contains_key("product_name"))
            || (rule.contains_key("board_vendor") && rule.contains_key("board_name")))
            || !rule.len().is_multiple_of(2)
        {
            bail!(
                "machine {name}: a match table needs a vendor and a model of the same SMBIOS structure (no incomplete pairs)"
            );
        }
        rules.push(rule);
    }
    Ok(rules)
}

pub fn description(table: &toml::Table) -> Result<String> {
    let text = table
        .get("description")
        .and_then(|v| v.as_str())
        .context("description must be a string")?;
    if text.is_empty() || text.chars().count() > 120 || text.chars().any(char::is_control) {
        bail!("description must have 1–120 characters without control characters");
    }
    Ok(text.to_owned())
}

/// One image CI can publish. Shell order also determines the default shell.
#[derive(Clone, Debug)]
pub struct Publication {
    pub tag: String,
    pub kind: &'static str,
    pub parent: Option<String>,
    pub machine: String,
    pub shells: Vec<String>,
    /// Complete expected labels, resolved from this row and image.toml together.
    pub selection: Selection,
    pub description: String,
    pub matches: Vec<DmiMatch>,
    pub shell_descriptions: BTreeMap<String, String>,
}

/// A composition has a published name only when its labels are consistent.
pub fn published_tag(selection: &Selection) -> Option<String> {
    validate_machine_name(&selection.machine).ok()?;
    if selection.default_shell.as_ref() != selection.shells.first() {
        return None;
    }
    for (index, name) in selection.shells.iter().enumerate() {
        validate_name(name).ok()?;
        if name == "none" || selection.shells[..index].contains(name) {
            return None;
        }
    }
    let tag = if selection.shells.is_empty() && selection.machine == "generic" {
        "base".into()
    } else {
        std::iter::once(selection.machine.as_str())
            .chain(selection.shells.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(".")
    };
    (tag.len() + "_sha-1234567".len() <= 128).then_some(tag)
}

/// Validate everything before returning rows to a shell loop. The base is
/// implicit; all roots precede their layers, regardless of machine list order.
pub fn publications(root: &Path) -> Result<Vec<Publication>> {
    let image = manifest(&root.join("image.toml"))?;
    for key in image.keys() {
        if ![
            "shells",
            "default-shell",
            "users",
            "installer",
            "publish",
        ]
        .contains(&key.as_str())
        {
            bail!("image.toml: `{key}` is an unknown key");
        }
    }
    // Also reject obsolete keys in an unselected machine manifest.
    let mut all_rules: Vec<(String, DmiMatch)> = Vec::new();
    for name in available(root, "machines")? {
        let manifest = machine_manifest(root, &name)?;
        for rule in machine_matches(&name, &manifest)? {
            for (other, previous) in &all_rules {
                if other != &name
                    && (rule.iter().all(|(k, v)| previous.get(k) == Some(v))
                        || previous.iter().all(|(k, v)| rule.get(k) == Some(v)))
                {
                    bail!("machines {other} and {name} have identical or subset match tables");
                }
            }
            all_rules.push((name.clone(), rule));
        }
    }
    let defaults = Selection::from_manifest(&image)?;
    defaults.validate_shells(root)?;
    if !match &defaults.default_shell {
        Some(name) => defaults.shells.contains(name),
        None => defaults.shells.is_empty(),
    } {
        bail!("image.toml: default-shell must name a member of its non-empty list");
    }
    let mut rows = vec![Publication {
        tag: "base".into(),
        kind: "root",
        parent: None,
        machine: "generic".into(),
        shells: vec![],
        description: description(&machine_manifest(root, "generic")?)?,
        matches: vec![],
        shell_descriptions: BTreeMap::new(),
        selection: Selection {
            shells: vec![],
            default_shell: None,
            machine: "generic".into(),
        },
    }];
    let mut layers = Vec::new();
    let mut shell_lists = Vec::new();
    let entries = match image.get("publish") {
        None => &[][..],
        Some(value) => value
            .as_array()
            .context("image.toml: `publish` must be a list of tables")?
            .as_slice(),
    };
    for entry in entries {
        let entry = entry
            .as_table()
            .context("image.toml: `publish` entries must be tables")?;
        for key in entry.keys() {
            if !["shells", "machines"].contains(&key.as_str()) {
                bail!("image.toml: publish.{key} is an unknown key");
            }
        }
        let shells = string_list(entry, "shells")?;
        let machines = string_list(entry, "machines")?;
        if shell_lists.contains(&shells) {
            bail!("image.toml: publish duplicates an ordered shell list");
        }
        shell_lists.push(shells.clone());
        if machines.is_empty() {
            bail!("image.toml: publish.machines must not be empty");
        }
        let has_generic = machines.iter().any(|name| name == "generic");
        if shells.is_empty() == has_generic {
            bail!(
                "image.toml: publish.machines must contain generic with shells, and must not contain generic without shells (that image is base)"
            );
        }
        let mut selected = defaults.clone();
        selected.shells = shells.clone();
        selected.default_shell = shells.first().cloned();
        selected.validate_shells(root)?;
        let parent = if shells.is_empty() {
            "base".into()
        } else {
            format!("generic.{}", shells.join("."))
        };
        for (index, name) in machines.iter().enumerate() {
            machine_manifest(root, name)?;
            if machines[..index].contains(name) {
                bail!("image.toml: publish.machines contains duplicate `{name}`");
            }
            selected.machine = name.clone();
            let tag = published_tag(&selected).context("image.toml: derived publication tag is invalid or exceeds 128 bytes with its suffix")?;
            if rows
                .iter()
                .chain(layers.iter())
                .any(|row: &Publication| row.tag == tag)
            {
                bail!("image.toml: duplicate derived tag `{tag}`");
            }
            let row = Publication {
                description: description(&machine_manifest(root, name)?)?,
                matches: machine_matches(name, &machine_manifest(root, name)?)?,
                shell_descriptions: shells
                    .iter()
                    .map(|shell| {
                        Ok((
                            shell.clone(),
                            description(&manifest(
                                &root.join("shells").join(shell).join("shell.toml"),
                            )?)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
                tag,
                kind: if name == "generic" { "root" } else { "layer" },
                parent: (name != "generic").then(|| parent.clone()),
                machine: name.clone(),
                shells: shells.clone(),
                selection: selected.clone(),
            };
            if row.kind == "root" {
                rows.push(row);
            } else {
                layers.push(row);
            }
        }
    }
    rows.extend(layers);
    let installer =
        optional_string(&image, "installer")?.context("image.toml: `installer` is required")?;
    if !rows
        .iter()
        .any(|row| row.tag == installer && row.machine == "generic" && !row.shells.is_empty())
    {
        bail!(
            "image.toml: installer must name a published generic composition with at least one shell"
        );
    }
    Ok(rows)
}

pub fn installer(root: &Path) -> Result<String> {
    publications(root)?;
    optional_string(&manifest(&root.join("image.toml"))?, "installer")?
        .context("image.toml: `installer` is required")
}

pub fn string_list(table: &toml::Table, key: &str) -> Result<Vec<String>> {
    table
        .get(key)
        .and_then(|v| v.as_array())
        .with_context(|| format!("`{key}` must be a list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .with_context(|| format!("`{key}` must contain strings"))
        })
        .collect()
}

fn optional_string(table: &toml::Table, key: &str) -> Result<Option<String>> {
    table
        .get(key)
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .with_context(|| format!("`{key}` must be a string"))
        })
        .transpose()
}
