//! `cargo xtask build`: podman build tagged with the checkout's slug.
//!
//! The base tag floats (`fedora-bootc:45`), so the nightly build picks up
//! base updates. The digest it resolved to is recorded as the
//! `ilex.base-digest` label, next to the git revision.
//!
//! The builder stage compiles through kache. When this machine has a kache
//! store of its own, the build mounts it read-only at `/kache-host` so
//! crates compiled on the host (or by another build) are restored instead of
//! compiled again.

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, cmd};

#[derive(clap::Args, Debug, Default)]
pub struct Args {
    #[command(flatten)]
    pub selection: crate::select::Args,
    /// Tag (e.g. `candidate`) or full reference to build instead of this checkout's tag
    #[arg(long)]
    pub tag: Option<String>,
    /// Build only a machine layer on this existing image (tag or full reference).
    #[arg(long)]
    pub on: Option<String>,
    /// Don't pull the base images first; build from local storage
    #[arg(long)]
    pub no_pull: bool,
    /// Don't mount the host's kache store into the build
    #[arg(long)]
    pub no_host_kache: bool,
    /// Extra arguments passed through to `podman build`
    #[arg(last = true)]
    pub podman_args: Vec<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let image = build(ws, &args)?;
    println!("{image}");
    Ok(())
}

/// Build the image and return its full reference.
pub fn build(ws: &Workspace, args: &Args) -> Result<String> {
    let resolved = args
        .selection
        .resolve(ws, args.on.as_deref(), args.tag.as_deref())?;
    build_resolved(ws, args, resolved)
}

/// `test` already resolved the selection while deciding whether to rebuild.
pub fn build_resolved(
    ws: &Workspace,
    args: &Args,
    resolved: crate::select::Resolved,
) -> Result<String> {
    build_resolved_inner(ws, args, resolved, None)
}

pub fn rebuild_legacy(ws: &Workspace, args: &Args, expected_id: &str) -> Result<String> {
    if args.tag.is_some()
        || args.on.is_some()
        || args.selection.machine.is_some()
        || !args.selection.shells.is_empty()
    {
        bail!("legacy rebuild targets the default selection and image of this checkout");
    }
    let resolved = args.selection.resolve(ws, None, None)?;
    if resolved.machine != "generic" || resolved.image != ws.image_ref() {
        bail!(
            "legacy rebuild requires the checkout's default generic image; clear selection environment overrides"
        );
    }
    build_resolved_inner(ws, args, resolved, Some(expected_id))
}

fn build_resolved_inner(
    ws: &Workspace,
    args: &Args,
    resolved: crate::select::Resolved,
    legacy_expected: Option<&str>,
) -> Result<String> {
    ilex_devkit::provider::require_arch(ilex_devkit::core::Architecture::X86_64)?;
    cmd::require("podman")?;
    if resolved.machine != "generic" {
        let base = &resolved.base_image;
        if args.on.is_none() {
            build(
                ws,
                &Args {
                    selection: crate::select::Args {
                        shells: args.selection.shells.clone(),
                        machine: Some("generic".into()),
                    },
                    tag: Some(base.clone()),
                    no_pull: args.no_pull,
                    no_host_kache: args.no_host_kache,
                    ..Args::default()
                },
            )?;
        }
        resolved.require_base_match()?;
        return ilex_devkit::machine::build(
            ws,
            base,
            &ws.root.join("machines").join(&resolved.machine),
            args.tag.as_deref(),
            &args.podman_args,
        );
    }
    if args.on.is_some() {
        bail!("--on requires a machine other than generic");
    }
    let image = args
        .tag
        .as_deref()
        .map_or(resolved.image, |tag| ws.resolve_image(Some(tag)));
    ilex_devkit::workspace::check_tag_length(&image)?;
    let selection = resolved.selection;
    let (identity, source) = ws.source_identity()?;
    let request = ilex_devkit::core::BuildRequest {
        schema: ilex_devkit::core::SCHEMA_VERSION,
        job: format!(
            "build-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ),
        checkout: identity,
        operation: ilex_devkit::core::BuildOperation::Image(ilex_devkit::core::ImageBuildOptions {
            output: image,
            no_pull: args.no_pull,
            no_host_kache: args.no_host_kache,
            podman_args: args.podman_args.clone(),
        }),
        selection,
        profile: None,
        host_arch: ilex_devkit::core::Architecture::host()?,
        guest_arch: ilex_devkit::core::Architecture::X86_64,
        source,
        environment: Default::default(),
        recipe_version: ilex_devkit::executor::RECIPE_VERSION.into(),
    };
    use ilex_devkit::core::BuildExecutor;
    let executor = ilex_devkit::executor::LocalBuildExecutor::new(ws.clone())?;
    let result = match legacy_expected {
        Some(id) => executor.execute_rebuilding_legacy(&request, id)?,
        None => executor.execute(&request)?,
    };
    Ok(result
        .image
        .context("image build returned no image")?
        .local_reference)
}

pub use ilex_devkit::image::{BASE_IMAGE, image_exists};
