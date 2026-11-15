//! Loose manifest access for host tooling. `build/compose.py` owns the schema.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

/// The image's composition, also recorded in its five configuration labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub shells: Vec<String>,
    pub default_shell: Option<String>,
    pub design_systems: Vec<String>,
    pub default_design_system: Option<String>,
    pub machine: String,
}

pub const LABELS: [&str; 5] = [
    "ilex.shells",
    "ilex.default-shell",
    "ilex.design-systems",
    "ilex.default-design-system",
    "ilex.machine",
];

impl Selection {
    pub fn read(root: &Path) -> Result<Self> {
        let image = manifest(&root.join("image.toml"))?;
        Ok(Self {
            shells: string_list(&image, "shells")?,
            default_shell: optional_string(&image, "default-shell")?,
            design_systems: string_list(&image, "design-systems")?,
            default_design_system: optional_string(&image, "default-design-system")?,
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
                self.design_systems.join(","),
                self.default_design_system.clone().unwrap_or_default(),
                self.machine.clone(),
            ])
            .collect()
    }

    /// Diagnostics can describe a manifest before compose has validated it.
    pub fn summary(&self) -> String {
        format!(
            "shells=[{}] default-shell={} design-systems=[{}] default-design-system={} machine={}",
            self.shells.join(","),
            self.default_shell.as_deref().unwrap_or("none"),
            self.design_systems.join(","),
            self.default_design_system.as_deref().unwrap_or("none"),
            self.machine
        ).escape_debug().to_string()
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
                    available.iter()
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
            design_systems: list("ilex.design-systems")?,
            default_design_system: optional("ilex.default-design-system")?,
            machine: get("ilex.machine")?.clone(),
        };
        validate_name(&selected.machine)?;
        if selected.shells.iter().any(|name| name == "none") {
            bail!("ilex.shells: `none` is a selection, not a shell name");
        }
        for (key, default, names) in [
            (
                "ilex.default-shell",
                &selected.default_shell,
                &selected.shells,
            ),
            (
                "ilex.default-design-system",
                &selected.default_design_system,
                &selected.design_systems,
            ),
        ] {
            if !match default {
                Some(name) => names.contains(name),
                None => names.is_empty(),
            } {
                bail!("{key} must name a member of its non-empty list");
            }
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
    if ["latest", "candidate", "main", "rechunked", "upgrade", "base"].contains(&name)
        || name.starts_with("sha") || name.starts_with("wt-")
    {
        bail!("machine name `{name}` is reserved");
    }
    Ok(())
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
