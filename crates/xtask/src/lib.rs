//! Build, boot, test and release the ilex image.
//!
//! Each subcommand lives in its own module with its own `Args`, so commands
//! can grow independently.

use anyhow::Result;
use clap::Subcommand;
use ilex_devkit::Workspace;

mod apps;
mod build;
mod builder;
mod changed;
mod dev_image;
mod disk;
mod doctor;
mod identity;
mod info;
mod iso;
mod migrate;
mod prepare_publish;
mod publish;
mod rechunk;
mod registry;
mod remote;
mod remote_worker;
mod runtime;
mod security;
#[cfg(target_os = "windows")]
mod security_windows;
mod select;
mod test;
mod tool;
mod vm;
#[cfg(target_os = "windows")]
mod vm_windows;

#[derive(Subcommand)]
pub enum CheckoutCmd {
    /// Resume, cancel or qualify this checkout's remote builder jobs
    Remote(remote::Args),
    #[command(hide = true)]
    RemoteWorker(remote_worker::Args),
    /// Provision and maintain the native Windows Hyper-V Fedora builder
    Builder(builder::Args),
    /// Import an explicitly exported development cache into the Windows builder
    Migrate(migrate::Args),
    /// Fetch or inspect the pinned per-user portable Windows QEMU runtime
    Runtime(runtime::Args),
    /// podman build, tagged with this checkout's slug, labels included
    Build(build::Args),
    /// Split the image into reproducible chunks and add the `ilex.fingerprint` label
    Rechunk(rechunk::Args),
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
    /// Show this checkout's slug, image, VM names and state directories
    Info(info::Args),
    /// Inspect automatic primary ownership, release stale claims, or rebuild legacy images
    Identity(identity::Args),
}

pub use apps::Args as AppsArgs;
pub use doctor::Args as DoctorArgs;
pub use security::Args as SecurityArgs;
pub use tool::Args as ToolArgs;
pub use vm::Args as VmArgs;

pub enum Cmd {
    Checkout(CheckoutCmd),
    Apps(AppsArgs),
    Vm(VmArgs),
    Tool(ToolArgs),
    Doctor(DoctorArgs),
    Security(SecurityArgs),
}

pub fn exit_code(err: &anyhow::Error) -> u8 {
    err.downcast_ref::<remote::Failure>()
        .map_or(1, |failure| failure.code)
}

pub fn run(cmd: Cmd) -> Result<()> {
    // Infrastructure commands work outside a checkout.
    let cmd = match cmd {
        Cmd::Checkout(CheckoutCmd::Identity(args)) => return identity::run(&args),
        Cmd::Checkout(CheckoutCmd::RemoteWorker(args)) => return remote_worker::run(args),
        Cmd::Checkout(CheckoutCmd::Builder(args)) => return builder::run(args),
        Cmd::Checkout(CheckoutCmd::Runtime(args)) => return runtime::run(args),
        other => other,
    };
    let ws = Workspace::discover()?;
    let Some(cmd) = remote::route(&ws, cmd)? else {
        return Ok(());
    };
    match cmd {
        Cmd::Checkout(CheckoutCmd::Remote(args)) => remote::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Migrate(args)) => migrate::run(&ws, args),
        Cmd::Apps(args) => apps::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Build(args)) => build::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Rechunk(args)) => rechunk::run(&ws, args),
        Cmd::Vm(args) => {
            let resolved = args.resolve(&ws)?;
            if args.secure {
                #[cfg(target_os = "windows")]
                return security_windows::vm(&ws, args, resolved);
                #[cfg(not(target_os = "windows"))]
                anyhow::bail!(
                    "vm --secure selects the Windows Hyper-V provider; use xtask security on Linux"
                );
            }
            #[cfg(target_os = "windows")]
            if args.gui || args.disk {
                return vm_windows::run(&ws, args, resolved);
            }
            if !cfg!(any(target_os = "linux", target_os = "windows")) {
                anyhow::bail!("no VM backend implemented for this host");
            }
            vm::run(&ws, args, resolved)
        }
        Cmd::Checkout(CheckoutCmd::Disk(args)) => disk::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Test(args)) => test::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Changed(args)) => changed::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Publish(args)) => publish::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::PreparePublish(args)) => prepare_publish::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Iso(args)) => iso::run(&ws, args),
        Cmd::Tool(args) => tool::run(&ws, args),
        Cmd::Checkout(CheckoutCmd::Info(args)) => info::run(&ws, args),
        Cmd::Doctor(args) => doctor::run(&ws, args),
        Cmd::Security(args) => security::run(&ws, args),
        Cmd::Checkout(
            CheckoutCmd::Builder(_)
            | CheckoutCmd::Runtime(_)
            | CheckoutCmd::Identity(_)
            | CheckoutCmd::RemoteWorker(_),
        ) => unreachable!("handled before workspace discovery"),
    }
}
