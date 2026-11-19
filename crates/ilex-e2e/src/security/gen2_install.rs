use super::{BOOT, Media, READY, exec, file_digest, grep_secret, save_frame, wait_screen, write};
use anyhow::{Result, ensure};
use ilex_devkit::{Workspace, core::*};
use ilex_tooling_windows::gen2::{self, Provider};
use serde_json::json;
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub fn transition(
    ws: &Workspace,
    provider: &Provider,
    resources: Resources,
    iso: &Path,
    artifacts: &Path,
) -> Result<()> {
    let mut spec = gen2::spec(
        &ws.identity()?,
        "security-iso-install",
        resources,
        BootSource::BlankDisk {
            bytes: 32 << 30,
            format: DiskFormat::Vhdx,
        },
    )?;
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.iso = Some(iso.into());
    spec.network = NetworkIntent::User { ssh: true };
    spec.ssh = Some(SshProvisioning {
        user: "root".into(),
        public_key: String::new(),
        recipe_hash: digest(include_bytes!("gen2-install.ks")),
    });
    let mut media = Media::new(&ws.ensure_state()?.join("gen2"), &spec.run)?;
    media.track(&spec.run);
    let result = (|| -> Result<()> {
        let files = media.0.join("oemdrv");
        fs::create_dir(&files)?;
        fs::write(files.join("ks.cfg"), include_bytes!("gen2-install.ks"))?;
        let private_key = recovery_key(&files)?;
        let raw_answers = media.0.join("answers.raw");
        fs::File::create(&raw_answers)?.set_len(32 << 20)?;
        ensure!(
            ext4_command("mke2fs")
                .args(["-q", "-t", "ext4", "-F", "-L", "OEMDRV", "-d"])
                .arg(ext4_path(&files)?)
                .arg(ext4_path(&raw_answers)?)
                .output()?
                .status
                .success(),
            "could not build the writable OEMDRV fixture"
        );
        let answers = media.0.join("answers.vhdx");
        gen2::vhdx(&raw_answers, &answers, "raw")?;
        let original = file_digest(&answers)?;
        let mut vm = provider.create_installer(spec, &answers)?;
        let start = Instant::now();
        let handle = provider.start(&mut vm)?;
        write(
            artifacts,
            "transition-input.json",
            &json!({"vm":handle, "iso":file_digest(iso)?, "oemdrv":original}),
        )?;
        // Fail closed on attempts to change media while running.
        ensure!(
            provider.boot_installed(&mut vm, false).is_err(),
            "running installer media change was accepted"
        );
        ensure!(
            provider
                .with_installer_evidence(&mut vm, |_| Ok(()))
                .is_err(),
            "running installer OEMDRV readback was accepted"
        );
        // A completed kickstart powers off. Post-install failures keep the
        // guest running and leave a diagnostic frame rather than passing.
        if let Err(error) = vm.wait_off(Duration::from_secs(1500)) {
            let _ = wait_screen(&vm, artifacts, "transition-stalled", Duration::ZERO, |_| {
                true
            });
            return Err(error.context("waiting for Anaconda to complete and power off"));
        }
        let installed = start.elapsed().as_secs_f64();
        provider.stop(&mut vm)?;
        ensure!(
            provider
                .with_installer_evidence(&mut vm, |_| Ok(()))
                .is_err(),
            "attached OEMDRV readback was accepted"
        );
        ensure!(
            provider.boot_installed(&mut vm, false)? == handle,
            "transition changed VM identity"
        );
        // Retry also verifies ejection and the sole boot target.
        ensure!(
            provider.boot_installed(&mut vm, false)? == handle,
            "transition retry changed VM identity"
        );
        let OemdrvEvidence {
            guest_boot: media_boot,
            plaintext: canary,
            envelope,
            snapshot_path: private_path,
        } = provider.with_installer_evidence(&mut vm, |raw| read_evidence(raw, &private_key))?;
        ensure!(
            !private_path.try_exists()? && file_digest(&answers)? == original,
            "OEMDRV readback retained its private copy or modified its seed"
        );
        let start = Instant::now();
        ensure!(
            provider.start(&mut vm)? == handle,
            "disk boot changed VM identity"
        );
        let boot = vm.wait_ssh(None, READY)?;
        let installed_boot = start.elapsed().as_secs_f64();
        let status = exec(&vm, "bootc status --format json")?;
        let bootc: serde_json::Value = serde_json::from_slice(&status)?;
        let deployment = &bootc["status"]["booted"]["image"];
        let manifest = deployment["imageDigest"].as_str().unwrap_or_default();
        ensure!(
            deployment["image"]["signature"] == "containerPolicy"
                && manifest.starts_with("sha256:")
                && manifest.len() == 71,
            "installed deployment did not retain its verified image origin"
        );
        fs::write(artifacts.join("transition-bootc.json"), &status)?;
        let trace = exec(
            &vm,
            r#"set -eu
cat /var/lib/ilex-gen2/installer-boot
test -c /dev/tpmrm0
test "$(od -An -tu1 -j4 -N1 /sys/firmware/efi/efivars/SecureBoot-*)" -eq 1
findmnt -n -o SOURCE /
test "$(lsblk -dn -o TYPE | grep -c rom || :)" -eq 0
test ! -e /dev/disk/by-label/OEMDRV
"#,
        )?;
        fs::write(artifacts.join("transition-guest.txt"), &trace)?;
        let installer_boot =
            String::from_utf8(exec(&vm, "cat /var/lib/ilex-gen2/installer-boot")?)?
                .trim()
                .to_owned();
        ensure!(
            !installer_boot.is_empty() && installer_boot == media_boot && installer_boot != boot,
            "installed guest did not start a new boot"
        );
        ensure!(
            exec(
                &vm,
                "systemd-creds decrypt --name=ilex-gen2-install /var/lib/ilex-gen2/installer.cred -"
            )? == b"ilex-gen2-installer-proof",
            "credential did not survive installation and same-VM disk boot"
        );
        write(
            artifacts,
            "transition.json",
            &json!({"vm":handle,"installer_boot":installer_boot,"installed_boot":boot,"manifest_digest":manifest,
            "install_seconds":installed,"disk_to_ssh_seconds":installed_boot,"unsealed":true,"settings":vm.status()?}),
        )?;
        save_frame(artifacts, "transition-installed", &vm.capture()?, None)?;
        let scanned = grep_secret(artifacts, std::str::from_utf8(&canary)?)?;
        for entry in fs::read_dir(artifacts)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                ensure!(
                    !fs::read(entry.path())?
                        .windows(envelope.len())
                        .any(|bytes| bytes == envelope),
                    "private OEMDRV envelope was published as an artifact"
                );
            }
        }
        write(
            artifacts,
            "oemdrv-readback.json",
            &json!({"guest_marker":media_boot,"envelope_bytes":envelope.len(),"decrypted_fingerprint_verified":true,"private_snapshot_removed":true,"parent_unchanged":true,"files_scanned":scanned,"plaintext_matches":0,"envelope_matches":0}),
        )?;
        vm.poweroff(BOOT)?;
        provider.destroy_owned(&mut vm)
    })();
    media.finish(provider, result)
}

