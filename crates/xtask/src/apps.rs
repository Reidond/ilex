//! Small network-only manifest check; installs no applications.
use anyhow::Result;
use ilex_devkit::{Workspace, cmd};
use std::process::Command;

#[derive(clap::Args)]
pub struct Args {
    /// Check every configured ID and the committed Flathub trust file
    #[arg(long, required = true)]
    pub check: bool,
}

pub fn run(ws: &Workspace, _args: Args) -> Result<()> {
    cmd::run(
        Command::new("python3")
            .args(["-I", "-S"])
            .arg(ws.root.join("build/apps-check.py"))
            .arg(&ws.root),
    )
}
