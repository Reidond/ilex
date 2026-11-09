//! `cargo xtask vm`: boot this checkout's image as an interactive ephemeral
//! VM (no disk) with `bcvk ephemeral run-ssh`, under the machine-wide VM
//! slot cap. Exiting the shell tears the VM down.

use std::process::Command;

use anyhow::Result;
use ilex_devkit::{Workspace, cmd, slots};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Image tag or reference to boot (defaults to this checkout's image)
    #[arg(long)]
    pub image: Option<String>,
    /// Guest memory, e.g. `4G`
    #[arg(long, default_value = "4G")]
    pub memory: String,
    /// Guest vCPUs
    #[arg(long, default_value_t = 4)]
    pub vcpus: u32,
    /// Command to run over SSH instead of an interactive shell
    #[arg(last = true)]
    pub command: Vec<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    cmd::require("bcvk")?;
    let image = ws.resolve_image(args.image.as_deref());
    let name = ws.vm_name("vm");
    let _slot = slots::acquire(&name)?;

    let mut bcvk = Command::new("bcvk");
    bcvk.args(["ephemeral", "run-ssh"])
        .args(["--memory", &args.memory])
        .args(["--vcpus", &args.vcpus.to_string()])
        .arg(&image);
    if !args.command.is_empty() {
        bcvk.arg("--").args(&args.command);
    }
    cmd::run(&mut bcvk)
}
