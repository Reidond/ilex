//! One encrypted Anaconda install, two signed deployments, one persistent TPM.
use super::*;

const BOOT_EVIDENCE: &str = include_str!("lifecycle-boot.py");

pub(super) fn run() -> Result<()> {
    for program in ["openssl", "cosign", "mkfs.ext4", "debugfs"] {
        cmd::require(program)?;
    }
    run_fixture(false, false, false, false, FixtureCase::UpgradeRollback)
}

pub(super) fn valid_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) fn source_image(candidate: &str) -> Result<String> {
    // Require the operator to name the compatibility floor explicitly. A
    // moving tag or a second build of the same kernel is not an update proof.
    let source = std::env::var("ILEX_ENCRYPTED_UPGRADE_FROM")
        .context("set ILEX_ENCRYPTED_UPGRADE_FROM to an older qualifying image")?;
    let podman = |image: &str| {
        cmd::output(Command::new("podman").args([
            "run",
            "--pull=never",
            "--rm",
            "--read-only",
            "--network=none",
            "--entrypoint",
            "/bin/sh",
            image,
            "-ec",
            "for k in /usr/lib/modules/*; do test ! -f \"$k/vmlinuz\" || basename \"$k\"; done",
        ]))
    };
    let old = podman(&source).context("inspect the locally available baseline kernel")?;
    let new = podman(candidate).context("inspect the locally available update kernel")?;
    ensure!(
        old.lines().count() == 1 && new.lines().count() == 1 && old != new,
        "baseline and candidate must each carry one distinct kernel release"
    );
    Ok(source)
}

pub(super) fn record_inputs(evidence: &Path, baseline: &str, update: &str) -> Result<()> {
    let record = |reference: &str| -> Result<Value> {
        let metadata = cmd::output(Command::new("podman").args([
            "image",
            "inspect",
            "--format",
            "{{json .}}",
            reference,
        ]))?;
        let metadata: Value = serde_json::from_str(&metadata)?;
        Ok(json!({
            "manifest_digest": metadata["Digest"],
            "image_id": metadata["Id"],
            "revision": metadata["Config"]["Labels"]["org.opencontainers.image.revision"],
        }))
    };
    let inputs = json!({"baseline":record(baseline)?, "update":record(update)?});
    fs::write(
        evidence.join("input-images.json"),
        serde_json::to_vec_pretty(&inputs)?,
    )?;
    Ok(())
}

pub(super) fn home_password() -> Result<Secret> {
    let mut secret = Secret::empty()?;
    for byte in b"Ix9-" {
        secret.push(*byte)?;
    }
    let mut random = File::open("/dev/urandom")?;
    for _ in 0..24 {
        let mut byte = [0];
        random.read_exact(&mut byte)?;
        let hex = b"0123456789abcdef";
        secret.push(hex[usize::from(byte[0] >> 4)])?;
        secret.push(hex[usize::from(byte[0] & 15)])?;
    }
    Ok(secret)
}

