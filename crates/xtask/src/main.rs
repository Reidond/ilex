//! `cargo xtask <cmd>`: build, boot, test and release the ilex image.
//!
//! Each subcommand lives in its own module with its own `Args`, so commands
//! can grow independently.

use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use ilex_devkit::Workspace;

mod build;
mod builder;
mod apps;
mod changed;
mod disk;
mod doctor;
mod identity;
mod info;
mod iso;
mod prepare_publish;
mod publish;
mod rechunk;
mod registry;
mod remote;
mod remote_worker;
mod runtime;
mod select;
mod security;
mod test;
mod tool;
mod vm;
mod wsl;

#[derive(Parser)]
#[command(
    name = "cargo xtask",
    about = "Build, boot, test and release the ilex image"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Resume, cancel or qualify this checkout's remote builder jobs
    Remote(remote::Args),
    #[command(hide = true)]
    RemoteWorker(remote_worker::Args),
    /// Provision and maintain the native Windows Hyper-V Fedora builder
    Builder(builder::Args),
    /// Fetch or inspect the pinned per-user portable Windows QEMU runtime
    Runtime(runtime::Args),
    /// Check configured Flatpaks against Flathub without downloading apps
    Apps(apps::Args),
    /// podman build, tagged with this checkout's slug, labels included
    Build(build::Args),
    /// Split the image into reproducible chunks and add the `ilex.fingerprint` label
    Rechunk(rechunk::Args),
    /// Boot the current image as an interactive ephemeral VM
    Vm(vm::Args),
    /// Write a qcow2 or raw disk with `bcvk to-disk`
    Disk(disk::Args),
    /// Build if needed, then run the e2e harness (`cargo xtask test ephemeral::`)
    Test(test::Args),
    /// Compare the candidate's fingerprint with a published image
    Changed(changed::Args),
    /// Push by digest, tag and sign (CI only)
    Publish(publish::Args),
    /// Record a machine fingerprint before testing its immutable image ID
    PreparePublish(prepare_publish::Args),
    /// Build the network installer ISO (releases are CI-only)
    Iso(iso::Args),
    /// Build a drupe tool image from Fedora packages (`cargo xtask tool strace`)
    Tool(tool::Args),
    /// Show this checkout's slug, image, VM names and state directories
    Info(info::Args),
    /// Probe the backend; --self-check exercises owned checkout fixtures
    Doctor(doctor::Args),
    /// Inspect automatic primary ownership, release stale claims, or rebuild legacy images
    Identity(identity::Args),
    /// Probe secure firmware/KVM/SMM; --self-check boots the firmware tier
    Security(security::Args),
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: error: {err:#}");
            err.downcast_ref::<remote::Failure>()
                .map(|failure| ExitCode::from(failure.code))
                .unwrap_or(ExitCode::FAILURE)
        }
    }
}

fn run(cmd: Cmd) -> Result<()> {
    if let Cmd::Identity(args) = &cmd {
        return identity::run(args);
    }
    // Infrastructure commands work outside a checkout and never discover WSL.
    let cmd = match cmd {
        Cmd::RemoteWorker(args) => return remote_worker::run(args),
        Cmd::Builder(args) => return builder::run(args),
        Cmd::Runtime(args) => return runtime::run(args),
        other => other,
    };
    let ws = Workspace::discover()?;
    let Some(cmd) = remote::route(&ws, cmd)? else {
        return Ok(());
    };
    match cmd {
        Cmd::Remote(args) => remote::run(&ws, args),
        Cmd::RemoteWorker(_) => unreachable!("handled before workspace discovery"),
        Cmd::Apps(args) => apps::run(&ws, args),
        Cmd::Build(args) => build::run(&ws, args),
        Cmd::Rechunk(args) => rechunk::run(&ws, args),
        // The VM stack lives in WSL2; --gui's window is ilex-viewer.exe.
        Cmd::Vm(args) => {
            // Reject invalid selection before building the native viewer or
            // handing over to WSL. GUI prediction inspects Linux image labels.
            let resolved = args.resolve(&ws)?;
            match ilex_devkit::host_backend() {
                ilex_devkit::HostBackend::WslCompatibility => {
                    wsl::forward(&ws.root, args.gui && !args.print_image)
                }
                ilex_devkit::HostBackend::Linux => vm::run(&ws, args, resolved),
                ilex_devkit::HostBackend::Unsupported => {
                    anyhow::bail!("no VM backend implemented for this host")
                }
            }
        }
        Cmd::Disk(args) => disk::run(&ws, args),
        Cmd::Test(args) => test::run(&ws, args),
        Cmd::Changed(args) => changed::run(&ws, args),
        Cmd::Publish(args) => publish::run(&ws, args),
        Cmd::PreparePublish(args) => prepare_publish::run(&ws, args),
        Cmd::Iso(args) => iso::run(&ws, args),
        Cmd::Tool(args) => tool::run(&ws, args),
        Cmd::Info(args) => info::run(&ws, args),
        Cmd::Doctor(args) => doctor::run(&ws, args),
        Cmd::Identity(args) => identity::run(&args),
        Cmd::Security(args) => security::run(&ws, args),
        Cmd::Builder(_) | Cmd::Runtime(_) => unreachable!("handled before workspace discovery"),
    }
}
