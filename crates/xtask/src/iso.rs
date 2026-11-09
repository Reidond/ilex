//! `cargo xtask iso`: build the installer ISO for a published digest and push it to `ilex-iso` (CI only).
//!
//! Not implemented yet; lands with the installer ISO phase.

use anyhow::{Result, bail};
use ilex_devkit::Workspace;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Arguments, defined when the command is implemented
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

pub fn run(_ws: &Workspace, _args: Args) -> Result<()> {
    bail!("`cargo xtask iso` is not implemented yet (lands with the installer ISO phase)")
}
