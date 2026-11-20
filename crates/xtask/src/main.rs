//! Compatibility entry point for cargo xtask and the Linux remote worker.

use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use xtask::{CheckoutCmd, Cmd};

#[derive(Parser)]
#[command(
    name = "cargo xtask",
    about = "Build, boot, test and release the ilex image"
)]
struct Cli {
    #[command(subcommand)]
    cmd: LegacyCmd,
}

#[derive(Subcommand)]
enum LegacyCmd {
    #[command(flatten)]
    Checkout(CheckoutCmd),
    /// Check configured Flatpaks against Flathub without downloading apps
    Apps(xtask::AppsArgs),
    /// Boot the current image as an interactive ephemeral VM
    Vm(xtask::VmArgs),
    /// Build a drupe tool image from Fedora packages (`cargo xtask tool strace`)
    Tool(xtask::ToolArgs),
    /// Probe the backend; --self-check exercises owned checkout fixtures
    Doctor(xtask::DoctorArgs),
    /// Probe secure firmware/KVM/SMM; --self-check boots the firmware tier
    Security(xtask::SecurityArgs),
}

fn main() -> ExitCode {
    let cmd = match Cli::parse().cmd {
        LegacyCmd::Checkout(cmd) => Cmd::Checkout(cmd),
        LegacyCmd::Apps(args) => Cmd::Apps(args),
        LegacyCmd::Vm(args) => Cmd::Vm(args),
        LegacyCmd::Tool(args) => Cmd::Tool(args),
        LegacyCmd::Doctor(args) => Cmd::Doctor(args),
        LegacyCmd::Security(args) => Cmd::Security(args),
    };
    if io::stderr().is_terminal() && !matches!(cmd, Cmd::Checkout(CheckoutCmd::RemoteWorker(_))) {
        eprintln!("xtask: use cargo ilexctl for checkout commands");
    }
    match xtask::run(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: error: {err:#}");
            ExitCode::from(xtask::exit_code(&err))
        }
    }
}
