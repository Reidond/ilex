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
    fs::write(
        artifacts.join("host.json"),
        serde_json::to_vec_pretty(&security::host_probe())?,
    )?;
    let start = Instant::now();
    let image = common::image()?;
    fs::write(
        artifacts.join("image.json"),
        cmd::output(Command::new("podman").args([
            "image",
            "inspect",
            "--format",
            "{{json .}}",
            &image,
        ]))?,
    )?;
    let result = qualify(&ws, &artifacts);
    let report = serde_json::json!({"qualification": if result.is_ok() { "passed" } else { "unqualified" }, "elapsed_seconds": start.elapsed().as_secs_f64(), "error": result.as_ref().err().map(|e| format!("{e:#}"))});
    fs::write(
        artifacts.join("result.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{report}");
    result
}

/// Drop always stops our process before removing our disk and identity bundle.
struct Fixture {
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
            provider,
            vm,
            spec,
            root,
        })
    }
    fn start(&mut self, deadline: Duration) -> Result<f64> {
        let start = Instant::now();
        self.provider.start(&mut self.vm)?;
        self.vm.running_mut()?.wait_ssh(deadline)?;
        Ok(start.elapsed().as_secs_f64())
    }
    fn logs(&mut self, artifacts: &Path, label: &str) -> Result<()> {
        let dir = &self.vm.running_mut()?.info.dir;
        for file in ["serial.log", "qemu.log", "swtpm.log"] {
            if dir.join(file).exists() {
                fs::copy(dir.join(file), artifacts.join(format!("{label}-{file}")))?;
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

fn qualify(ws: &Workspace, artifacts: &Path) -> Result<()> {
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
    // 4 GiB / 2 vCPUs by default; diagnostic sizes must be explicit.
    let disk = {
        let _install_slot = ilex_devkit::slots::acquire(&ws.vm_name("security-install"))?;
        vmtest::disk(ws)?
    };
    let mut fixture = Fixture::new(ws, &disk, "security-firmware", accelerator, true, true)?;
    let mut qualification = ilex_devkit::qualification::Run::begin(ws, &fixture.spec, artifacts)?;
    let mut result = (|| -> Result<()> {
        let caps = fixture.provider.probe(&fixture.spec)?;
        caps.require_provisioning(Capability::FirmwareEnforcement)?;
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
        let first = match fixture.start(deadline) {
            Ok(seconds) => seconds,
            Err(error) => {
                let _ = fixture.logs(artifacts, "failed-start");
                return Err(error);
            }
        };
        let result = (|| -> Result<()> {
            measurements(fixture.vm.running_mut()?, artifacts, "first")?;
            let vm = fixture.vm.running_mut()?;
            let versions = vm
            .ssh_ok("rpm -q shim-x64 grub2-efi-x64 systemd; uname -r; bootctl status --no-pager; sha256sum /sys/firmware/efi/efivars/PK-* /sys/firmware/efi/efivars/KEK-* /sys/firmware/efi/efivars/db-* /sys/firmware/efi/efivars/dbx-*")?;
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
            measurements(fixture.vm.running_mut()?, artifacts, "warm")?;
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
            let cold = fixture.start(deadline)?;
            measurements(fixture.vm.running_mut()?, artifacts, "cold")?;
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
        unsigned_control(ws, artifacts, accelerator, deadline)?;
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
    for label in ["six-gib", "different-vcpus"] {
        let mut other = regular.clone();
        if label == "six-gib" {
            other.resources.memory_bytes = 6 * 1024 * 1024 * 1024;
        } else {
            other.resources.vcpus += 1;
        }
        let caps = fixture.provider.probe(&other)?;
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
fn measurements(vm: &qemu::Vm, artifacts: &Path, label: &str) -> Result<()> {
    vm.ssh_ok("set -eu; test \"$(od -An -j4 -N1 -tu1 /sys/firmware/efi/efivars/SecureBoot-*)\" -eq 1; test \"$(od -An -j4 -N1 -tu1 /sys/firmware/efi/efivars/SetupMode-*)\" -eq 0; test \"$(cat /sys/class/tpm/tpm0/tpm_version_major)\" = 2; test -c /dev/tpmrm0")?;
    let pcr = vm
        .ssh_ok("cat /sys/class/tpm/tpm0/pcr-sha256/7")?
        .to_ascii_lowercase();
    let out = vm.ssh("cat /sys/kernel/security/tpm0/binary_bios_measurements")?;
    ensure!(out.status.success(), "cannot capture guest TPM event log");
    fs::write(artifacts.join(format!("{label}-eventlog.bin")), &out.stdout)?;
    let replay = security::replay_pcr7(&out.stdout)?;
    ensure!(
        pcr == replay.sha256,
        "PCR7 {pcr} differs from replay {}",
        replay.sha256
    );
    fs::write(
        artifacts.join(format!("{label}-pcr7.json")),
        serde_json::to_vec_pretty(&replay)?,
    )?;
    println!(
        "security {label}: SB=1 SetupMode=0 TPM2 PCR7={pcr}; {} replayed events",
        replay.events
    );
    Ok(())
}

fn unsigned_control(
    ws: &Workspace,
    artifacts: &Path,
    accelerator: Accelerator,
    deadline: Duration,
) -> Result<()> {
    let payload = unsigned_efi();
    ensure!(
        payload[0x98 + 112 + 4 * 8..0x98 + 112 + 5 * 8]
            .iter()
            .all(|b| *b == 0),
        "EFI security directory must be empty"
    );
    fs::write(artifacts.join("unsigned.efi"), &payload)?;
    let source = artifacts.join("unsigned.img");
    fs::write(&source, fat_disk(&payload))?;
    let run = |secure| -> Result<serde_json::Value> {
        let label = if secure {
            "unsigned-on"
        } else {
            "unsigned-off"
        };
        let mut fixture = Fixture::new(
            ws,
            &source,
            &format!("security-{label}"),
            accelerator,
            secure,
            false,
        )?;
        fixture.provider.start(&mut fixture.vm)?;
        let vm = fixture.vm.running_mut()?;
        let start = Instant::now();
        let result = loop {
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
        };
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

/// Minimal position-independent x64 EFI application. It emits a serial marker
/// and loops. Generated PE32+ has no Authenticode directory or external tools.
fn unsigned_efi() -> Vec<u8> {
    let mut b = vec![0u8; 1024];
    fn u16(b: &mut [u8], at: usize, v: u16) {
        b[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn u32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
    b[..2].copy_from_slice(b"MZ");
    u32(&mut b, 0x3c, 0x80);
    b[0x80..0x84].copy_from_slice(b"PE\0\0");
    u16(&mut b, 0x84, 0x8664);
    u16(&mut b, 0x86, 1);
    u16(&mut b, 0x94, 240);
    u16(&mut b, 0x96, 0x22);
    let o = 0x98;
    u16(&mut b, o, 0x20b);
    u32(&mut b, o + 4, 512);
    u32(&mut b, o + 16, 0x1000);
    u32(&mut b, o + 20, 0x1000);
    b[o + 24..o + 32].copy_from_slice(&0x400000u64.to_le_bytes());
    u32(&mut b, o + 32, 4096);
    u32(&mut b, o + 36, 512);
    u16(&mut b, o + 40, 6);
    u16(&mut b, o + 48, 1);
    u32(&mut b, o + 56, 8192);
    u32(&mut b, o + 60, 512);
    u16(&mut b, o + 68, 10);
    u32(&mut b, o + 108, 16);
    let s = o + 240;
    b[s..s + 8].copy_from_slice(b".text\0\0\0");
    u32(&mut b, s + 8, 512);
    u32(&mut b, s + 12, 4096);
    u32(&mut b, s + 16, 512);
    u32(&mut b, s + 20, 512);
    u32(&mut b, s + 36, 0x60000020);
    let mut code = vec![0x66, 0xba, 0xf8, 0x03]; // mov dx, 0x3f8
    for byte in b"ILEX-UNSIGNED-EXECUTED\r\n" {
        code.extend([0xb0, *byte, 0xee]);
    }
    code.extend([0xeb, 0xfe]);
    b[512..512 + code.len()].copy_from_slice(&code);
    b
}

/// An MBR ESP/FAT16 fixture, avoiding loop mounts or privileged host installs.
fn fat_disk(payload: &[u8]) -> Vec<u8> {
    let offset = 2048 * 512;
    let sectors = 32768usize;
    let mut disk = vec![0u8; offset + sectors * 512];
    disk[446 + 4] = 0xef;
    disk[454..458].copy_from_slice(&2048u32.to_le_bytes());
    disk[458..462].copy_from_slice(&(sectors as u32).to_le_bytes());
    disk[510..512].copy_from_slice(&[0x55, 0xaa]);
    let b = &mut disk[offset..];
    b[..3].copy_from_slice(&[0xeb, 0x3c, 0x90]);
    b[3..11].copy_from_slice(b"ILEXTEST");
    b[11..13].copy_from_slice(&512u16.to_le_bytes());
    b[13] = 4;
    b[14..16].copy_from_slice(&1u16.to_le_bytes());
    b[16] = 2;
    b[17..19].copy_from_slice(&512u16.to_le_bytes());
    b[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
    b[21] = 0xf8;
    b[22..24].copy_from_slice(&32u16.to_le_bytes());
    b[24..26].copy_from_slice(&63u16.to_le_bytes());
    b[26..28].copy_from_slice(&255u16.to_le_bytes());
    b[28..32].copy_from_slice(&2048u32.to_le_bytes());
    b[36] = 0x80;
    b[38] = 0x29;
    b[39..43].copy_from_slice(&1u32.to_le_bytes());
    b[43..54].copy_from_slice(b"ILEX EFI   ");
    b[54..62].copy_from_slice(b"FAT16   ");
    b[510..512].copy_from_slice(&[0x55, 0xaa]);
    for fat in [512, 33 * 512] {
        for (i, value) in [0xfff8u16, 0xffff, 0xffff, 0xffff, 0xffff]
            .iter()
            .enumerate()
        {
            b[fat + i * 2..fat + i * 2 + 2].copy_from_slice(&value.to_le_bytes());
        }
    }
    fn entry(b: &mut [u8], at: usize, name: &[u8; 11], dir: bool, cluster: u16, size: u32) {
        b[at..at + 11].copy_from_slice(name);
        b[at + 11] = if dir { 0x10 } else { 0x20 };
        b[at + 26..at + 28].copy_from_slice(&cluster.to_le_bytes());
        b[at + 28..at + 32].copy_from_slice(&size.to_le_bytes());
    }
    entry(b, 65 * 512, b"EFI        ", true, 2, 0);
    let data = 97 * 512;
    entry(b, data, b"BOOT       ", true, 3, 0);
    entry(
        b,
        data + 2048,
        b"BOOTX64 EFI",
        false,
        4,
        payload.len() as u32,
    );
    b[data + 4096..data + 4096 + payload.len()].copy_from_slice(payload);
    disk
}
