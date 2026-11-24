//! Security host probing and the booted-guest qualification entry point.
use anyhow::Result;
use ilex_devkit::Workspace;
#[cfg(not(target_os = "windows"))]
use ilex_devkit::{
    cmd,
    core::{Capability, Requirement, TpmLifecycle, VmProvider},
    provider::{LinuxProvider, spec},
    qemu::VmConfig,
};
#[cfg(not(target_os = "windows"))]
use std::process::Command;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Boot the selected image and qualify firmware, measurements and lifecycle
    #[arg(long)]
    pub(super) self_check: bool,
    /// Hardware accelerator: KVM on Linux, Hyper-V on Windows
    #[arg(long, value_parser = ["kvm", "hyperv"])]
    pub(super) accelerator: Option<String>,
    /// Assert T6's existing qualification for this exact resource profile; no VM is launched
    #[arg(long, conflicts_with = "self_check")]
    pub(super) require_qualified: bool,
    /// Explicit profile size (otherwise ILEX_SECURITY_MEMORY, then 4G)
    #[arg(long)]
    pub(super) memory: Option<String>,
    /// Reconcile one interrupted, journaled Gen2 run owned by this checkout
    #[arg(long, conflicts_with_all = ["self_check", "require_qualified"])]
    pub(super) cleanup_run: Option<String>,
    /// Open an owned blank Gen2 installer fixture for the manual encrypted lifecycle
    #[arg(long, conflicts_with_all = ["self_check", "require_qualified", "cleanup_run"])]
    pub(super) encrypted_console: bool,
}
pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    #[cfg(target_os = "windows")]
    return crate::security_windows::run(ws, args);
    #[cfg(not(target_os = "windows"))]
    linux(ws, args)
}

#[cfg(not(target_os = "windows"))]
fn linux(ws: &Workspace, args: Args) -> Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "security backend is unavailable on this host"
    );
    anyhow::ensure!(
        args.cleanup_run.is_none() && !args.encrypted_console,
        "--cleanup-run and --encrypted-console require the Windows Gen2 provider"
    );
    let accelerator = args.accelerator.as_deref().unwrap_or("kvm");
    anyhow::ensure!(accelerator != "hyperv", "Hyper-V requires native Windows");
    let memory = args
        .memory
        .or_else(|| std::env::var("ILEX_SECURITY_MEMORY").ok())
        .unwrap_or_else(|| "4G".into());
    if args.require_qualified {
        let mut config = VmConfig::new("security-probe", "unused.qcow2");
        config.memory = memory;
        config.snapshot = false;
        let mut request = spec(ws, &config)?;
        request.secure_boot = Requirement::Required;
        request.tpm = TpmLifecycle::Persistent {
            bundle_id: request.run.clone(),
        };
        let caps = LinuxProvider::new(ws.clone())?.probe(&request)?;
        println!("{}", serde_json::to_string_pretty(&caps)?);
        caps.require(Capability::FirmwareEnforcement)?;
        caps.require(Capability::PersistentTpm)?;
        return Ok(());
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&ilex_devkit::security::host_probe())?
    );
    if args.self_check {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        cmd::run(
            Command::new(cargo)
                .current_dir(&ws.root)
                .args(["ilexctl", "test", "security::firmware"])
                .env("ILEX_SECURITY_MEMORY", memory),
        )?;
    }
    Ok(())
}