#[allow(clippy::too_many_arguments)] // Shared state from the exact-ISO fixture.
pub(super) fn qualify(
    ws: &Workspace,
    fixture: &mut Fixture,
    evidence: &Path,
    registry: &Registry,
    baseline: &Value,
    update: &Value,
    installer_probe: &Value,
    tokens: &Value,
    recovery: &Secret,
    home_password: &Secret,
    credentials: &Value,
    stage: &mut &'static str,
) -> Result<()> {
    let uuid = tokens["uuid"]
        .as_str()
        .context("installer root UUID missing")?;
    ensure!(
        uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "invalid installer root UUID"
    );
    ensure!(
        baseline["digest"] != update["digest"],
        "same deployment digest"
    );
    fs::write(
        evidence.join("update.json"),
        serde_json::to_vec_pretty(update)?,
    )?;
    let mut first_kernel = None;
    let mut home_data = None;
    let mut boots = Vec::new();
    let mut header = None;

    *stage = "baseline_cold_boot";
    fixture.disk_boot()?;
    for (phase, manifest) in [
        ("baseline", baseline),
        ("upgrade", update),
        ("rollback", baseline),
    ] {
        let auto = evidence.join(phase);
        fs::create_dir(&auto)?;
        *stage = match phase {
            "baseline" => "baseline_automatic_unlock",
            "upgrade" => "upgrade_automatic_unlock",
            _ => "rollback_automatic_unlock",
        };
        assertions(fixture, &auto, manifest, installer_probe)?;
        let vm = fixture.vm.running_mut()?;
        vm.ssh_ok("timeout 30s systemctl mask --now bootc-fetch-apply-updates.timer")?;
        fs::write(
            auto.join("versions.txt"),
            vm.ssh_ok("rpm -q systemd cryptsetup shim-x64 grub2-efi-x64; cat /usr/lib/ilex/security/tools.json")?,
        )?;
        let payload = boot_payload(vm, &auto)?;
        record_boot(vm, &mut boots)?;
        ensure!(
            !payload["kernel_release"].is_null(),
            "running kernel release absent"
        );
        if let Some(first) = &first_kernel {
            if phase == "upgrade" {
                compare_kernels(first, &payload)?;
                let bootc: Value = serde_json::from_slice(&fs::read(auto.join("bootc.json"))?)?;
                ensure!(
                    bootc["status"]["rollback"]["image"]["imageDigest"] == baseline["digest"],
                    "upgrade did not retain the original deployment"
                );
            } else {
                ensure!(
                    first == &payload,
                    "rollback did not restore both original boot payloads"
                );
            }
        } else {
            first_kernel = Some(payload.clone());
        }
        check_tokens(vm, &auto, uuid, tokens, &mut header)?;
        home(&mut home_data, vm, &auto, home_password)?;
        guest_privacy(vm, &auto, "automatic", credentials)?;
        poweroff(fixture, evidence, phase)?;

        *stage = match phase {
            "baseline" => "baseline_recovery_key",
            "upgrade" => "upgrade_recovery_key",
            _ => "rollback_recovery_key",
        };
        let manual = auto.join("recovery");
        fs::create_dir(&manual)?;
        let (vars, saved) = recovery_boot(ws, fixture, &manual, recovery)?;
        let vm = fixture.vm.running_mut()?;
        let boot = record_boot(vm, &mut boots)?;
        let status = guest_json(vm, &manual, "bootc.json", "bootc status --json")?;
        ensure!(
            status["status"]["booted"]["image"]["imageDigest"] == manifest["digest"]
                && status["spec"]["image"]["signature"] == "containerPolicy",
            "recovery boot did not retain deployment and signature enforcement"
        );
        ensure!(
            boot_payload(vm, &manual)? == payload,
            "recovery used a different boot payload"
        );
        let probe = guest_json(vm, &manual, "probe.json", "ilex-enroll probe --json")?;
        let pcr = vm.ssh_ok("cat /sys/class/tpm/tpm0/pcr-sha256/7")?;
        ensure!(
            probe["secure_boot"] == false
                && probe["tpm2"] == true
                && pcr.len() == 64
                && pcr.bytes().all(|b| b.is_ascii_hexdigit())
                && !installer_probe["pcr7"]
                    .as_str()
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(&pcr)),
            "recovery fixture did not change Secure Boot and PCR 7"
        );
        fs::write(manual.join("pcr7.txt"), pcr)?;
        let unlock = guest_json(
            vm,
            &manual,
            "root-unlock.json",
            "cat /run/ilex/root-unlock.json",
        )?;
        ensure!(
            unlock["schema"] == 1
                && unlock["uuid"] == uuid
                && unlock["boot_id"] == boot
                && unlock["method"] == "fallback",
            "the recovery key did not unlock the expected root on this boot"
        );
        check_tokens(vm, &manual, uuid, tokens, &mut header)?;
        home(&mut home_data, vm, &manual, home_password)?;
        guest_privacy(vm, &manual, "recovery", credentials)?;
        poweroff(fixture, evidence, &format!("{phase}-recovery"))?;
        fs::write(vars, saved)?;

        *stage = "restored_policy_automatic_unlock";
        fixture.disk_boot()?;
        let restored = auto.join("restored");
        fs::create_dir(&restored)?;
        assertions(fixture, &restored, manifest, installer_probe)?;
        let vm = fixture.vm.running_mut()?;
        record_boot(vm, &mut boots)?;
        check_tokens(vm, &restored, uuid, tokens, &mut header)?;
        ensure!(
            boot_payload(vm, &restored)? == payload,
            "restored boot payload changed"
        );
        home(&mut home_data, vm, &restored, home_password)?;
        match phase {
            "baseline" => {
                *stage = "verify_signature_and_stage_upgrade";
                stage_upgrade(vm, &restored, registry, baseline, update)?;
            }
            "upgrade" => {
                *stage = "stage_rollback";
                vm.ssh_ok("timeout -k 10s 120s bootc rollback")?;
            }
            _ => {}
        }
        guest_privacy(vm, &restored, "restored", credentials)?;
        poweroff(fixture, evidence, &format!("{phase}-restored"))?;
        if phase != "rollback" {
            fixture.disk_boot()?;
        }
    }
    Ok(())
}

fn record_boot(vm: &qemu::Vm, boots: &mut Vec<String>) -> Result<String> {
    let boot = vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?;
    ensure!(!boots.contains(&boot), "reused a previous boot ID");
    boots.push(boot.clone());
    Ok(boot)
}

fn boot_payload(vm: &qemu::Vm, path: &Path) -> Result<Value> {
    vm.ssh_ok(common::BOOT_MOUNT_CHECK)?;
    let cmd = format!(
        "timeout -k 10s 120s python3 -I -S - <<'ILEX_BOOT_EVIDENCE'\n{BOOT_EVIDENCE}\nILEX_BOOT_EVIDENCE"
    );
    guest_json(vm, path, "boot-payload.json", &cmd)
}

