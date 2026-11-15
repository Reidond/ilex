//! `cargo xtask test [--shell NAME] [--build] [filters...]`: build if needed, then
//! run the e2e harness against it.
//!
//! The image under test is this checkout's (`localhost/ilex:main` or
//! `localhost/ilex:wt-<name>`), built first when it doesn't exist locally or
//! with `--build`. Set `ILEX_IMAGE` to test a different image (a tag such as
//! `candidate`, or a full reference): it is used as given and never rebuilt
//! unless `--build` is passed.
//! Repeat `--shell` to choose the installed shells (first is default), or use
//! `--shell none` for the headless variant; ILEX_SHELLS is the environment default.
//! A stale implicit selection is rebuilt. Explicit references are checked only
//! when --shell or ILEX_SHELLS requests a selection; a mismatch is refused.
//!
//! Filters select tests by substring, so `ephemeral::` runs one tier and
//! several filters (`ephemeral:: upgrade::`) run the union. Everything after
//! `--` goes to the test binary untouched (`-- --test-threads 1`).

use std::process::Command;

use anyhow::{Result, bail};
use ilex_devkit::{Workspace, bcvk, cmd};

use crate::build;

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(flatten)]
    pub selection: crate::select::Args,
    /// Build the image first even if it already exists locally
    #[arg(long)]
    pub build: bool,
    /// Don't pull the base images when building
    #[arg(long)]
    pub no_pull: bool,
    /// Test name filters, e.g. `ephemeral::` or `upgrade::` (all tests when empty)
    pub filters: Vec<String>,
    /// Arguments passed to the test binary
    #[arg(last = true)]
    pub test_args: Vec<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let image = image_under_test(ws, &args)?;
    eprintln!(
        "ilex: testing {image} from {} (slug {})",
        ws.root.display(),
        ws.slug
    );

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut test = Command::new(cargo);
    test.current_dir(&ws.root)
        .args(["test", "--package", "ilex-e2e", "--test", "e2e", "--"])
        .args(&args.filters)
        .args(&args.test_args)
        .env("ILEX_IMAGE", &image)
        .env("ILEX_ROOT", &ws.root);
    // `iso::` installs from an installer ISO built from the image under test.
    if args.filters.iter().any(|f| f.contains("iso")) {
        prepare_iso(ws, &mut test)?;
    }
    cmd::run(&mut test)
}

/// Build the installer ISO (unless `ILEX_ISO` names one already) and tell the
/// harness what it is and what the installed machine must track. The
/// installer carries no image: it installs the published `:latest`, so what
/// `iso::` checks is the installer and that image, not the one under test.
fn prepare_iso(ws: &Workspace, test: &mut Command) -> Result<()> {
    if std::env::var_os("ILEX_ISO").is_some_and(|p| !p.is_empty()) {
        return Ok(());
    }
    let iso = crate::iso::build(
        ws,
        &crate::iso::Build {
            source: crate::iso::DEFAULT_SOURCE,
            target: crate::iso::DEFAULT_TARGET,
            output_dir: None,
        },
    )?;
    test.env("ILEX_ISO", iso)
        .env("ILEX_ISO_TARGET", crate::iso::DEFAULT_TARGET);
    Ok(())
}

/// The image to test, building it first when needed.
fn image_under_test(ws: &Workspace, args: &Args) -> Result<String> {
    let requested = std::env::var("ILEX_IMAGE").ok().filter(|s| !s.is_empty());
    let resolved = args.selection.resolve(ws, requested.as_deref(), None)?;
    let machine_layer = args.selection.requested_machine()?.as_deref().is_some_and(|m| m != "generic");
    let image = resolved.image.clone();
    let mut rebuild = args.build;
    if !rebuild {
        if requested.is_none() && !bcvk::image_exists(&image) {
            rebuild = true;
        } else if let Some(message) = resolved.mismatch()? {
            if requested.is_some() {
                let action = if machine_layer {
                    if ilex_devkit::machine::image_machine_label(&image)
                        .is_ok_and(|name| name.as_deref() == Some("generic"))
                    {
                        "pass --build to derive a local machine layer from a generic ILEX_IMAGE base"
                    } else {
                        "choose a generic base to build the requested machine"
                    }
                } else {
                    "pass --build to rebuild ILEX_IMAGE with this selection"
                };
                bail!(
                    "{message}; ILEX_IMAGE overrides the variant; unset ILEX_IMAGE to test it, or {action}"
                );
            }
            eprintln!("ilex: {message}; rebuilding the implicit image");
            rebuild = true;
        }
    }
    if rebuild {
        return build::build_resolved(
            ws,
            &build::Args {
                selection: args.selection.clone(),
                tag: if machine_layer { None } else { requested.clone() },
                on: if machine_layer { requested.clone() } else { None },
                no_pull: args.no_pull,
                ..build::Args::default()
            },
            resolved,
        );
    }
    Ok(image)
}
