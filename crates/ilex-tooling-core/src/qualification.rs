//! Evidence storage and capability promotion shared by security providers.
use crate::*;
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub fn record_path(state: &Path, profile: &SecurityProfile) -> Result<PathBuf> {
    let hash = digest(&serde_json::to_vec(profile)?);
    Ok(state
        .join("security-qualification")
        .join(&hash[7..])
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
    pub fn begin(
        path: PathBuf,
        spec: &VmSpec,
        profile: SecurityProfile,
        artifacts: &Path,
        process_start: impl Fn(u32) -> Result<String>,
    ) -> Result<Self> {
        ensure!(
            spec.security_launch == SecurityLaunch::QualificationFixture,
            "qualification requires an explicit fixture launch"
        );
        let _lock = locked(&path)?;
        if path.exists() {
            let previous: Record = serde_json::from_slice(&fs::read(&path)?)?;
            ensure!(previous.owner == spec.owner, "foreign qualification record");
            ensure!(
                previous.qualification.outcome != QualificationStatus::Qualifying
                    || !process_start(previous.writer_pid)
                        .is_ok_and(|s| s == previous.writer_start),
                "qualification already in progress for this exact profile"
            );
        }
        let record = Record {
            schema: SCHEMA_VERSION,
            owner: spec.owner.clone(),
            writer_pid: std::process::id(),
            writer_start: process_start(std::process::id())?,
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

pub fn invalidate(
    path: &Path,
    profile: &SecurityProfile,
    owner: &str,
    run: &str,
    artifacts: &Path,
    writer_start: String,
    reason: &str,
) -> Result<()> {
    let _lock = locked(path)?;
    let mut qualification = load(path, owner, profile)?;
    qualification.outcome = QualificationStatus::Failed(reason.into());
    qualification.evidence = Some(QualificationEvidence {
        owner: owner.into(),
        run: run.into(),
        recorded_unix_ms: now()?,
        artifacts: artifacts.into(),
    });
    write(
        path,
        &Record {
            schema: SCHEMA_VERSION,
            owner: owner.into(),
            writer_pid: std::process::id(),
            writer_start,
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