fn ext4_command(name: &str) -> Command {
    if let Some(distro) = std::env::var_os("ILEX_GEN2_EXT4_WSL") {
        let mut command = Command::new("wsl.exe");
        command
            .arg("--distribution")
            .arg(distro)
            .args(["--exec", &format!("/usr/sbin/{name}")]);
        command
    } else {
        Command::new(format!("{name}.exe"))
    }
}

fn ext4_path(path: &Path) -> Result<OsString> {
    let Some(distro) = std::env::var_os("ILEX_GEN2_EXT4_WSL") else {
        return Ok(path.as_os_str().into());
    };
    let output = Command::new("wsl.exe")
        .arg("--distribution")
        .arg(distro)
        .args(["--exec", "wslpath", "-a", "-u"])
        .arg(path)
        .output()?;
    ensure!(
        output.status.success(),
        "could not resolve the ext4 fixture path"
    );
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

fn recovery_key(files: &Path) -> Result<Vec<u8>> {
    let private = Command::new("openssl.exe")
        .args([
            "genpkey",
            "-algorithm",
            "RSA",
            "-pkeyopt",
            "rsa_keygen_bits:2048",
        ])
        .output()?;
    ensure!(
        private.status.success() && !private.stdout.is_empty(),
        "fixture recovery key generation failed"
    );
    let mut public = Command::new("openssl.exe")
        .args(["pkey", "-pubout", "-out"])
        .arg(files.join("recovery.pub"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    public
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("missing fixture public key pipe"))?
        .write_all(&private.stdout)?;
    ensure!(
        public.wait()?.success(),
        "fixture public key generation failed"
    );
    Ok(private.stdout)
}

struct OemdrvEvidence {
    guest_boot: String,
    plaintext: Vec<u8>,
    envelope: Vec<u8>,
    snapshot_path: std::path::PathBuf,
}

fn read_evidence(raw: &Path, private_key: &[u8]) -> Result<OemdrvEvidence> {
    let extract = |entry: &str| -> Result<Vec<u8>> {
        let output = ext4_command("debugfs")
            .args(["-R", &format!("cat {entry}")])
            .arg(ext4_path(raw)?)
            .output()?;
        ensure!(
            output.status.success() && !output.stdout.is_empty(),
            "missing OEMDRV witness"
        );
        Ok(output.stdout)
    };
    let guest_boot = String::from_utf8(extract("/public/installer-boot")?)?
        .trim()
        .to_owned();
    let fingerprint = String::from_utf8(extract("/public/recovery-sha256")?)?;
    let envelope = extract("/recovery.oaep")?;
    ensure!(
        envelope.len() == 256,
        "invalid private recovery envelope size"
    );
    let key_path = raw.with_file_name("recovery.pem");
    let envelope_path = raw.with_file_name("recovery.oaep");
    fs::write(&key_path, private_key)?;
    fs::write(&envelope_path, &envelope)?;
    let result = Command::new("openssl.exe")
        .args(["pkeyutl", "-decrypt", "-inkey"])
        .arg(&key_path)
        .arg("-in")
        .arg(&envelope_path)
        .args([
            "-pkeyopt",
            "rsa_padding_mode:oaep",
            "-pkeyopt",
            "rsa_oaep_md:sha256",
        ])
        .output()?;
    ensure!(
        result.status.success()
            && result.stdout.len() == 71
            && digest(&result.stdout)[7..] == *fingerprint.trim(),
        "private OEMDRV envelope verification failed"
    );
    Ok(OemdrvEvidence {
        guest_boot,
        plaintext: result.stdout,
        envelope,
        snapshot_path: raw.to_owned(),
    })
}
