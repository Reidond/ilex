//! Exact-ISO encrypted install. A disposable signed local registry delivers the
//! candidate; it never publishes to ghcr or relaxes the shipped reject policy.
use crate::common;
use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::{
    Workspace, cmd,
    core::{Accelerator, Capability, Requirement, TpmLifecycle, VmProvider, VmSpec},
    provider::{LinuxProvider, LinuxVm},
    qemu::{self, VmConfig},
    security,
};
use ilex_vmctl::Vmctl;
use libtest_mimic::Trial;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::Read,
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const GUEST: &str = include_str!("encrypted/guest.py");

pub fn trials() -> Vec<Trial> {
    vec![common::trial("security::encrypted_install", run)]
}

struct Registry {
    id: String,
    owner: String,
}
impl Drop for Registry {
    fn drop(&mut self) {
        let owner = cmd::probe(Command::new("podman").args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"ilex.checkout-owner\"}}",
            &self.id,
        ]));
        if owner.as_deref().map(str::trim) == Some(self.owner.as_str()) {
            let _ = Command::new("podman")
                .args(["rm", "--force", &self.id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
fn registry(ws: &Workspace, image: &str, work: &Path) -> Result<(Registry, Value)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let owner = ws.identity()?.owner;
    let name = ws.vm_name(&format!("enroll-registry-{}", std::process::id()));
    let id = cmd::output(Command::new("podman").args([
        "run",
        "--rm",
        "--detach",
        "--name",
        &name,
        "--label",
        &format!("ilex.checkout-owner={owner}"),
        "--publish",
        &format!("127.0.0.1:{port}:5000"),
        "docker.io/library/registry:2",
    ]))?;
    let registry = Registry {
        id: id.trim().into(),
        owner,
    };
    let local = format!("localhost:{port}/ilex:test");
    let repository = format!("10.0.2.2:{port}/ilex");
    let digestfile = work.join("digest");
    cmd::run(
        Command::new("podman")
            .args(["push", "--tls-verify=false", "--digestfile"])
            .arg(&digestfile)
            .arg(image)
            .arg(&local),
    )?;
    let digest = fs::read_to_string(digestfile)?.trim().to_string();
    ensure!(
        digest.starts_with("sha256:") && digest.len() == 71,
        "invalid candidate digest"
    );
    let key = work.join("cosign");
    cmd::run(
        Command::new("cosign")
            .args(["generate-key-pair", "--output-key-prefix"])
            .arg(&key)
            .env("COSIGN_PASSWORD", ""),
    )?;
    cmd::run(
        Command::new("cosign")
            .args([
                "sign",
                "--yes",
                "--tlog-upload=false",
                "--allow-insecure-registry",
                "--key",
            ])
            .arg(key.with_extension("key"))
            .args(["--sign-container-identity", &repository])
            .arg(format!("localhost:{port}/ilex@{digest}"))
            .env("COSIGN_PASSWORD", ""),
    )?;
    Ok((
        registry,
        json!({"repository": repository, "source":format!("{repository}@{digest}"), "digest":digest, "fixture":"candidate-signed-local-registry"}),
    ))
}

struct Fixture {
    provider: LinuxProvider,
    vm: LinuxVm,
    spec: VmSpec,
}
impl Fixture {
    fn logs(&mut self, artifacts: &Path, label: &str) -> Result<()> {
        let info = &self.vm.running_mut()?.info;
        for file in ["serial.log", "qemu.log", "swtpm.log"] {
            if info.dir.join(file).is_file() {
                fs::copy(
                    info.dir.join(file),
                    artifacts.join(format!("{label}-{file}")),
                )?;
            }
        }
        Ok(())
    }
    fn disk_boot(&mut self) -> Result<()> {
        self.provider.stop(&mut self.vm)?;
        self.spec.iso = None;
        self.spec.additional_disks.clear();
        self.vm = self.provider.create(self.spec.clone())?;
        self.provider.start(&mut self.vm)?;
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = self.provider.destroy_owned(&mut self.vm) {
            eprintln!("encrypted fixture cleanup failed: {e:#}");
        }
    }
}

fn seed(work: &Path, config: &Value) -> Result<(PathBuf, Vec<u8>)> {
    let dir = work.join("seed");
    fs::create_dir(&dir)?;
    let mut passphrase = vec![0u8; 48];
    File::open("/dev/urandom")?.read_exact(&mut passphrase)?;
    for byte in &mut passphrase {
        *byte = b'a' + *byte % 26;
    }
    fs::write(dir.join("passphrase"), &passphrase)?;
    fs::write(dir.join("config.json"), serde_json::to_vec(config)?)?;
    fs::write(dir.join("guest.py"), GUEST)?;
    fs::copy(work.join("cosign.pub"), dir.join("test.pub"))?;
    // Start the private console driver before the shipped completion %post.
    // The source override changes only the test registry and keeps target and
    // signature enforcement. No passphrase bytes occur in ks.cfg or scripts.
    let ks = r#"%post --nochroot --erroronfail
python3 -I -S /run/ilex-encrypted-test/seed/guest.py drive > /run/ilex-encrypted-test/driver.log 2>&1 &
%end
%include /usr/share/anaconda/interactive-defaults.ks
graphical
lang en_US.UTF-8
keyboard us
timezone Etc/UTC --utc
rootpw --lock
zerombr
ignoredisk --only-use=vda
clearpart --all --initlabel --drives=vda
bootloader --append="console=ttyS0,115200n8"
%include /run/ilex-encrypted-test/storage.ks
poweroff
%pre --erroronfail
mkdir -p /run/ilex-seed-boot
mount /dev/disk/by-label/OEMDRV /run/ilex-seed-boot
cp /run/ilex-seed-boot/guest.py /run/ilex-encrypted-guest.py
umount /run/ilex-seed-boot
python3 -I -S /run/ilex-encrypted-guest.py prepare
%end
%post --nochroot --erroronfail
python3 -I -S /run/ilex-encrypted-test/seed/guest.py finish
%end
"#;
    fs::write(dir.join("ks.cfg"), ks)?;
    let seed = work.join("oemdrv.img");
    File::create(&seed)?.set_len(32 << 20)?;
    cmd::run(
        Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "OEMDRV", "-d"])
            .arg(&dir)
            .arg(&seed),
    )?;
    Ok((seed, passphrase))
}

