//! Shared Linux backend qualification for part 4. No direct kernel boot and no
//! ignored success: missing prerequisites fail with an unqualified report.
use crate::{common, disk::vmtest};
use anyhow::{Result, ensure};
use ilex_devkit::{
    Workspace, cmd,
    core::{
        Accelerator, Capability, QualificationStatus, Requirement, SecurityLaunch, TpmLifecycle,
        VmProvider, VmRuntimeStatus, VmSpec,
    },
    provider::{LinuxProvider, LinuxVm},
    qemu::{self, VmConfig},
    security,
};
use libtest_mimic::Trial;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

mod probes;
mod unsigned;

pub fn trials() -> Vec<Trial> {
    vec![common::trial("security::firmware", run)]
}

fn run() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "security")?;
    fs::write(
        artifacts.join("result.json"),
        b"{\"qualification\":\"unqualified\",\"reason\":\"run has not completed\"}\n",
    )?;
    let start = Instant::now();
    let mut stage = "image";
    let result = (|| {
        fs::write(
            artifacts.join("host.json"),
            serde_json::to_vec_pretty(&security::host_probe())?,
        )?;
        let image = common::image()?;
        let inspected = cmd::output(Command::new("podman").args([
            "image",
            "inspect",
            "--format",
            "{{json .}}",
            &image,
        ]))?;
        fs::write(artifacts.join("image.json"), &inspected)?;
        let inspected: serde_json::Value = serde_json::from_str(&inspected)?;
        // A cloud-image firmware pass cannot qualify the candidate ilex image.
        let labels = serde_json::from_value(inspected["Config"]["Labels"].clone())?;
        let selection = ilex_devkit::config::Selection::from_labels(&labels)?;
        let digest = inspected["Digest"].as_str().unwrap_or_default();
        ensure!(
            digest.starts_with("sha256:") && digest.len() == 71,
            "image has no immutable manifest digest"
        );
        qualify(&ws, &artifacts, digest, &selection, &mut stage)
    })();
    // Diagnostic labels describe this tier's failure stage, not a second
    // capability/qualification model. T6's typed record remains authoritative.
    let disposition = if result.is_ok() {
        "qualified"
    } else {
        match stage {
            "platform" => "platform_unsupported_or_unqualified",
            "guest_boot" | "guest_assertions" | "unsigned_control" => "guest_failed",
            _ => "unqualified",
        }
    };
    let report = serde_json::json!({"qualification": if result.is_ok() { "passed" } else { "unqualified" }, "disposition": disposition, "stage": stage, "elapsed_seconds": start.elapsed().as_secs_f64(), "error": result.as_ref().err().map(|e| format!("{e:#}"))});
    fs::write(
        artifacts.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{report}");
    result
}

