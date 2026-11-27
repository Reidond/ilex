//! `cargo ilexctl test [--shell NAME] [--build] [filters...]`: build if needed, then
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

#[derive(clap::Args, Debug, serde::Serialize, serde::Deserialize)]
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
    // The next installer's live ISO and live image carry no OS image:
    // `iso::next…` and `installer::` alone need neither the checkout's image
    // nor a build of it.
    let installer_only = !args.filters.is_empty()
        && args
            .filters
            .iter()
            .all(|f| crate::iso_next::is_next_filter(f) || f.contains("installer::"));
    let image = if installer_only {
        None
    } else {
        Some(image_under_test(ws, &args)?)
    };
    eprintln!(
        "ilex: testing {} from {} (slug {})",
        image.as_deref().unwrap_or("the live installer"),
        ws.root.display(),
        ws.slug
    );

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut test = Command::new(cargo);
    test.current_dir(&ws.root)
        .args(["test", "--package", "ilex-e2e", "--test", "e2e", "--"])
        .args(&args.filters)
        .args(&args.test_args)
        .env("ILEX_ROOT", &ws.root);
    if let Some(image) = &image {
        test.env("ILEX_IMAGE", image);
    }
    // `iso::` exercises the installer and its published payload, independently of this image.
    // `iso::next…` boots the next installer's live ISO instead, which has no payload.
    if args.filters.iter().any(|f| {
        (f.contains("iso") && !crate::iso_next::is_next_filter(f))
            || f.contains("encrypted_")
            || f.contains("installer_profile_setup")
            || f.contains("installer_no_profile")
            || f.contains("security::upgrade")
    }) {
        prepare_iso(ws, &mut test)?;
    }
    // Each edition's ISO is built only when a filter asks for its trials.
    if args
        .filters
        .iter()
        .any(|f| crate::iso_next::is_next_filter(f) && !crate::iso_next::is_kde_filter(f))
    {
        prepare_iso_next(
            ws,
            &mut test,
            crate::iso_next::DEFAULT_EDITION,
            "ILEX_ISO_NEXT",
        )?;
    }
    if args
        .filters
        .iter()
        .any(|f| crate::iso_next::is_kde_filter(f))
    {
        prepare_iso_next(
            ws,
            &mut test,
            crate::iso_next::KDE_EDITION,
            "ILEX_ISO_NEXT_KDE",
        )?;
    }
    if args.filters.iter().any(|f| f.contains("installer::")) {
        prepare_installer_image(ws, &mut test)?;
    }
    cmd::run(&mut test)
}

/// Build the next installer's live image (unless `ILEX_INSTALLER_IMAGE` names
/// one) for `installer::`: the root helper's tier boots it without an ISO.
fn prepare_installer_image(ws: &Workspace, test: &mut Command) -> Result<()> {
    if std::env::var_os("ILEX_INSTALLER_IMAGE").is_some_and(|p| !p.is_empty()) {
        return Ok(());
    }
    let image = crate::iso_next::build_image(
        ws,
        &crate::iso_next::Build {
            edition: crate::iso_next::DEFAULT_EDITION,
            output_dir: None,
        },
    )?;
    test.env("ILEX_INSTALLER_IMAGE", image);
    Ok(())
}

/// Build an edition's live ISO of the next installer (unless `variable`,
/// `ILEX_ISO_NEXT` for the GNOME edition and `ILEX_ISO_NEXT_KDE` for the KDE
/// one, names one already) for its `iso::next…` trials.
fn prepare_iso_next(
    ws: &Workspace,
    test: &mut Command,
    edition: &str,
    variable: &str,
) -> Result<()> {
    if std::env::var_os(variable).is_some_and(|p| !p.is_empty()) {
        return Ok(());
    }
    let iso = crate::iso_next::build(
        ws,
        &crate::iso_next::Build {
            edition,
            output_dir: None,
        },
    )?;
    test.env(variable, iso);
    Ok(())
}

/// Build the installer ISO (unless `ILEX_ISO` names one already) and tell the
/// harness what it is and what the installed machine must track. The
/// installer carries no image: it installs the published installer composition, so what
/// `iso::` checks is the installer and that image, not the one under test.
fn prepare_iso(ws: &Workspace, test: &mut Command) -> Result<()> {
    if std::env::var_os("ILEX_ISO").is_some_and(|p| !p.is_empty()) {
        return Ok(());
    }
    let iso = crate::iso::build(
        ws,
        &crate::iso::Build {
            testing_tag: None,
            output_dir: None,
        },
    )?;
    test.env("ILEX_ISO", iso);
    Ok(())
}

/// The image to test, building it first when needed.
fn image_under_test(ws: &Workspace, args: &Args) -> Result<String> {
    let requested = std::env::var("ILEX_IMAGE").ok().filter(|s| !s.is_empty());
    image_under_test_with_reference(ws, args, requested)
}

pub(crate) fn image_under_test_with_reference(
    ws: &Workspace,
    args: &Args,
    requested: Option<String>,
) -> Result<String> {
    let resolved = args.selection.resolve(ws, requested.as_deref(), None)?;
    let machine_layer = args
        .selection
        .requested_machine()?
        .as_deref()
        .is_some_and(|m| m != "generic");
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
                tag: if machine_layer {
                    None
                } else {
                    requested.clone()
                },
                on: if machine_layer {
                    requested.clone()
                } else {
                    None
                },
                no_pull: args.no_pull,
                ..build::Args::default()
            },
            resolved,
        );
    }
    Ok(image)
}
