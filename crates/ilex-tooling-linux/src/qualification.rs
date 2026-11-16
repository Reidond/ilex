//! Host/profile-scoped evidence. A prerequisite probe cannot write a pass.
use crate::{Workspace, cmd, security};
use anyhow::{Context, Result, ensure};
use ilex_tooling_core::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
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

pub fn record_path(ws: &Workspace, profile: &SecurityProfile) -> Result<PathBuf> {
    Ok(ws
        .state_dir()
        .join("security-qualification")
        .join(security::hash(&serde_json::to_vec(profile)?))
        .join("record.json"))
}

#[derive(Serialize, Deserialize)]
struct Record {
    schema: u32,
    owner: String,
    writer_pid: u32,
    writer_start: String,
    qualification: SecurityQualification,
}

pub fn load(path: &Path, owner: &str, profile: &SecurityProfile) -> Result<SecurityQualification> {
    if !path.exists() {
        return Ok(SecurityQualification {
            profile: profile.clone(),
            outcome: QualificationStatus::Unknown,
            evidence: None,
        });
    }
    let record: Record = serde_json::from_slice(&fs::read(path)?)?;
    check_version(record.schema)?;
    ensure!(
        record.owner == owner && record.qualification.profile == *profile,
        "qualification owner/profile mismatch"
    );
    Ok(record.qualification)
}

fn locked(path: &Path) -> Result<File> {
    fs::create_dir_all(path.parent().context("qualification path has no parent")?)?;
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    file.lock()?;
    Ok(file)
}
fn write(path: &Path, record: &Record) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(record)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}

/// Owns one measurement generation. Dropping/crashing never preserves a prior
/// pass: the on-disk generation is Qualifying until all guest checks finish.
pub struct Run {
    path: PathBuf,
    record: Record,
    finished: bool,
}
impl Run {
    pub fn begin(ws: &Workspace, spec: &VmSpec, artifacts: &Path) -> Result<Self> {
        ensure!(
            spec.security_launch == SecurityLaunch::QualificationFixture,
            "qualification requires an explicit fixture launch"
        );
        ws.identity()?.authorize(&spec.owner)?;
        let profile = profile(ws, spec)?;
        let path = record_path(ws, &profile)?;
        let _lock = locked(&path)?;
        if path.exists() {
            let previous: Record = serde_json::from_slice(&fs::read(&path)?)?;
            ensure!(previous.owner == spec.owner, "foreign qualification record");
            ensure!(
                previous.qualification.outcome != QualificationStatus::Qualifying
                    || !crate::provider::process_start(previous.writer_pid)
                        .is_ok_and(|s| s == previous.writer_start),
                "qualification already in progress for this exact profile"
            );
        }
        let record = Record {
            schema: SCHEMA_VERSION,
            owner: spec.owner.clone(),
            writer_pid: std::process::id(),
            writer_start: crate::provider::process_start(std::process::id())?,
            qualification: SecurityQualification {
                profile,
                outcome: QualificationStatus::Qualifying,
                evidence: Some(QualificationEvidence {
                    owner: spec.owner.clone(),
                    run: spec.run.clone(),
                    recorded_unix_ms: now()?,
                    artifacts: artifacts.to_path_buf(),
                }),
            },
        };
        write(&path, &record)?;
        Ok(Self {
            path,
            record,
            finished: false,
        })
    }
    pub fn finish(&mut self, result: &Result<()>) -> Result<()> {
        let _lock = locked(&self.path)?;
        let current = load(
            &self.path,
            &self.record.owner,
            &self.record.qualification.profile,
        )?;
        ensure!(
            current.evidence.as_ref().map(|e| &e.run)
                == self.record.qualification.evidence.as_ref().map(|e| &e.run),
            "qualification generation was replaced"
        );
        if result.is_ok() {
            ensure!(
                current.outcome == QualificationStatus::Qualifying,
                "a recorded failure cannot be promoted to a pass"
            );
        }
        self.record.qualification.outcome = match result {
            Ok(()) => QualificationStatus::Passed,
            Err(error) => QualificationStatus::Failed(format!("{error:#}")),
        };
        self.record
            .qualification
            .evidence
            .as_mut()
            .unwrap()
            .recorded_unix_ms = now()?;
        write(&self.path, &self.record)?;
        self.finished = true;
        Ok(())
    }
    pub fn evidence(&self) -> &SecurityQualification {
        &self.record.qualification
    }
}
impl Drop for Run {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish(&Err(anyhow::anyhow!(
                "qualification interrupted before completion"
            )));
        }
    }
}
fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}

/// Called by the owning process guard when a real accelerator error is seen.
/// vmctl's read-only probe still reports live failures without writing state.
pub(crate) fn runtime_failure(info: &crate::qemu::VmInfo, reason: &str) -> Result<()> {
    let (Some(profile), Some(path)) = (&info.security_profile, &info.qualification_record) else {
        return Ok(());
    };
    let _lock = locked(path)?;
    let mut qualification = load(path, &info.owner, profile)?;
    qualification.outcome = QualificationStatus::Failed(reason.into());
    qualification.evidence = Some(QualificationEvidence {
        owner: info.owner.clone(),
        run: info.run.clone(),
        recorded_unix_ms: now()?,
        artifacts: info.dir.clone(),
    });
    write(
        path,
        &Record {
            schema: SCHEMA_VERSION,
            owner: info.owner.clone(),
            writer_pid: std::process::id(),
            writer_start: crate::provider::process_start(std::process::id())?,
            qualification,
        },
    )
}

pub fn apply(caps: &mut VmCapabilities, qualification: SecurityQualification) {
    let passed = qualification.outcome == QualificationStatus::Passed;
    for feature in [Capability::FirmwareEnforcement, Capability::PersistentTpm] {
        let support = caps
            .provisioning
            .get(&feature)
            .cloned()
            .unwrap_or_else(|| CapabilityStatus::Unavailable("not provisioned".into()));
        let effective = if support != CapabilityStatus::Available {
            support
        } else if passed {
            CapabilityStatus::Available
        } else {
            CapabilityStatus::NotYetQualified(format!(
                "exact profile is {:?}",
                qualification.outcome
            ))
        };
        caps.features.insert(feature, effective);
    }
    caps.qualification = Some(qualification);
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
