//! Host/profile-scoped evidence. A prerequisite probe cannot write a pass.
use crate::{Workspace, cmd, security};
use anyhow::{Context, Result};
use ilex_tooling_core::*;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn profile(ws: &Workspace, spec: &VmSpec) -> Result<SecurityProfile> {
    let cpu = fs::read_to_string("/proc/cpuinfo")?;
    let mut cpu: Vec<_> = cpu
        .lines()
        .filter(|line| {
            [
                "vendor_id",
                "cpu family",
                "model\t",
                "model name",
                "stepping",
                "microcode",
                "flags",
                "address sizes",
            ]
            .iter()
            .any(|key| line.starts_with(key))
        })
        .collect();
    cpu.sort_unstable();
    cpu.dedup();
    let mut host = BTreeMap::new();
    for (key, file) in [
        ("machine", "/etc/machine-id"),
        ("boot", "/proc/sys/kernel/random/boot_id"),
    ] {
        host.insert(key.into(), security::hash(fs::read(file)?.trim_ascii()));
    }
    host.insert(
        "cpu_sha256".into(),
        security::hash(cpu.join("\n").as_bytes()),
    );
    for (key, program, arg) in [
        ("kernel", "uname", "-r"),
        ("qemu", "qemu-system-x86_64", "--version"),
        ("swtpm", "swtpm", "--version"),
    ] {
        host.insert(
            key.into(),
            cmd::probe(Command::new(program).arg(arg))
                .context("qualification host version probe failed")?,
        );
    }
    host.insert(
        "kvm_smm".into(),
        security::kvm_smm().map_or_else(|_| "unavailable".into(), |v| v.to_string()),
    );
    Ok(SecurityProfile {
        protocol: 2,
        accelerator: spec.accelerator,
        resources: spec.resources.clone(),
        cpu_model: if spec.accelerator == Accelerator::Native {
            "host"
        } else {
            "max"
        }
        .into(),
        guest_arch: spec.guest_arch,
        host,
        firmware: security::firmware_identity(ws, spec)?,
    })
}

pub use ilex_tooling_core::qualification::{apply, load};

pub fn record_path(ws: &Workspace, profile: &SecurityProfile) -> Result<PathBuf> {
    ilex_tooling_core::qualification::record_path(&ws.state_dir(), profile)
}

pub fn begin(
    ws: &Workspace,
    spec: &VmSpec,
    artifacts: &Path,
) -> Result<ilex_tooling_core::qualification::Run> {
    ws.identity()?.authorize(&spec.owner)?;
    let profile = profile(ws, spec)?;
    ilex_tooling_core::qualification::Run::begin(
        record_path(ws, &profile)?,
        spec,
        profile,
        artifacts,
        crate::provider::process_start,
    )
}

/// Called by the owning process guard when a real accelerator error is seen.
pub(crate) fn runtime_failure(info: &crate::qemu::VmInfo, reason: &str) -> Result<()> {
    let (Some(profile), Some(path)) = (&info.security_profile, &info.qualification_record) else {
        return Ok(());
    };
    ilex_tooling_core::qualification::invalidate(
        path,
        profile,
        &info.owner,
        &info.run,
        &info.dir,
        crate::provider::process_start(std::process::id())?,
        reason,
    )
}

/// Runtime truth overrides historical profile evidence. Failed/paused/unknown
/// QMP state never advertises effective security capabilities as Available.
pub fn runtime(info: &crate::qemu::VmInfo, caps: &mut VmCapabilities) {
    caps.runtime = if !info.is_running() {
        VmRuntimeStatus::Stopped
    } else {
        match crate::qmp::Qmp::connect_with_timeout(
            &info.qmp_sock,
            std::time::Duration::from_secs(2),
        )
        .and_then(|mut q| q.execute("query-status", serde_json::json!({})))
        {
            Ok(value) if value["status"] == "running" && value["running"] == true => {
                VmRuntimeStatus::Running
            }
            Ok(value)
                if value["status"] == "internal-error"
                    || value["status"] == "guest-panicked"
                    || value["status"] == "io-error" =>
            {
                VmRuntimeStatus::Failed(value["status"].as_str().unwrap_or("QMP failure").into())
            }
            Ok(value) => VmRuntimeStatus::Paused(
                value["status"]
                    .as_str()
                    .unwrap_or("unknown QMP status")
                    .into(),
            ),
            Err(_) => VmRuntimeStatus::Unknown,
        }
    };
    if let (Some(profile), Some(path)) = (&info.security_profile, &info.qualification_record) {
        match load(path, &info.owner, profile) {
            Ok(record) => apply(caps, record),
            Err(error) => apply(
                caps,
                SecurityQualification {
                    profile: profile.clone(),
                    outcome: QualificationStatus::Failed(format!("invalid evidence: {error:#}")),
                    evidence: None,
                },
            ),
        }
    }
    if !matches!(caps.runtime, VmRuntimeStatus::Running) {
        for feature in [Capability::FirmwareEnforcement, Capability::PersistentTpm] {
            caps.features.insert(
                feature,
                CapabilityStatus::NotYetQualified(format!("runtime is {:?}", caps.runtime)),
            );
        }
        if let Some(record) = &mut caps.qualification {
            record.outcome = match &caps.runtime {
                VmRuntimeStatus::Failed(reason) => QualificationStatus::Failed(reason.clone()),
                _ if matches!(record.outcome, QualificationStatus::Failed(_)) => {
                    record.outcome.clone()
                }
                _ => QualificationStatus::Unknown,
            };
        }
    }
}