fn extract(seed: &Path, artifacts: &Path, file: &str) -> Result<Vec<u8>> {
    let output = Command::new("debugfs")
        .args(["-R", &format!("cat /public/{file}")])
        .arg(seed)
        .output()?;
    ensure!(
        output.status.success() && !output.stdout.is_empty(),
        "installer evidence missing: {file}"
    );
    fs::write(artifacts.join(format!("installer-{file}")), &output.stdout)?;
    Ok(output.stdout)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct FailureSnapshot {
    schema: u8,
    phase: String,
    drive_step: String,
    errno: Option<u16>,
    helper_errno: Option<u16>,
    uinput_loaded: bool,
    tty8_mode: Option<u8>,
    target_exists: bool,
    layout: String,
    helper_errors: Vec<String>,
    passphrase_prompt: bool,
    recovery_prompt: bool,
    confirmation_prompt: bool,
    ready_prompt: bool,
}

fn failure_snapshot(seed: &Path, published: &Path) -> Result<()> {
    // Failure evidence bypasses the full privacy gate only after strict field,
    // code and numeric validation. Never export raw debugfs output or errors.
    let output = Command::new("debugfs")
        .args(["-R", "cat /public/failure.json"])
        .arg(seed)
        .output()?;
    ensure!(output.status.success() && output.stdout.len() <= 4096);
    let snapshot: FailureSnapshot = serde_json::from_slice(&output.stdout)?;
    ensure!(snapshot.schema == 1);
    ensure!(["prepare", "drive", "finish"].contains(&snapshot.phase.as_str()));
    ensure!(
        [
            "not-started",
            "uinput-load",
            "uinput-open",
            "uinput-create",
            "input-observe",
            "wait-passphrase",
            "capture-before-input",
            "enter-passphrase",
            "wait-recovery",
            "acknowledge-recovery",
            "wait-ready",
            "capture-confirmation",
            "wait-final-scan",
        ]
        .contains(&snapshot.drive_step.as_str())
    );
    ensure!(
        [
            "not-present",
            "encrypted",
            "unencrypted",
            "error",
            "unavailable"
        ]
        .contains(&snapshot.layout.as_str())
    );
    ensure!(snapshot.errno.is_none_or(|value| value <= 4095));
    ensure!(snapshot.helper_errno.is_none_or(|value| value <= 4095));
    ensure!(snapshot.tty8_mode.is_none_or(|value| value <= 3));
    ensure!(snapshot.helper_errors.len() <= 9);
    ensure!(snapshot.helper_errors.iter().all(|code| {
        [
            "var-unencrypted",
            "root-layout",
            "boot-encrypted",
            "crypttab",
            "swap",
            "text-console",
            "tty-required",
            "tool-failed",
            "memory-lock",
        ]
        .contains(&code.as_str())
    }));
    fs::write(
        published.join("failure.json"),
        serde_json::to_vec_pretty(&snapshot)?,
    )?;
    Ok(())
}

fn install(fixture: &mut Fixture, artifacts: &Path, deadline: Duration) -> Result<()> {
    fixture.provider.start(&mut fixture.vm)?;
    let vm = fixture.vm.running_mut()?;
    let ctl = Vmctl::from_info(vm.info.clone());
    let start = Instant::now();
    let mut screenshots = [false; 2];
    loop {
        let serial = fs::read_to_string(vm.info.dir.join("serial.log")).unwrap_or_default();
        ensure!(
            !serial.contains("ILEX-ENCRYPTED-FAILED"),
            "private encrypted fixture failed; secret diagnostics suppressed"
        );
        for (index, (marker, file)) in [
            ("ILEX-ENCRYPTED-COMPLETION-SAFE-SCREEN", "completion.png"),
            (
                "ILEX-ENCRYPTED-RECOVERY-SAFE-SCREEN",
                "recovery-confirmed.png",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if !screenshots[index] && serial.contains(marker) {
                // Guest waits for this explicit nonsecret acknowledgement.
                // No timeout or failure screenshot may capture the visible key.
                ctl.save_screenshot(&artifacts.join(file))?;
                ctl.key("shift")?;
                screenshots[index] = true;
            }
        }
        if let Some(status) = vm.exited()? {
            ensure!(
                status.success()
                    && serial.contains("ILEX-ENCRYPTED-DONE")
                    && screenshots == [true, true],
                "encrypted ISO installation did not finish"
            );
            break;
        }
        ensure!(
            start.elapsed() < deadline,
            "encrypted installer deadline exceeded; qualification remains unqualified"
        );
        thread::sleep(Duration::from_secs(2));
    }
    Ok(())
}

fn guest_json(vm: &qemu::Vm, artifacts: &Path, file: &str, script: &str) -> Result<Value> {
    let bytes = vm.ssh_ok(script)?;
    let value: Value = serde_json::from_str(&bytes)?;
    fs::write(artifacts.join(file), serde_json::to_vec_pretty(&value)?)?;
    Ok(value)
}

fn poweroff(fixture: &mut Fixture) -> Result<()> {
    let vm = fixture.vm.running_mut()?;
    let _ = vm.ssh("systemctl poweroff");
    let start = Instant::now();
    while vm.exited()?.is_none() {
        ensure!(
            start.elapsed() < Duration::from_secs(120),
            "guest poweroff timed out"
        );
        thread::sleep(Duration::from_millis(200));
    }
    fixture.provider.stop(&mut fixture.vm)
}

fn no_tpm(
    ws: &Workspace,
    source: &Path,
    template: &VmSpec,
    work: &Path,
    artifacts: &Path,
    passphrase: &[u8],
) -> Result<()> {
    let disk = work.join("no-tpm.qcow2");
    cmd::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(source)
            .arg(&disk),
    )?;
    let mut spec = template.clone();
    spec.run.push_str("-no-tpm");
    spec.purpose = "encrypted-no-tpm".into();
    spec.tpm = TpmLifecycle::Off;
    // This negative has a separate identity. It measures fallback behavior and
    // cannot mint a passing T6 firmware/TPM qualification for a different profile.
    spec.security_launch = ilex_devkit::core::SecurityLaunch::QualificationFixture;
    spec.boot = ilex_devkit::core::BootSource::Disk {
        path: disk,
        format: ilex_devkit::core::DiskFormat::Qcow2,
        writable: true,
    };
    let provider = LinuxProvider::new(ws.clone())?;
    let vm = provider.create(spec.clone())?;
    let mut fixture = Fixture { provider, vm, spec };
    fixture.provider.start(&mut fixture.vm)?;
    let vm = fixture.vm.running_mut()?;
    let start = Instant::now();
    loop {
        let serial = fs::read_to_string(vm.info.dir.join("serial.log"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if serial.contains("please enter")
            && (serial.contains("passphrase") || serial.contains("password"))
        {
            break;
        }
        ensure!(
            vm.exited()?.is_none() && start.elapsed() < Duration::from_secs(900),
            "no-TPM guest did not request fallback authentication"
        );
        thread::sleep(Duration::from_secs(2));
    }
    let ctl = Vmctl::from_info(vm.info.clone());
    ctl.save_screenshot(&artifacts.join("no-tpm-passphrase-prompt.png"))?;
    ctl.type_text(std::str::from_utf8(passphrase)?)?;
    ctl.key("ret")?;
    vm.wait_ssh(Duration::from_secs(900))?;
    let probe = guest_json(
        vm,
        artifacts,
        "no-tpm-probe.json",
        "ilex-enroll probe --json",
    )?;
    ensure!(
        probe["secure_boot"] == true
            && probe["setup_mode"] == false
            && probe["tpm2"] == false
            && probe["reason"] == "no-tpm",
        "no-TPM mode was not preserved"
    );
    let evidence = guest_json(
        vm,
        artifacts,
        "no-tpm-unlock.json",
        "cat /run/ilex/root-unlock.json",
    )?;
    ensure!(
        evidence["method"] == "unknown",
        "fallback was incorrectly labeled as TPM unlock"
    );
    // Reuse acknowledged recovery during a local completion retry with no
    // TPM. This must defer to passphrase-only mode without generating a key
    // or changing any slot/token. No credential enters SSH, argv or a unit.
    let before = guest_json(
        vm,
        artifacts,
        "no-tpm-enrollment-before.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        before["recovery"]["acknowledged"] == true,
        "recovery acknowledgement missing"
    );
    let uuid = before["uuid"]
        .as_str()
        .context("missing no-TPM root UUID")?;
    ensure!(
        uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "invalid no-TPM root UUID"
    );
    let token_command = format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}");
    let tokens_before = guest_json(vm, artifacts, "no-tpm-tokens-before.json", &token_command)?;
    vm.ssh_ok("chvt 8; systemd-run --quiet --remain-after-exit --unit=ilex-enroll-no-tpm-completion /usr/bin/ilex-enroll complete --root / --tty /dev/tty8")?;
    vm.ssh_ok("timeout 60 sh -c 'until grep -aFq \"Re-enter the disk passphrase\" /dev/vcs8; do sleep 1; done'")?;
    ctl.type_text(std::str::from_utf8(passphrase)?)?;
    ctl.key("ret")?;
    vm.ssh_ok("timeout 120 sh -c 'until grep -aFq \"Installation completion is ready.\" /dev/vcs8; do sleep 1; done'")?;
    ctl.save_screenshot(&artifacts.join("no-tpm-passphrase-only.png"))?;
    ctl.key("ret")?;
    vm.ssh_ok("set -eu; timeout 30 sh -c 'until test \"$(systemctl show -p SubState --value ilex-enroll-no-tpm-completion)\" = exited; do sleep 1; done'; test \"$(systemctl show -p Result --value ilex-enroll-no-tpm-completion)\" = success; systemctl stop ilex-enroll-no-tpm-completion")?;
    let after = guest_json(
        vm,
        artifacts,
        "no-tpm-enrollment.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    let setup = guest_json(
        vm,
        artifacts,
        "no-tpm-setup.json",
        "cat /etc/ilex/setup.json",
    )?;
    ensure!(
        after["state"] == "unavailable"
            && after["reason"] == "no-tpm"
            && after["recovery"] == before["recovery"]
            && setup["security"]["tpm"]["status"] == "passphrase-only",
        "no-TPM completion did not preserve passphrase-only mode"
    );
    let tokens_after = guest_json(vm, artifacts, "no-tpm-tokens.json", &token_command)?;
    ensure!(
        tokens_after == tokens_before,
        "no-TPM completion changed LUKS slots/tokens"
    );
    vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; ilex-enroll layout --root /; test -z \"$(systemctl --failed --no-legend --plain)\"")?;
    fixture.logs(artifacts, "no-tpm")?;
    poweroff(&mut fixture)
}

fn assertions(
    fixture: &mut Fixture,
    artifacts: &Path,
    expected: &Value,
    installer_probe: &Value,
) -> Result<()> {
    let vm = fixture.vm.running_mut()?;
    vm.wait_ssh(Duration::from_secs(900))?; // No keyboard/password on first boot.
    let bootc = guest_json(vm, artifacts, "bootc.json", "bootc status --json")?;
    ensure!(
        bootc["status"]["booted"]["image"]["imageDigest"] == expected["digest"],
        "installed digest differs from signed candidate"
    );
    ensure!(
        bootc["spec"]["image"]["signature"] == "containerPolicy",
        "target origin is not enforcing"
    );
    let probe = guest_json(vm, artifacts, "probe.json", "ilex-enroll probe --json")?;
    ensure!(
        probe["verdict"] == "eligible" && probe["pcr7"] == installer_probe["pcr7"],
        "ISO/installed PCR 7 mismatch"
    );
    let raw = vm.ssh("cat /sys/kernel/security/tpm0/binary_bios_measurements")?;
    ensure!(raw.status.success(), "installed event log unavailable");
    fs::write(artifacts.join("eventlog.bin"), &raw.stdout)?;
    let replay = security::replay_pcr7(&raw.stdout)?;
    ensure!(
        probe["pcr7"] == replay.sha256,
        "installed event log replay mismatch"
    );
    fs::write(
        artifacts.join("replay.json"),
        serde_json::to_vec_pretty(&replay)?,
    )?;
    let layout = guest_json(vm, artifacts, "layout.json", "ilex-enroll layout --root /")?;
    let evidence = guest_json(
        vm,
        artifacts,
        "root-unlock.json",
        "cat /run/ilex/root-unlock.json",
    )?;
    ensure!(
        evidence["schema"] == 1
            && evidence["uuid"] == layout["uuid"]
            && evidence["boot_id"] == vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?.trim()
            && evidence["method"] == "tpm2",
        "first boot lacks direct TPM unlock evidence"
    );
    let uuid = layout["uuid"].as_str().context("no root UUID")?;
    ensure!(
        uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "invalid root UUID"
    );
    let tokens = guest_json(
        vm,
        artifacts,
        "tokens.json",
        &format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}"),
    )?;
    ensure!(
        tokens["tokens"]
            .as_array()
            .context("no tokens")?
            .iter()
            .any(|t| t["pcr7_sha256"] == true),
        "explicit token policy missing"
    );
    let state = guest_json(
        vm,
        artifacts,
        "enrollment.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        state["state"] == "pending" && state["recovery"]["acknowledged"] == true,
        "enrollment was not acknowledged and pending verification"
    );
    ensure!(
        evidence["token"]
            .as_str()
            .and_then(|id| id.parse::<u64>().ok())
            == state["tpm"]["token"].as_u64()
            && evidence["slot"] == state["tpm"]["slot"],
        "unlock evidence differs from the enrolled token/slot"
    );
    vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; grep -qw 'rd.luks.options=tpm2-device=auto' /proc/cmdline; ! grep -q headless /etc/crypttab; test \"$(systemctl is-system-running --wait)\" = running; test -z \"$(systemctl --failed --no-legend --plain)\"; ilex-enroll --self-check")?;
    vm.ssh_ok(&format!("grep 'UUID={uuid}' /etc/crypttab | grep -q tpm2-device=auto; ! grep -q headless /proc/cmdline"))?;
    let topology = vm.ssh_ok("set -eu; lsblk --json --output NAME,TYPE,FSTYPE,UUID,MOUNTPOINTS; cat /etc/crypttab; cat /proc/cmdline; cat /proc/swaps; for path in /sysroot /var /boot /boot/efi; do findmnt --target \"$path\"; done")?;
    fs::write(artifacts.join("storage.txt"), topology)?;
    Ok(())
}

