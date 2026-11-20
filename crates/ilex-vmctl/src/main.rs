//! Compatibility entry point for the original VM CLI and MCP registration.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ilex_vmctl::cli;

#[derive(Parser)]
#[command(
    name = "ilex-vmctl",
    version,
    about = "Screenshots and input for ilex VMs"
)]
struct Cli {
    /// Worktree slug of the VM to attach to (default: the checkout you are in; see `cargo xtask info`)
    #[arg(long, global = true, env = "ILEX_VM_SLUG")]
    slug: Option<String>,
    /// Which VM when several run for the slug (`gui`, `vm`, `graphical`, ...)
    #[arg(long, global = true, env = "ILEX_VM")]
    vm: Option<String>,
    #[command(subcommand)]
    cmd: LegacyCmd,
}

#[derive(Subcommand)]
enum LegacyCmd {
    #[command(flatten)]
    Control(cli::Cmd),
    /// Serve the same tools as an MCP server on stdio
    ///
    /// Register with: claude mcp add --transport stdio ilex-vm -- ilex-vmctl mcp
    Mcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        LegacyCmd::Control(cmd) => cli::run(cmd, cli.slug.as_deref(), cli.vm.as_deref()),
        LegacyCmd::Mcp => cli::serve(cli.slug.as_deref(), cli.vm),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ilex-vmctl: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
