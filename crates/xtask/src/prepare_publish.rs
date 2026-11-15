//! Record a machine fingerprint before testing, without any registry credentials.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use ilex_devkit::{Workspace, bcvk, cmd, config, machine};

use crate::{rechunk, registry::{self, FINGERPRINT_LABEL}};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Local machine image to label before capability checks and ephemeral tests
    pub image: String,
    #[arg(long)]
    pub machine: String,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    config::machine_manifest(&ws.root, &args.machine)?;
    let image = ws.resolve_image(Some(&args.image));
    if args.machine == "generic" || machine::image_machine(&image)? != args.machine {
        bail!("{image} must be the requested non-generic machine {}", args.machine);
    }
    // Never reuse a label here: preparation and publication independently
    // recompute the fingerprint, including the actual base's machine key.
    let fingerprint = rechunk::fingerprint(ws, &image)?;
    if rechunk::local_label(&image, FINGERPRINT_LABEL)?.as_deref() != Some(&fingerprint) {
        let id = bcvk::image_id(&image)?;
        let mut build = Command::new("podman");
        build.args(["build", "--pull=never", "--file", "-", "--tag", &image,
            "--label", &format!("{FINGERPRINT_LABEL}={fingerprint}")]);
        eprintln!("$ {}", cmd::display(&build));
        let mut child = build.stdin(Stdio::piped()).stdout(std::io::stderr()).spawn()?;
        writeln!(child.stdin.take().context("label build stdin")?, "FROM {id}")?;
        let status = child.wait()?;
        if !status.success() {
            bail!("machine fingerprint label build failed with {status}");
        }
    }
    registry::github_output("fingerprint", &fingerprint)?;
    registry::github_output("image_id", &bcvk::image_id(&image)?)
}