fn compare_kernels(prior: &Value, new: &Value) -> Result<()> {
    for key in ["kernel_release", "kernel_sha256", "initramfs_sha256"] {
        ensure!(
            new[key].is_string() && prior[key].is_string() && new[key] != prior[key],
            "actual upgrade did not change {key}"
        );
    }
    Ok(())
}

fn check_tokens(
    vm: &qemu::Vm,
    path: &Path,
    uuid: &str,
    original: &Value,
    header: &mut Option<String>,
) -> Result<()> {
    let observed = guest_json(
        vm,
        path,
        "lifecycle-tokens.json",
        &format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}"),
    )?;
    ensure!(
        observed == *original,
        "root UUID or LUKS token/slot metadata changed"
    );
    let digest = header_digest(vm, uuid)?;
    if let Some(prior) = header {
        ensure!(*prior == digest, "LUKS2 header metadata changed");
    } else {
        *header = Some(digest.clone());
    }
    fs::write(path.join("luks-header-sha256.txt"), digest)?;
    Ok(())
}

fn home(expected: &mut Option<Value>, vm: &qemu::Vm, path: &Path, password: &Secret) -> Result<()> {
    let observed = crate::homed::guest(
        vm,
        json!({
            "action":"encrypted-lifecycle", "create":expected.is_none(),
            "secrets":[std::str::from_utf8(password.bytes())?],
        }),
    )?;
    ensure!(observed["data_sha256"].is_string(), "home checksum missing");
    if let Some(expected) = expected {
        ensure!(
            *expected == observed,
            "encrypted home data or identity changed"
        );
    } else {
        *expected = Some(observed.clone());
    }
    fs::write(
        path.join("homed.json"),
        serde_json::to_vec_pretty(&observed)?,
    )?;
    Ok(())
}

fn stage_upgrade(
    vm: &qemu::Vm,
    path: &Path,
    registry: &Registry,
    baseline: &Value,
    update: &Value,
) -> Result<()> {
    let source = update["source"].as_str().context("update source missing")?;
    let repository = update["repository"]
        .as_str()
        .context("update repository missing")?;
    let key = qemu::base64(&fs::read(registry.work.join("cosign.pub"))?);
    vm.ssh_ok(&format!(r#"set -eu
install -d /etc/containers/registries.conf.d /etc/containers/registries.d /etc/pki/containers
printf %s '{key}' | base64 -d > /etc/pki/containers/ilex-fixture.pub
python3 -I -S - '{repository}' <<'PY'
import json, sys
from pathlib import Path
p = Path('/etc/containers/policy.json')
policy = json.loads(p.read_text())
assert policy['default'] == [{{'type': 'reject'}}]
policy['transports']['docker'][sys.argv[1]] = [{{
    'type': 'sigstoreSigned', 'keyPath': '/etc/pki/containers/ilex-fixture.pub',
    'signedIdentity': {{'type': 'matchRepository'}},
}}]
p.write_text(json.dumps(policy) + '\n')
PY
printf '[[registry]]\nlocation = "%s"\ninsecure = true\n' '{repository}' > /etc/containers/registries.conf.d/99-ilex-fixture.conf
printf 'docker:\n  %s:\n    use-sigstore-attachments: true\n' '{repository}' > /etc/containers/registries.d/ilex-fixture.yaml
"#))?;
    let command =
        format!("timeout -k 15s 1200s bootc switch --enforce-container-sigpolicy {source}");
    let rejected = vm.ssh(&command)?;
    let reason = String::from_utf8_lossy(&rejected.stderr).to_ascii_lowercase();
    ensure!(
        rejected.status.code() == Some(1)
            && (reason.contains("signature") || reason.contains("sigstore")),
        "unsigned target did not produce a signature rejection"
    );
    let unchanged = guest_json(vm, path, "unsigned-rejected.json", "bootc status --json")?;
    ensure!(
        unchanged["status"]["staged"].is_null()
            && unchanged["status"]["booted"]["image"]["imageDigest"] == baseline["digest"],
        "unsigned switch staged or changed the installed deployment"
    );
    registry.sign(update["digest"].as_str().context("update digest missing")?)?;
    vm.ssh_ok(&command)?;
    let staged = guest_json(vm, path, "signed-staged.json", "bootc status --json")?;
    ensure!(
        staged["status"]["booted"]["image"]["imageDigest"] == baseline["digest"]
            && staged["status"]["staged"]["image"]["imageDigest"] == update["digest"]
            && staged["status"]["staged"]["image"]["image"]["signature"] == "containerPolicy",
        "signed switch did not stage the expected enforcing deployment"
    );
    Ok(())
}
