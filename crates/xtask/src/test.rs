//! `cargo xtask test [--build] [filters...]`: build the image if needed, then
//! run the e2e harness against it.
//!
//! The image under test is this checkout's (`localhost/ilex:main` or
//! `localhost/ilex:wt-<name>`), built first when it doesn't exist locally or
//! with `--build`. Set `ILEX_IMAGE` to test a different image (a tag such as
//! `candidate`, or a full reference): it is used as given and never rebuilt
//! unless `--build` is passed.
//!
//! Filters select tests by substring, so `ephemeral::` runs one tier and
//! several filters (`ephemeral:: upgrade::`) run the union. Everything after
//! `--` goes to the test binary untouched (`-- --test-threads 1`).

use std::process::Command;

use anyhow::Result;
use ilex_devkit::{Workspace, bcvk, cmd};

use crate::build;

#[derive(clap::Args, Debug)]
pub struct Args {
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
    cmd::run(&mut test)
}

/// The image to test, building it first when needed.
fn image_under_test(ws: &Workspace, args: &Args) -> Result<String> {
    let requested = std::env::var("ILEX_IMAGE").ok().filter(|s| !s.is_empty());
    let image = ws.resolve_image(requested.as_deref());
    if args.build || (requested.is_none() && !bcvk::image_exists(&image)) {
        let tag = requested.is_some().then(|| image.clone());
        build::build(
            ws,
            &build::Args {
                tag,
                no_pull: args.no_pull,
                ..build::Args::default()
            },
        )?;
    }
    Ok(image)
}
