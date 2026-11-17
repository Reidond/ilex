//! Security host probing and the booted-guest qualification entry point.
use anyhow::Result;
use ilex_devkit::{
    Workspace, cmd,
    core::{Accelerator, Capability, Requirement, TpmLifecycle, VmProvider},
    provider::{LinuxProvider, spec},
    qemu::VmConfig,
};
use std::process::Command;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Boot the selected image and qualify firmware, measurements and lifecycle
    #[arg(long)]
    self_check: bool,
    /// Explicit software emulation; no automatic fallback when KVM/SMM fails
    #[arg(long, value_parser = ["kvm", "tcg"], default_value = "kvm")]
    accelerator: String,
    /// Assert T6's existing qualification for this exact resource profile; no VM is launched
    #[arg(long, conflicts_with = "self_check")]
    require_qualified: bool,
    /// Explicit profile size (otherwise ILEX_SECURITY_MEMORY, then 4G)
    #[arg(long)]
    memory: Option<String>,
}
pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    if !cfg!(target_os = "linux") {
        println!(
            "{}",
            serde_json::json!({
                "qualification": "unqualified",
                "disposition": "platform_unsupported_or_unqualified",
                "reason": "Native Windows security requires tooling T5's Hyper-V Gen2 provider (MicrosoftUEFICertificateAuthority and persistent vTPM). It is not available in this backend. Linux/WSL diagnostics require running inside the existing Linux executor."
            })
        );
        anyhow::bail!("no native Windows security provider available; no guest qualified");
    }
    let memory = args
        .memory
        .or_else(|| std::env::var("ILEX_SECURITY_MEMORY").ok())
        .unwrap_or_else(|| "4G".into());
    if args.require_qualified {
        let mut config = VmConfig::new("security-probe", "unused.qcow2");
        config.memory = memory;
        config.snapshot = false;
        let mut request = spec(ws, &config)?;
        request.accelerator = if args.accelerator == "tcg" {
            Accelerator::Tcg
        } else {
            Accelerator::Native
        };
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
                .args(["xtask", "test", "security::firmware"])
                .env("ILEX_SECURITY_ACCELERATOR", args.accelerator)
                .env("ILEX_SECURITY_MEMORY", memory),
        )?;
    }
    Ok(())
}