/// Drop always stops our process before removing our disk and identity bundle.
struct Fixture {
    workspace: Workspace,
    provider: LinuxProvider,
    vm: LinuxVm,
    spec: VmSpec,
    root: PathBuf,
}
impl Fixture {
    fn cleanup(&mut self) -> Result<()> {
        self.provider.destroy_owned(&mut self.vm)?;
        if self.root.exists() {
            fs::remove_dir_all(&self.root)?;
        }
        ensure!(!self.root.exists(), "security fixture disk cleanup failed");
        Ok(())
    }
    fn new(
        ws: &Workspace,
        source: &Path,
        purpose: &str,
        accelerator: Accelerator,
        secure: bool,
        tpm: bool,
    ) -> Result<Self> {
        let mut config = VmConfig::new(purpose, source);
        config.memory = std::env::var("ILEX_SECURITY_MEMORY").unwrap_or_else(|_| "4G".into());
        config.purpose = format!(
            "{}-{}-{}",
            config.purpose,
            if accelerator == Accelerator::Native {
                "kvm"
            } else {
                "tcg"
            },
            ilex_devkit::provider::memory_bytes(&config.memory)? / (1024 * 1024)
        );
        config.snapshot = false;
        let mut spec = ilex_devkit::provider::spec(ws, &config)?;
        spec.accelerator = accelerator;
        spec.security_launch = SecurityLaunch::QualificationFixture;
        spec.secure_boot = if secure {
            Requirement::Required
        } else {
            Requirement::Off
        };
        if tpm {
            spec.tpm = TpmLifecycle::Persistent {
                bundle_id: spec.run.clone(),
            };
        }
        let root = ws.state_dir().join("security-fixtures").join(&spec.run);
        // Validate all capabilities before allocating disk/state.
        let provider = LinuxProvider::new(ws.clone())?;
        provider.validate(&spec)?;
        fs::create_dir_all(&root)?;
        let disk = root.join("disk.qcow2");
        cmd::run(
            Command::new("qemu-img")
                .args([
                    "create",
                    "-f",
                    "qcow2",
                    "-F",
                    qemu::DiskFormat::from_path(source).name(),
                    "-b",
                ])
                .arg(source)
                .arg(&disk),
        )?;
        let ilex_devkit::core::BootSource::Disk { path, .. } = &mut spec.boot else {
            unreachable!()
        };
        *path = disk;
        // Provider format is explicit, unlike the legacy filename inference.
        let ilex_devkit::core::BootSource::Disk { format, .. } = &mut spec.boot else {
            unreachable!()
        };
        *format = ilex_devkit::core::DiskFormat::Qcow2;
        let vm = provider.create(spec.clone())?;
        Ok(Self {
            workspace: ws.clone(),
            provider,
            vm,
            spec,
            root,
        })
    }
    fn start(&mut self, deadline: Duration, stage: &mut &'static str) -> Result<f64> {
        let start = Instant::now();
        // Missing/denied firmware or TPM provisioning is a platform failure;
        // only a successfully launched VM can fail guest boot assertions.
        *stage = "platform";
        self.provider.start(&mut self.vm)?;
        *stage = "guest_boot";
        self.vm.running_mut()?.wait_ssh(deadline)?;
        Ok(start.elapsed().as_secs_f64())
    }
    fn logs(&self, artifacts: &Path, label: &str) -> Result<()> {
        // Startup can fail before LinuxProvider returns a running handle.
        // Authorize the known fixture path independently, before cleanup/retry.
        self.workspace.identity()?.authorize(&self.spec.owner)?;
        let runtime = self.workspace.ensure_runtime()?;
        let dir = runtime.join(&self.spec.purpose);
        if !dir.try_exists()? {
            return Ok(());
        }
        ilex_devkit::provider::require_owned_path(&runtime, &dir)?;
        for file in ["serial.log", "qemu.log", "swtpm.log", "swtpm-setup.log"] {
            let source = dir.join(file);
            if source.try_exists()? {
                ilex_devkit::provider::require_owned_path(&dir, &source)?;
                fs::copy(source, artifacts.join(format!("{label}-{file}")))?;
            }
        }
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        match self.provider.destroy_owned(&mut self.vm) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&self.root);
            }
            Err(e) => eprintln!(
                "security fixture cleanup: {e:#}; retained {}",
                self.root.display()
            ),
        }
    }
}

