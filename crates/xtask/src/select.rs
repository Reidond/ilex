//! Selection shared by build, test, vm and disk. Machine selection joins it
//! here when derived machine images are introduced.

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, config::Selection, workspace::check_tag_length};

#[derive(clap::Args, Clone, Debug, Default)]
#[group(id = "selection")]
pub struct Args {
    /// Shell to install (repeatable; first is default). `none` is headless.
    /// Defaults to ILEX_SHELLS (comma-separated), then image.toml.
    #[arg(long = "shell")]
    pub shells: Vec<String>,
}

pub struct Resolved {
    pub image: String,
    /// This checkout's own tag, even when `image` is an explicit reference.
    pub implicit_image: String,
    // A malformed default manifest still goes to compose for its authoritative
    // schema diagnostic. Explicit selection must be resolved before podman.
    pub selection: Option<Selection>,
}

impl Args {
    pub fn resolve(&self, ws: &Workspace, image: Option<&str>) -> Result<Resolved> {
        let requested = self.requested_shells()?;
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
        if let Some(selection) = &selection {
            selection.validate_shells(&ws.root)?;
        }
        // An explicit tag/reference keeps its existing meaning. Only implicit
        // checkout tags acquire shell suffixes; never append them twice when
        // test hands a resolved reference to build, or vm hands one to disk.
        let implicit_image = ws.variant_ref(
            selection.as_ref().filter(|_| variant).map(|s| s.shells.as_slice()),
        );
        let image = image.map(|image| ws.resolve_image(Some(image)))
            .unwrap_or_else(|| implicit_image.clone());
        check_tag_length(&image)?;
        Ok(Resolved { image, implicit_image, selection })
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
    /// Layers we build belong to this checkout, never to the base reference.
    pub fn viewer_image(&self) -> Result<String> {
        let image = format!("{}-vm", self.implicit_image);
        check_tag_length(&image)?;
        Ok(image)
    }
}
