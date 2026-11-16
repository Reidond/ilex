//! Security host probing and the booted-guest qualification entry point.
use anyhow::Result;
use ilex_devkit::{Workspace, cmd};
use std::process::Command;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Boot the selected image and qualify firmware, measurements and lifecycle
    #[arg(long)]
    self_check: bool,
    /// Explicit software emulation; no automatic fallback when KVM/SMM fails
    #[arg(long, value_parser = ["kvm", "tcg"], default_value = "kvm")]
    accelerator: String,
}
pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "security qualification runs on the Linux backend"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&ilex_devkit::security::host_probe())?
    );
    if args.self_check {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        cmd::run(
            Command::new(cargo)
                .current_dir(&ws.root)
                .args(["xtask", "test", "security::firmware"])
                .env("ILEX_SECURITY_ACCELERATOR", args.accelerator),
        )?;
    }
    Ok(())
}