fn qualify(
    ws: &Workspace,
    artifacts: &Path,
    digest: &str,
    selection: &ilex_devkit::config::Selection,
    stage: &mut &'static str,
) -> Result<()> {
    let accelerator = match std::env::var("ILEX_SECURITY_ACCELERATOR").as_deref() {
        Ok("tcg") => Accelerator::Tcg,
        Ok("kvm") | Err(_) => Accelerator::Native,
        Ok(value) => anyhow::bail!(
            "unsupported ILEX_SECURITY_ACCELERATOR={value}; use kvm or tcg explicitly"
        ),
    };
    let deadline = Duration::from_secs(if accelerator == Accelerator::Tcg {
        900
    } else {
        420
    });
    *stage = "disk_delivery";
    // The disk installer leases its own slot. Finish before starting the guest.
    let disk = vmtest::disk(ws)?;
    *stage = "platform";
    let mut fixture = Fixture::new(ws, &disk, "security-firmware", accelerator, true, true)?;
    let mut qualification = ilex_devkit::qualification::begin(ws, &fixture.spec, artifacts)?;
    let mut result = (|| -> Result<()> {
        let caps = fixture.provider.probe(&fixture.spec)?;
        fs::write(
            artifacts.join("provisioning.json"),
            serde_json::to_vec_pretty(&caps)?,
        )?;
        caps.require_provisioning(Capability::FirmwareEnforcement)?;
        caps.require_provisioning(Capability::PersistentTpm)?;
        ensure!(
            caps.require(Capability::FirmwareEnforcement).is_err(),
            "fixture launch promoted unqualified capabilities"
        );
        let mut regular = fixture.spec.clone();
        regular.security_launch = SecurityLaunch::RequireQualified;
        ensure!(
            fixture.provider.validate(&regular).is_err(),
            "unqualified regular launch accepted"
        );
        println!(
            "security: {accelerator:?}, {} bytes, 2 vCPUs; each boot deadline {deadline:?}",
            fixture.spec.resources.memory_bytes
        );
        let first = match fixture.start(deadline, stage) {
            Ok(seconds) => seconds,
            Err(error) => {
                if let Err(log_error) = fixture.logs(artifacts, "failed-start") {
                    return Err(error.context(format!(
                        "startup diagnostics collection failed: {log_error:#}"
                    )));
                }
                return Err(error);
            }
        };
        let result = (|| -> Result<()> {
            *stage = "guest_assertions";
            measurements(
                fixture.vm.running_mut()?,
                artifacts,
                "first",
                digest,
                selection,
            )?;
            let control = ilex_vmctl::Vmctl::from_info(fixture.vm.running_mut()?.info.clone());
            probes::amdgpu(&control, artifacts)?;
            let vm = fixture.vm.running_mut()?;
            let versions = vm
            .ssh_ok("set -eu; rpm -q bootc shim-x64 grub2-efi-x64 systemd kernel-core; uname -r; bootctl status --no-pager; sha256sum /sys/firmware/efi/efivars/PK-* /sys/firmware/efi/efivars/KEK-* /sys/firmware/efi/efivars/db-* /sys/firmware/efi/efivars/dbx-*")?;
            fs::write(artifacts.join("guest.txt"), versions)?;
            // Encrypt a deliberately public test value to this TPM's primary seed
            // and PCR7 policy. Successful decrypt after cold restart proves TPM
            // identity persistence, rather than merely seeing the same PCR hash.
            // systemd-creds accepts a numeric PCR mask; manufacturing activates
            // only SHA256, so no implicit SHA1 policy can satisfy this fixture.
            vm.ssh_ok("set -eu; printf ilex-public-tpm-persistence > /var/tmp/ilex-security-plain; systemd-creds encrypt --with-key=tpm2 --tpm2-device=auto --tpm2-pcrs=7 /var/tmp/ilex-security-plain /var/tmp/ilex-security.cred; systemd-creds decrypt /var/tmp/ilex-security.cred - | cmp - /var/tmp/ilex-security-plain")?;
            let start = Instant::now();
            vm.reboot(deadline)?;
            let warm = start.elapsed().as_secs_f64();
            measurements(
                fixture.vm.running_mut()?,
                artifacts,
                "warm",
                digest,
                selection,
            )?;
            decrypt(fixture.vm.running_mut()?)?;
            fixture.logs(artifacts, "warm")?;
            // Graceful poweroff, then start a NEW QEMU and swtpm using the bundle.
            let vm = fixture.vm.running_mut()?;
            let _ = vm.ssh("systemctl poweroff");
            let start = Instant::now();
            while vm.exited()?.is_none() {
                ensure!(
                    start.elapsed() < Duration::from_secs(90),
                    "guest poweroff deadline exceeded"
                );
                thread::sleep(Duration::from_millis(100));
            }
            fixture.provider.stop(&mut fixture.vm)?;
            let cold = fixture.start(deadline, stage)?;
            *stage = "guest_assertions";
            measurements(
                fixture.vm.running_mut()?,
                artifacts,
                "cold",
                digest,
                selection,
            )?;
            decrypt(fixture.vm.running_mut()?)?;
            let bundle = security::bundle_path(ws, &fixture.spec)?;
            fs::copy(bundle.join("bundle.json"), artifacts.join("bundle.json"))?;
            fs::write(
                artifacts.join("timing.json"),
                serde_json::to_vec_pretty(
                    &serde_json::json!({"accelerator": accelerator, "memory_bytes": fixture.spec.resources.memory_bytes, "vcpus": 2, "boot_deadline_seconds": deadline.as_secs(), "first_ssh_seconds": first, "warm_ssh_seconds": warm, "cold_ssh_seconds": cold, "tpm_decrypt_after_warm_and_cold": true}),
                )?,
            )?;
            Ok(())
        })();
        fixture.logs(artifacts, "final")?;
        result?;
        fixture.provider.stop(&mut fixture.vm)?; // Keep identity; release its slot.
        unsigned_control(ws, artifacts, accelerator, deadline, stage)?;
        ensure!(
            !qemu::running_vms(ws)
                .iter()
                .any(|v| v.run == fixture.spec.run),
            "security VM leaked"
        );
        Ok(())
    })();
    qualification.finish(&result)?;
    if result.is_ok() {
        *stage = "capability_contract";
        result = qualification_contract(&mut fixture, artifacts);
        if result.is_err() {
            qualification.finish(&result)?;
        }
    } else {
        let caps = fixture.provider.probe(&fixture.spec)?;
        ensure!(
            matches!(
                caps.qualification.as_ref().map(|q| &q.outcome),
                Some(QualificationStatus::Failed(_))
            ),
            "recorded failure lost"
        );
        ensure!(
            caps.require(Capability::FirmwareEnforcement).is_err(),
            "failed profile became qualified"
        );
        fs::write(
            artifacts.join("failed-profile.json"),
            serde_json::to_vec_pretty(&caps)?,
        )?;
        if let Ok(vm) = fixture.vm.running_mut() {
            let control = ilex_vmctl::Vmctl::from_info(vm.info.clone());
            let caps = control.capabilities();
            ensure!(
                caps.require(Capability::FirmwareEnforcement).is_err(),
                "failed runtime advertised available enforcement"
            );
            fs::write(
                artifacts.join("failed-runtime.json"),
                serde_json::to_vec_pretty(&caps)?,
            )?;
        }
    }
    fs::write(
        artifacts.join("qualification.json"),
        serde_json::to_vec_pretty(qualification.evidence())?,
    )?;
    fixture.cleanup()?;
    result?;
    fs::write(
        artifacts.join("cleanup.txt"),
        "Owned QEMU/swtpm stopped; fixture disks and vars/TPM bundles removed. Base disk and image retained.\n",
    )?;
    Ok(())
}

