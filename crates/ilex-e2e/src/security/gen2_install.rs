use super::{BOOT, Media, READY, exec, file_digest, save_frame, wait_screen, write};
use anyhow::{Result, ensure};
use ilex_devkit::{Workspace, core::*};
use ilex_tooling_windows::gen2::{self, Provider};
use serde_json::json;
use std::{
    fs,
    path::Path,
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
        let answers = media.0.join("answers.iso");
        provider.write_oemdrv(&files, &answers)?;
        let mut vm = provider.create_installer(spec, &answers)?;
        let start = Instant::now();
        let handle = provider.start(&mut vm)?;
        write(
            artifacts,
            "transition-input.json",
            &json!({"vm":handle, "iso":file_digest(iso)?, "oemdrv":file_digest(&answers)?}),
        )?;
        // Fail closed on attempts to change media while running.
        ensure!(
            provider.boot_installed(&mut vm, false).is_err(),
            "running installer media change was accepted"
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
            provider.boot_installed(&mut vm, false)? == handle,
            "transition changed VM identity"
        );
        // Retry also verifies ejection and the sole boot target.
        ensure!(
            provider.boot_installed(&mut vm, false)? == handle,
            "transition retry changed VM identity"
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
"#,
        )?;
        fs::write(artifacts.join("transition-guest.txt"), &trace)?;
        let installer_boot =
            String::from_utf8(exec(&vm, "cat /var/lib/ilex-gen2/installer-boot")?)?
                .trim()
                .to_owned();
        ensure!(
            !installer_boot.is_empty() && installer_boot != boot,
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
        vm.poweroff(BOOT)?;
        provider.destroy_owned(&mut vm)
    })();
    media.finish(provider, result)
}
