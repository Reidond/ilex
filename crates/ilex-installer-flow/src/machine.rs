//! The machine page's content, made from the helper's probe: what this
//! computer is, which configuration was detected, what the person may choose
//! instead and what to say when detection did not give a clean answer. There
//! is no desktop choice: one ISO installs one edition's shell list, and every
//! machine of the catalogue gets exactly that.

use crate::messages::Message;
use ilex_installer_model::probe::Probe;
use ilex_installer_model::{Composition, Identity};

/// One configuration the person may pick: a machine of this edition's
/// catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineChoice {
    /// The composition tag (`ms-7c35.gnome`): what `Plan` takes.
    pub tag: String,
    pub machine: String,
    /// The part of the catalogue description before the first `: `
    /// (`MSI MEG X570 UNIFY (MS-7C35)`, `Any x86-64 machine`).
    pub title: String,
    /// The rest, capitalised; may be empty.
    pub subtitle: String,
}

impl MachineChoice {
    fn new(composition: &Composition, description: &str) -> Self {
        let (title, subtitle) = match description.split_once(": ") {
            Some((title, rest)) => (title.trim(), capitalise(rest.trim())),
            None => (description.trim(), String::new()),
        };
        Self {
            tag: composition.tag.clone(),
            machine: composition.machine.clone(),
            title: title.to_owned(),
            subtitle,
        }
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// How serious a notice is, so an interface can pick an icon or style.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// What was selected first, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Exactly one machine configuration's rules match this computer.
    Detected { machine: String },
    /// No machine configuration matches: the edition's default.
    NotRecognised,
    /// More than one matches: the default, with the conflict named.
    Conflict { machines: Vec<String> },
    /// One matches but it has no composition in this edition.
    NoComposition { machine: String },
    /// `ilex.composition=` on the kernel command line (or the choice file).
    Chosen { how: String },
    /// `ilex.composition=` named a tag this edition does not offer.
    BadOverride { error: String },
}

impl Origin {
    /// A word for serial markers and logs.
    pub fn word(&self) -> &'static str {
        match self {
            Self::Detected { .. } => "detected",
            Self::NotRecognised => "not-recognised",
            Self::Conflict { .. } => "conflict",
            Self::NoComposition { .. } => "no-composition",
            Self::Chosen { .. } => "chosen",
            Self::BadOverride { .. } => "bad-override",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub severity: Severity,
    pub message: Message,
}

#[derive(Clone, Debug)]
pub struct MachineView {
    /// "Vendor Model" as the firmware reports it.
    pub computer: String,
    /// One entry per machine of this edition, in the catalogue's order.
    pub choices: Vec<MachineChoice>,
    /// The tag preselected.
    pub preselected: String,
    pub origin: Origin,
    /// The shells every choice installs, in order (their catalogue labels).
    pub shells: Vec<String>,
}

impl MachineView {
    pub fn from_probe(probe: &Probe) -> Self {
        let table = &probe.catalogue;
        let choices: Vec<MachineChoice> = table
            .compositions
            .iter()
            .map(|composition| {
                let description = table
                    .machines
                    .get(&composition.machine)
                    .map_or(composition.machine.as_str(), |m| m.description.as_str());
                MachineChoice::new(composition, description)
            })
            .collect();
        let default_tag = table.default.clone();
        let detection = &probe.detection;
        let (preselected, origin) = if let Some(error) = &probe.selection_error {
            (
                detection.tag.clone(),
                Origin::BadOverride {
                    error: error.clone(),
                },
            )
        } else if let Some(selected) = probe
            .selection
            .as_ref()
            .filter(|s| s.reason.starts_with("chosen"))
        {
            (
                selected.tag.clone(),
                Origin::Chosen {
                    how: selected.reason.clone(),
                },
            )
        } else {
            let origin = match detection.matches.as_slice() {
                [] => Origin::NotRecognised,
                [machine] => {
                    let has_composition = table
                        .composition(&detection.tag)
                        .is_some_and(|c| &c.machine == machine);
                    if has_composition {
                        Origin::Detected {
                            machine: machine.clone(),
                        }
                    } else {
                        Origin::NoComposition {
                            machine: machine.clone(),
                        }
                    }
                }
                several => Origin::Conflict {
                    machines: several.to_vec(),
                },
            };
            (detection.tag.clone(), origin)
        };
        let shells = table
            .composition(&default_tag)
            .map(|c| {
                c.shells
                    .iter()
                    .map(|name| {
                        table
                            .shells
                            .get(name)
                            .map_or_else(|| name.clone(), |s| s.label.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            computer: describe_computer(&detection.identity),
            choices,
            preselected,
            origin,
            shells,
        }
    }

    pub fn choice(&self, tag: &str) -> Option<&MachineChoice> {
        self.choices.iter().find(|c| c.tag == tag)
    }

    pub fn index_of(&self, tag: &str) -> Option<usize> {
        self.choices.iter().position(|c| c.tag == tag)
    }

    /// What to tell the person above the list, from how the preselection came
    /// about. The wording follows the model's: a conflict names the machines,
    /// a machine without a composition here says so, an unknown boot argument
    /// repeats the model's list of offers.
    pub fn notice(&self) -> Notice {
        let title_of = |machine: &str| {
            self.choices
                .iter()
                .find(|c| c.machine == machine)
                .map_or_else(|| machine.to_owned(), |c| c.title.clone())
        };
        match &self.origin {
            Origin::Detected { machine } => Notice {
                severity: Severity::Info,
                message: Message::new(
                    format!("Recognised this computer as {}", title_of(machine)),
                    "Its own configuration is selected. Pick another if you want the generic one.",
                ),
            },
            Origin::NotRecognised => Notice {
                severity: Severity::Info,
                message: Message::new(
                    "No configuration is specific to this computer",
                    "The generic one works on any x86-64 computer and is selected.",
                ),
            },
            Origin::Conflict { machines } => Notice {
                severity: Severity::Warning,
                message: Message::new(
                    "More than one configuration matches this computer",
                    format!(
                        "{} all claim it, so the generic configuration is selected. Pick the one you want.",
                        machines.join(", ")
                    ),
                ),
            },
            Origin::NoComposition { machine } => Notice {
                severity: Severity::Warning,
                message: Message::new(
                    format!("{} has no configuration for this edition", machine),
                    "The computer is recognised, but nothing is published for it with this desktop, so the generic configuration is selected.",
                ),
            },
            Origin::Chosen { how } => Notice {
                severity: Severity::Info,
                message: chosen_before_start(how),
            },
            Origin::BadOverride { error } => Notice {
                severity: Severity::Error,
                message: Message::new(
                    "The boot options name a configuration this installer does not offer",
                    error.clone(),
                ),
            },
        }
    }
}

/// "Vendor Model" for the page that says what this computer is: the system's
/// own strings, else the board's.
pub fn describe_computer(identity: &Identity) -> String {
    let pair = |vendor: &str, model: &str| match (vendor.is_empty(), model.is_empty()) {
        (true, true) => None,
        (false, true) => Some(vendor.to_owned()),
        (true, false) => Some(model.to_owned()),
        (false, false) => Some(format!("{vendor} {model}")),
    };
    pair(&identity.sys_vendor, &identity.product_name)
        .or_else(|| pair(&identity.board_vendor, &identity.board_name))
        .unwrap_or_else(|| "A computer that does not report its model".to_owned())
}

/// What to say about a configuration that was named before the installer
/// started. The model's reason says where the name came from, and each source
/// gets its own sentence: the boot command line's `ilex.composition=`
/// (`chosen at boot`), or the choice file a chooser or a kickstart wrote
/// (`chosen in the installer`).
fn chosen_before_start(how: &str) -> Message {
    match how {
        "chosen at boot" => Message::new(
            "The boot options chose this configuration",
            "The installer started with ilex.composition= naming it. Pick another if that was not meant.",
        ),
        "chosen in the installer" => Message::new(
            "A choice made before the installer started selected this configuration",
            "A choice file prepared for this boot names it. Pick another if that was not meant.",
        ),
        other => Message::new(
            format!("{} before the installer started", capitalise(other)),
            "Pick another configuration if that was not meant.",
        ),
    }
}