fn qualification_contract(fixture: &mut Fixture, artifacts: &Path) -> Result<()> {
    let mut regular = fixture.spec.clone();
    regular.security_launch = SecurityLaunch::RequireQualified;
    let caps = fixture.provider.probe(&regular)?;
    caps.require(Capability::FirmwareEnforcement)?;
    caps.require(Capability::PersistentTpm)?;
    fixture.provider.validate(&regular)?;
    fs::write(
        artifacts.join("qualified-profile.json"),
        serde_json::to_vec_pretty(&caps)?,
    )?;
    for label in ["different-memory", "different-vcpus"] {
        let mut other = regular.clone();
        // Profiles qualified by earlier runs are valid evidence, not negative
        // fixtures. Search relative to this measured profile without assuming
        // that any particular RAM size or CPU count has never been measured.
        let caps = loop {
            if label == "different-memory" {
                other.resources.memory_bytes = other
                    .resources
                    .memory_bytes
                    .checked_add(1024 * 1024)
                    .ok_or_else(|| anyhow::anyhow!("no larger memory profile is representable"))?;
            } else {
                other.resources.vcpus =
                    other.resources.vcpus.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("no larger vCPU profile is representable")
                    })?;
            }
            let caps = fixture.provider.probe(&other)?;
            if caps
                .qualification
                .as_ref()
                .is_some_and(|q| q.outcome == QualificationStatus::Unknown)
            {
                break caps;
            }
        };
        ensure!(
            other.resources != regular.resources,
            "negative profile matches the measured profile"
        );
        caps.require_provisioning(Capability::FirmwareEnforcement)?;
        ensure!(
            caps.require(Capability::FirmwareEnforcement).is_err()
                && fixture.provider.validate(&other).is_err(),
            "unmeasured {label} profile was qualified"
        );
        other.security_launch = SecurityLaunch::QualificationFixture;
        fixture.provider.validate(&other)?;
        fs::write(
            artifacts.join(format!("unknown-{label}.json")),
            serde_json::to_vec_pretty(&caps)?,
        )?;
    }
    // Exercise live QMP truth, not a synthetic paused-status helper.
    fixture.provider.start(&mut fixture.vm)?;
    let vm = fixture.vm.running_mut()?;
    let control = ilex_vmctl::Vmctl::from_info(vm.info.clone());
    {
        let mut qmp = ilex_devkit::qmp::Qmp::connect(&vm.info.qmp_sock)?;
        qmp.execute("stop", serde_json::json!({}))?;
    }
    let caps = control.capabilities();
    ensure!(
        matches!(caps.runtime, VmRuntimeStatus::Paused(_))
            && caps.require(Capability::FirmwareEnforcement).is_err(),
        "paused runtime retained qualified capabilities"
    );
    fs::write(
        artifacts.join("paused-runtime.json"),
        serde_json::to_vec_pretty(&caps)?,
    )?;
    {
        let mut qmp = ilex_devkit::qmp::Qmp::connect(&vm.info.qmp_sock)?;
        qmp.execute("cont", serde_json::json!({}))?;
    }
    let caps = control.capabilities();
    ensure!(
        caps.runtime == VmRuntimeStatus::Running,
        "guest did not resume"
    );
    caps.require(Capability::FirmwareEnforcement)?;
    fs::write(
        artifacts.join("running-runtime.json"),
        serde_json::to_vec_pretty(&caps)?,
    )?;
    Ok(())
}

