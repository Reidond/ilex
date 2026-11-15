//! Selection shared by build, test, vm and disk.

use std::collections::BTreeMap;
use std::process::Command;

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd, config::Selection, workspace::check_tag_length};

#[derive(clap::Args, Clone, Debug, Default)]
#[group(id = "selection")]
pub struct Args {
    /// Shell to install (repeatable; first is default). `none` is headless.
    /// Defaults to ILEX_SHELLS (comma-separated), then image.toml.
    #[arg(long = "shell")]
    pub shells: Vec<String>,
    /// Machine configuration (defaults to ILEX_MACHINE, then generic).
    #[arg(long)]
    pub machine: Option<String>,
}

pub struct Resolved {
    pub image: String,
    /// This checkout's own tag, even when `image` is an explicit reference.
    pub implicit_image: String,
    pub base_image: String,
    pub machine: String,
    // A malformed default manifest still goes to compose for its authoritative
    // schema diagnostic. Explicit selection must be resolved before podman.
    pub selection: Option<Selection>,
    check_selection: bool,
    check_base_selection: bool,
    explicit_image: bool,
    build_command: String,
}

impl Args {
    /// An explicit build output can shorten an otherwise overlong variant.
    pub fn resolve(&self, ws: &Workspace, image: Option<&str>, output: Option<&str>) -> Result<Resolved> {
        let image_is_explicit = image.is_some();
        let requested = self.requested_shells()?;
        let requested_machine = self.requested_machine()?;
        // Explicit image references without selection retain the CI contract:
        // use the named image, without comparing it with this checkout.
        let check_base_selection = image.is_none() || requested.is_some();
        let check_selection = check_base_selection || requested_machine.is_some();
        let mut build_command = "cargo xtask build".to_string();
        if let Some(shells) = &requested {
            if shells.is_empty() {
                build_command.push_str(" --shell none");
            } else {
                for shell in shells {
                    build_command.push_str(" --shell ");
                    build_command.push_str(shell);
                }
            }
        }
        if let Some(machine) = &requested_machine {
            build_command.push_str(" --machine ");
            build_command.push_str(machine);
        }
        let defaults = Selection::read(&ws.root);
        let (selection, variant) = match requested {
            Some(shells) => {
                let defaults = defaults.context("reading image.toml for shell selection")?;
                let mut selection = defaults.clone();
                selection.default_shell = shells.first().cloned();
                selection.shells = shells;
                let variant = selection.shells != defaults.shells
                    || selection.default_shell != defaults.default_shell;
                (Some(selection), variant)
            }
            None => (defaults.ok(), false),
        };
        let machine = requested_machine.unwrap_or_else(|| "generic".into());
        ilex_devkit::config::validate_machine_name(&machine)?;
        let available = ilex_devkit::config::available(&ws.root, "machines")?;
        if !available.contains(&machine) {
            bail!("unknown machine `{machine}`; available machines: {}",
                available.iter().map(|name| format!("{name:?}")).collect::<Vec<_>>().join(", "));
        }
        let selection = selection.map(|mut selection| {
            selection.machine = machine.clone();
            selection.without_shell_designs();
            selection
        });
        if let Some(selection) = &selection {
            selection.validate_shells(&ws.root)?;
        }
        // An explicit tag/reference keeps its existing meaning. Only implicit
        // checkout tags acquire shell suffixes; never append them twice when
        // test hands a resolved reference to build, or vm hands one to disk.
        let implicit_base = ws.variant_ref(
            selection
                .as_ref()
                .filter(|_| variant)
                .map(|s| s.shells.as_slice()),
        );
        let implicit_image = if machine == "generic" { implicit_base.clone() } else {
            format!("{implicit_base}.m-{machine}")
        };
        let base_image = image.map(|image| ws.resolve_image(Some(image))).unwrap_or(implicit_base);
        let image = image
            .map(|image| ws.resolve_image(Some(image)))
            .unwrap_or_else(|| implicit_image.clone());
        check_tag_length(&output.map(|tag| ws.resolve_image(Some(tag))).unwrap_or_else(|| image.clone()))?;
        Ok(Resolved {
            image,
            implicit_image,
            base_image,
            machine,
            selection,
            check_selection,
            check_base_selection,
            explicit_image: image_is_explicit,
            build_command,
        })
    }

    pub fn requested_machine(&self) -> Result<Option<String>> {
        if let Some(name) = &self.machine { return Ok(Some(name.clone())); }
        match std::env::var("ILEX_MACHINE") {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error).context("reading ILEX_MACHINE"),
        }
    }

    fn requested_shells(&self) -> Result<Option<Vec<String>>> {
        let shells = if !self.shells.is_empty() {
            self.shells.clone()
        } else {
            match std::env::var("ILEX_SHELLS") {
                Ok(value) if !value.is_empty() => value.split(',').map(str::to_owned).collect(),
                Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
                Err(error) => return Err(error).context("reading ILEX_SHELLS"),
            }
        };
        if shells.iter().any(|s| s == "none") {
            if shells.len() != 1 {
                bail!("--shell / ILEX_SHELLS: `none` must be used alone");
            }
            return Ok(Some(vec![]));
        }
        Ok(Some(shells))
    }
}

impl Resolved {
    /// A stale or unlabelled image is rebuilt only when the caller owns the
    /// implicit tag. Explicit references are never silently recomposed.
    pub fn mismatch(&self) -> Result<Option<String>> {
        self.mismatch_for(&self.image, false)
    }

    fn mismatch_for(&self, image: &str, base: bool) -> Result<Option<String>> {
        if !(if base { self.check_base_selection } else { self.check_selection }) {
            return Ok(None);
        }
        if !crate::build::image_exists(image) {
            let hint = if self.explicit_image {
                format!("build or pull {image} first")
            } else {
                format!("run `{}` first", self.build_command)
            };
            return Ok(Some(format!(
                "image {image} not found in local container storage; {hint}",
            )));
        }
        let requested = self.selection.as_ref().context(
            "cannot read the requested selection from image.toml; run `cargo xtask build` for manifest diagnostics",
        )?;
        let actual = (|| -> Result<Selection> {
            let text = cmd::output(Command::new("podman").args([
                "image",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                image,
            ]))?;
            let labels: Option<BTreeMap<String, String>> =
                serde_json::from_str(&text).context("reading image configuration labels")?;
            Selection::from_labels(&labels.unwrap_or_default())
        })();
        let built_for = match &actual {
            Ok(actual) => actual.summary(),
            Err(error) => format!("an unknown configuration ({error:#})"),
        };
        if actual.is_ok_and(|mut actual| {
            if base { actual.machine = requested.machine.clone(); }
            actual == *requested
        }) {
            return Ok(None);
        }
        let hint = if self.explicit_image {
            format!("build or pull {image} for the requested configuration")
        } else {
            format!("run `{}` and use {}", self.build_command, self.implicit_image)
        };
        Ok(Some(format!(
            "image {image} was built for {built_for}; requested {}; {hint}",
            requested.summary(),
        )))
    }

    /// A layer changes the machine, while preserving its base's composition.
    pub fn require_base_match(&self) -> Result<()> {
        if let Some(message) = self.mismatch_for(&self.base_image, true)? {
            bail!("{message}");
        }
        Ok(())
    }

    pub fn require_match(&self) -> Result<()> {
        if let Some(message) = self.mismatch()? {
            bail!("{message}");
        }
        Ok(())
    }

}