fn evidence_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files.extend(evidence_files(&entry.path())?);
        } else {
            ensure!(kind.is_file(), "unsupported evidence file type");
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn clear_evidence(directory: &Path) -> Result<()> {
    // This is only the namespace returned for this checkout's encrypted tier.
    // Remove stale evidence so a failed retry cannot inherit an earlier green.
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn run() -> Result<()> {
    let ws = common::workspace()?;
    let published = qemu::artifact_dir(&ws, "encrypted-install")?;
    clear_evidence(&published)?;
    fs::write(
        published.join("result.json"),
        b"{\"qualification\":\"unqualified\",\"reason\":\"run not complete\"}\n",
    )?;
    let run = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let work = ws.ensure_state()?.join("encrypted-fixtures").join(&run);
    fs::create_dir_all(&work)?;
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
    // Until both privacy gates pass, raw diagnostics remain inside the private
    // fixture directory. CI always uploads only `published`.
    let artifacts = work.join("evidence");
    fs::create_dir(&artifacts)?;
    let start = Instant::now();
    let mut stage = "platform";
    let mut result = (|| {
        let iso = PathBuf::from(std::env::var_os("ILEX_ISO").context("ILEX_ISO required")?);
        ensure!(iso.is_file(), "exact installer ISO missing");
        let disk = work.join("root.qcow2");
        cmd::run(
            Command::new("qemu-img")
                .args(["create", "-q", "-f", "qcow2"])
                .arg(&disk)
                .arg("40G"),
        )?;
        let mut config = VmConfig::new("encrypted-install", &disk);
        config.snapshot = false;
        config.memory = "6G".into();
        config.cdrom = Some(iso.clone());
        let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
        spec.run = run.clone();
        spec.accelerator = match std::env::var("ILEX_SECURITY_ACCELERATOR").as_deref() {
            Ok("tcg") => Accelerator::Tcg,
            Ok("kvm") | Err(_) => Accelerator::Native,
            Ok(_) => bail!("explicit kvm/tcg accelerator required"),
        };
        spec.secure_boot = Requirement::Required;
        spec.tpm = TpmLifecycle::Persistent {
            bundle_id: run.clone(),
        };
        let provider = LinuxProvider::new(ws.clone())?;
        let caps = provider.probe(&spec)?;
        fs::write(
            artifacts.join("backend.json"),
            serde_json::to_vec_pretty(&caps)?,
        )?;
        caps.require(Capability::FirmwareEnforcement)?;
        caps.require(Capability::PersistentTpm)?; // Exact 6 GiB / 2 vCPU profile.
        stage = "fixture";
        let image = common::image()?;
        let (_registry, expected) = registry(&ws, &image, &work)?;
        fs::write(
            artifacts.join("candidate.json"),
            serde_json::to_vec_pretty(&expected)?,
        )?;
        let (seed_path, passphrase) = seed(&work, &expected)?;
        spec.additional_disks.push(ilex_devkit::core::AttachedDisk {
            path: seed_path.clone(),
            format: ilex_devkit::core::DiskFormat::Raw,
            blank_bytes: None,
            read_only: false,
        });
        let vm = provider.create(spec.clone())?;
        let mut fixture = Fixture { provider, vm, spec };
        stage = "installation";
        let deadline = Duration::from_secs(
            std::env::var("ILEX_ENCRYPTED_INSTALL_TIMEOUT")
                .unwrap_or_else(|_| "10800".into())
                .parse()?,
        );
        let installed = install(&mut fixture, &artifacts, deadline);
        fixture.logs(&artifacts, "installer")?;
        if installed.is_err() {
            fixture.provider.stop(&mut fixture.vm)?;
            // A missing/invalid snapshot adds no public data and never masks
            // the original failed qualification.
            let _ = failure_snapshot(&seed_path, &published);
        }
        installed?;
        fixture.provider.stop(&mut fixture.vm)?;
        let installer_probe: Value =
            serde_json::from_slice(&extract(&seed_path, &artifacts, "probe.json")?)?;
        ensure!(
            installer_probe["verdict"] == "eligible",
            "installer probe unavailable"
        );
        let replay = security::replay_pcr7(&extract(&seed_path, &artifacts, "eventlog.bin")?)?;
        ensure!(
            installer_probe["pcr7"] == replay.sha256,
            "installer PCR replay mismatch"
        );
        fs::write(
            artifacts.join("installer-replay.json"),
            serde_json::to_vec_pretty(&replay)?,
        )?;
        for file in [
            "layout.json",
            "enrollment.json",
            "setup.json",
            "versions.txt",
            "leak-scan.json",
        ] {
            extract(&seed_path, &artifacts, file)?;
        }
        let scan: Value =
            serde_json::from_slice(&fs::read(artifacts.join("installer-leak-scan.json"))?)?;
        ensure!(
            scan["secret_found"] == false && scan["journal_scanned"] == true,
            "private guest secret scan failed"
        );
        stage = "first_installed_boot";
        fixture.disk_boot()?;
        let booted = assertions(&mut fixture, &artifacts, &expected, &installer_probe);
        fixture.logs(&artifacts, "installed")?;
        booted?;
        stage = "cold_restart";
        poweroff(&mut fixture)?;
        fixture.disk_boot()?;
        let cold = artifacts.join("cold");
        fs::create_dir_all(&cold)?;
        assertions(&mut fixture, &cold, &expected, &installer_probe)?;
        fixture.logs(&artifacts, "cold")?;
        poweroff(&mut fixture)?;
        stage = "no_tpm_fallback";
        no_tpm(&ws, &disk, &fixture.spec, &work, &artifacts, &passphrase)?;
        // Artifacts cannot contain the test passphrase. Never print a matching
        // path/line or the bytes, including on a failed assertion.
        for path in evidence_files(&artifacts)? {
            ensure!(
                !fs::read(path)?
                    .windows(passphrase.len())
                    .any(|w| w == passphrase),
                "secret found in artifacts; details suppressed"
            );
        }
        let iso_hash = cmd::output(Command::new("sha256sum").arg(&iso))?;
        fs::write(artifacts.join("iso-sha256.txt"), iso_hash)?;
        Ok(())
    })();
    if result.is_ok() {
        let export = (|| -> Result<()> {
            for path in evidence_files(&artifacts)? {
                let target = published.join(path.strip_prefix(&artifacts)?);
                fs::create_dir_all(target.parent().context("evidence has no parent")?)?;
                fs::copy(path, target)?;
            }
            Ok(())
        })();
        if let Err(error) = export {
            clear_evidence(&published)?;
            stage = "evidence_export";
            result = Err(error.context("publishing privacy-checked evidence"));
        }
    }
    // Credentials, signing key, seed, disk and unverified diagnostics are never
    // artifacts. Cleanup failure must not leave a passing result behind.
    if let Err(error) = fs::remove_dir_all(&work) {
        stage = "cleanup";
        result = Err(error.into());
    }
    fs::write(
        published.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"qualification": if result.is_ok() {"passed"} else {"unqualified"}, "stage":stage, "elapsed_seconds":start.elapsed().as_secs_f64(), "error":result.as_ref().err().map(|e|format!("{e:#}")), "scope":"exact ISO, signed local candidate, first installed boot; physical hardware remains unqualified"}),
        )?,
    )?;
    result
}