fn decrypt(vm: &qemu::Vm) -> Result<()> {
    vm.ssh_ok(
        "systemd-creds decrypt /var/tmp/ilex-security.cred - | cmp - /var/tmp/ilex-security-plain",
    )?;
    Ok(())
}
fn measurements(
    vm: &qemu::Vm,
    artifacts: &Path,
    label: &str,
    digest: &str,
    selection: &ilex_devkit::config::Selection,
) -> Result<()> {
    let control = ilex_vmctl::Vmctl::from_info(vm.info.clone());
    probes::guest(&control, artifacts, label, digest, selection)
}

fn unsigned_control(
    ws: &Workspace,
    artifacts: &Path,
    accelerator: Accelerator,
    deadline: Duration,
    stage: &mut &'static str,
) -> Result<()> {
    *stage = "platform";
    let payload = unsigned::serial_payload();
    ensure!(
        payload[0x98 + 112 + 4 * 8..0x98 + 112 + 5 * 8]
            .iter()
            .all(|b| *b == 0),
        "EFI security directory must be empty"
    );
    fs::write(artifacts.join("unsigned.efi"), &payload)?;
    let source = artifacts.join("unsigned.img");
    fs::write(&source, unsigned::fat_disk(&payload))?;
    let mut run = |secure| -> Result<serde_json::Value> {
        let label = if secure {
            "unsigned-on"
        } else {
            "unsigned-off"
        };
        // Each control must provision and launch before it can fail as a guest.
        // In particular, the on control must not inherit the off control's stage.
        *stage = "platform";
        let mut fixture = Fixture::new(
            ws,
            &source,
            &format!("security-efi-{}", if secure { "on" } else { "off" }),
            accelerator,
            secure,
            false,
        )?;
        if let Err(error) = fixture.provider.start(&mut fixture.vm) {
            if let Err(log_error) = fixture.logs(artifacts, &format!("{label}-failed-start")) {
                return Err(error.context(format!(
                    "unsigned-control startup diagnostics collection failed: {log_error:#}"
                )));
            }
            return Err(error);
        }
        *stage = "unsigned_control";
        let vm = fixture.vm.running_mut()?;
        let start = Instant::now();
        let result = (|| loop {
            let serial = fs::read_to_string(vm.info.dir.join("serial.log")).unwrap_or_default();
            let executed = serial.contains("ILEX-UNSIGNED-EXECUTED");
            if secure {
                ensure!(!executed, "unsigned EFI executed with Secure Boot required");
                if serial.contains("Access Denied") || serial.contains("Security Violation") {
                    break Ok(());
                }
            } else if executed {
                break Ok(());
            }
            ensure!(
                vm.exited()?.is_none(),
                "unsigned control QEMU exited before evidence"
            );
            if start.elapsed() > deadline {
                break Err(anyhow::anyhow!(
                    "{label} unqualified: no {} within {deadline:?}",
                    if secure {
                        "explicit firmware rejection"
                    } else {
                        "payload execution marker"
                    }
                ));
            }
            thread::sleep(Duration::from_millis(100));
        })();
        fixture.logs(artifacts, label)?;
        result?;
        fs::write(
            artifacts.join(format!("{label}.json")),
            serde_json::to_vec_pretty(
                &serde_json::json!({"payload_sha256": security::hash(&payload), "secure_boot_required": secure, "elapsed_seconds": start.elapsed().as_secs_f64(), "evidence": if secure { "firmware rejection" } else { "serial execution marker" }}),
            )?,
        )?;
        let manifest: serde_json::Value = serde_json::from_slice(&fs::read(
            security::bundle_path(ws, &fixture.spec)?.join("bundle.json"),
        )?)?;
        fs::write(
            artifacts.join(format!("{label}-firmware.json")),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        fixture.cleanup()?;
        Ok(manifest)
    };
    // The positive control first proves that this exact PE file is executable.
    let result = (|| -> Result<()> {
        let off = run(false)?;
        let on = run(true)?;
        ensure!(
            off["code_sha256"] == on["code_sha256"]
                && off["vars_template_sha256"] == on["vars_template_sha256"]
                && off["vars_initial_sha256"] != on["vars_initial_sha256"]
                && off["sb_disabled_byte"].is_number()
                && on["sb_disabled_byte"].is_null(),
            "control firmware differed beyond the explicit SB enable switch"
        );
        Ok(())
    })();
    fs::remove_file(source)?;
    result
}
