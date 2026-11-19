//! Real native Gen2 qualification against the portable T6 contract.
use anyhow::{Context, Result, ensure};
use ilex_devkit::{Workspace, config::Selection, core::*};
use ilex_tooling_windows::{
    gen2::{self, Provider, Vm},
    transport,
};
use libtest_mimic::Trial;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[path = "security/probes.rs"]
mod probes;
#[path = "security/unsigned.rs"]
mod unsigned;

const RECOVERY: &str = "ilex-gen2-recovery-410625\n";
const READY: Duration = Duration::from_secs(420);
const BOOT: Duration = Duration::from_secs(120);
const CREDENTIAL: &str = "/var/tmp/ilex-gen2.cred";
const SEAL: &str = "set -eu; printf ilex-gen2-tpm-proof | systemd-creds encrypt --name=ilex-gen2 --with-key=tpm2 --tpm2-device=auto --tpm2-pcrs=7 - /var/tmp/ilex-gen2.cred";
const UNSEAL: &str = "systemd-creds decrypt --name=ilex-gen2 /var/tmp/ilex-gen2.cred -";

pub fn trial() -> Trial {
    Trial::test("security::firmware", || {
        run().map_err(|e| format!("{e:#}").into())
    })
}

fn run() -> Result<()> {
    transport::install_cancel_handler()?;
    let ws = Workspace::discover()?;
    let resources: Resources = serde_json::from_str(
        &std::env::var("ILEX_GEN2_RESOURCES").context("use cargo xtask security --self-check")?,
    )?;
    let selection: Selection = serde_json::from_str(&std::env::var("ILEX_GEN2_SELECTION")?)?;
    let provider = Provider::new(ws.identity()?, &ws.ensure_state()?)?;
    let boot = BootSource::BlankDisk {
        bytes: 32 << 30,
        format: DiskFormat::Vhdx,
    };
    let mut spec = gen2::spec(&ws.identity()?, "security-firmware", resources, boot)?;
    spec.security_launch = SecurityLaunch::QualificationFixture;
    let artifacts = ilex_devkit::qemu::artifact_dir(&ws, "gen2-security")?.join(&spec.run);
    fs::create_dir(&artifacts)?;
    eprintln!("Gen2 evidence: {}", artifacts.display());
    let mut record = provider.begin_qualification(&spec, &artifacts)?;
    let start = Instant::now();
    let mut stage = "media";
    let result = qualify(&ws, &provider, spec, &artifacts, &selection, &mut stage);
    record.finish(&result)?;
    write(&artifacts, "qualification.json", record.evidence())?;
    write(
        &artifacts,
        "result.json",
        &json!({
            "stage":stage, "elapsed_seconds":start.elapsed().as_secs_f64(),
            "qualification":if result.is_ok() {"passed"} else {"unqualified"},
            "error":result.as_ref().err().map(|e| format!("{e:#}"))
        }),
    )?;
    result
}

fn qualify(
    ws: &Workspace,
    provider: &Provider,
    mut spec: VmSpec,
    artifacts: &Path,
    selection: &Selection,
    stage: &mut &'static str,
) -> Result<()> {
    let source = PathBuf::from(std::env::var_os("ILEX_GEN2_DISK").context("missing T3 disk")?);
    let collection = source
        .parent()
        .and_then(Path::parent)
        .context("missing Gen2 disk collection")?;
    let disk: DiskArtifact = serde_json::from_slice(
        &fs::read(collection.join("disk.windows.json"))
            .context("reading Gen2 disk delivery receipt")?,
    )?;
    check_version(disk.schema)?;
    ensure!(
        disk.format == DiskFormat::Qcow2
            && disk.location.path == source
            && disk.location.executor_id == format!("windows-owner:{}", spec.owner)
            && disk.provisioning_hash == digest(b"ilex-gen2-v1;seed-host-key;seed-access-key")
            && file_digest(&source)? == disk.location.sha256,
        "invalid installed-disk delivery"
    );
    let digest = disk
        .image
        .registry_digest
        .as_deref()
        .context("missing installed image digest")?;
    write(artifacts, "disk.json", &disk)?;
    let iso = PathBuf::from(std::env::var_os("ILEX_ISO").context("missing exact installer ISO")?);
    let iso_hash = file_digest(&iso)?;
    ensure!(
        iso_hash.eq_ignore_ascii_case(&format!("sha256:{}", std::env::var("ILEX_ISO_SHA256")?)),
        "installer ISO hash mismatch: {iso_hash}"
    );
    write(
        artifacts,
        "installer-media.json",
        &json!({"path":iso,"sha256":iso_hash,"bytes":fs::metadata(&iso)?.len()}),
    )?;

    *stage = "installer";
    let mut installer = gen2::spec(
        &ws.identity()?,
        "security-iso",
        spec.resources.clone(),
        spec.boot.clone(),
    )?;
    installer.iso = Some(iso);
    installer.network = NetworkIntent::User { ssh: false };
    installer.security_launch = SecurityLaunch::QualificationFixture;
    let mut vm = provider.create(installer)?;
    let start = Instant::now();
    provider.start(&mut vm)?;
    write(artifacts, "installer-vm.json", &vm.status()?)?;
    let booted = wait_screen(&vm, artifacts, "installer-chooser", READY, |ocr| {
        ocr.contains("what to install on this computer") && ocr.contains("machine configuration")
    })?;
    let iso_elapsed = start.elapsed().as_secs_f64();
    provider.destroy_owned(&mut vm)?;
    drop(vm);

    *stage = "installed_disk";
    // These files are outside evidence so a large disposable disk cannot be
    // accidentally attached to a PR. Parent deletion follows guest teardown.
    let media = Media::new(&ws.ensure_state()?.join("gen2"), &spec.run)?;
    let parent = provider.installed_parent(&source, &disk.location.sha256)?;
    spec.boot = BootSource::Disk {
        path: parent,
        format: DiskFormat::Vhdx,
        writable: true,
    };
    spec.network = NetworkIntent::User { ssh: true };
    spec.ssh = Some(SshProvisioning {
        user: "root".into(),
        public_key: String::new(),
        recipe_hash: disk.provisioning_hash.clone(),
    });
    let mut vm = provider.create(spec.clone())?;
    let start = Instant::now();
    provider.start(&mut vm)?;
    let positive = provider.inspect(&vm)?;
    let first_config = vm.status()?;
    write(artifacts, "positive-vm.json", &first_config)?;
    *stage = "pre_ssh_keyboard";
    let (first_id, prompt) = recovery(&vm, artifacts, "recovery-prompt")?;
    let first_elapsed = start.elapsed().as_secs_f64();
    *stage = "guest_measurements";
    probes::guest(&vm, artifacts, "first", digest, selection)?;
    probes::amdgpu(&vm, artifacts)?;
    let diagnostics = exec(
        &vm,
        "set -eu; for module in hv_vmbus hv_storvsc hv_netvsc; do test -d /sys/module/$module; done; lsmod; rpm -q bootc shim-x64 grub2-efi-x64 systemd kernel-core; bootctl status --no-pager; lsblk -o NAME,TRAN,FSTYPE; uname -r",
    )?;
    fs::write(artifacts.join("guest.txt"), diagnostics)?;
    let session = gen2::wait(BOOT, || {
        let frame = vm.capture()?;
        ensure!(
            image(&frame).fingerprint() != image(&prompt).fingerprint()
                && !image(&frame).is_blank(),
            "waiting for the Linux console after recovery"
        );
        Ok(frame)
    })?;
    save_frame(artifacts, "linux-session", &session, None)?;
    fs::write(
        artifacts.join("linux-pointer.txt"),
        vm.qualify_pointer(&session, session.width / 3, session.height / 3)?,
    )?;
    vm.capabilities()?.require(Capability::SessionPointer)?;
    write(artifacts, "linux-control.json", &vm.capabilities()?)?;

    *stage = "tpm_warm_and_cold";
    exec(&vm, SEAL)?;
    unseal(&vm)?;
    let ciphertext = media.0.join("sealed.cred");
    vm.download(CREDENTIAL, &ciphertext)?;
    let sealed_hash = file_digest(&ciphertext)?;
    let start = Instant::now();
    vm.reboot()?;
    let warm_id = vm.wait_ssh(Some(&first_id), READY)?;
    let warm_elapsed = start.elapsed().as_secs_f64();
    probes::guest(&vm, artifacts, "warm", digest, selection)?;
    unseal(&vm)?;
    vm.guest_exec(&["systemctl".into(), "--no-block".into(), "poweroff".into()])?;
    vm.wait_off(BOOT)?;
    provider.stop(&mut vm)?;
    let start = Instant::now();
    provider.start(&mut vm)?;
    let cold_id = vm.wait_ssh(Some(&warm_id), READY)?;
    let cold_elapsed = start.elapsed().as_secs_f64();
    probes::guest(&vm, artifacts, "cold", digest, selection)?;
    unseal(&vm)?;
    let cold_config = vm.status()?;
    ensure!(
        provider.inspect(&vm)? == positive
            && first_id != cold_id
            && first_config["protector_sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64)
            && first_config["protector_sha256"] == cold_config["protector_sha256"],
        "cold boot did not preserve VM UUID/key protector with a distinct guest boot"
    );
    write(artifacts, "cold-vm.json", &cold_config)?;
    let positive_measurement: Value =
        serde_json::from_slice(&fs::read(artifacts.join("cold-pcr7.json"))?)?;
    let cold = vm.capture()?;
    save_frame(artifacts, "cold-boot", &cold, None)?;
    provider.destroy_owned(&mut vm)?;
    drop(vm);

    *stage = "fresh_tpm";
    let mut fresh_spec = gen2::spec(
        &ws.identity()?,
        "security-fresh-tpm",
        spec.resources.clone(),
        spec.boot.clone(),
    )?;
    fresh_spec.network = spec.network.clone();
    fresh_spec.ssh = spec.ssh.clone();
    fresh_spec.security_launch = SecurityLaunch::QualificationFixture;
    let mut fresh = provider.create(fresh_spec)?;
    let start = Instant::now();
    provider.start(&mut fresh)?;
    let (_, _) = recovery(&fresh, artifacts, "fresh-recovery-prompt")?;
    let fresh_elapsed = start.elapsed().as_secs_f64();
    probes::guest(&fresh, artifacts, "fresh", digest, selection)?;
    let fresh_measurement: Value =
        serde_json::from_slice(&fs::read(artifacts.join("fresh-pcr7.json"))?)?;
    let fresh_config = fresh.status()?;
    ensure!(
        fresh_config["id"] != cold_config["id"]
            && fresh_config["protector_sha256"] != cold_config["protector_sha256"]
            && fresh_measurement["sha256"] == positive_measurement["sha256"],
        "fresh-vTPM negative must differ in identity while matching PCR7"
    );
    fresh.upload(&ciphertext, CREDENTIAL)?;
    let uploaded = exec(&fresh, "sha256sum /var/tmp/ilex-gen2.cred")?;
    ensure!(
        String::from_utf8(uploaded)?.split_whitespace().next()
            == sealed_hash.strip_prefix("sha256:"),
        "negative used a different sealed credential"
    );
    let failure = exec(
        &fresh,
        &format!(
            "set +e; diagnostic=$({UNSEAL} 2>&1 >/dev/null); code=$?; test \"$code\" -ne 0 && printf '%s\\n' \"$diagnostic\""
        ),
    )?;
    ensure!(
        String::from_utf8_lossy(&failure)
            .to_ascii_lowercase()
            .contains("tpm"),
        "fresh vTPM produced an unrelated failure"
    );
    fs::write(artifacts.join("fresh-unseal.txt"), failure)?;
    write(artifacts, "fresh-vm.json", &fresh_config)?;
    provider.destroy_owned(&mut fresh)?;
    drop(fresh);

    *stage = "unsigned_control";
    let raw = media.0.join("unsigned.raw");
    let parent = media.0.join("unsigned.vhdx");
    let (payload, image) = unsigned::image();
    fs::write(&raw, image)?;
    gen2::vhdx(&raw, &parent, "raw")?;
    let mut controls = vec![];
    for enabled in [false, true] {
        let label = if enabled {
            "unsigned-on"
        } else {
            "unsigned-off"
        };
        let mut control = gen2::spec(
            &ws.identity()?,
            label,
            spec.resources.clone(),
            BootSource::Disk {
                path: parent.clone(),
                format: DiskFormat::Vhdx,
                writable: true,
            },
        )?;
        control.tpm = TpmLifecycle::Off;
        control.security_launch = SecurityLaunch::QualificationFixture;
        control.secure_boot = if enabled {
            Requirement::Required
        } else {
            Requirement::Off
        };
        let mut vm = provider.create(control)?;
        let start = Instant::now();
        provider.start(&mut vm)?;
        let frame = wait_screen(&vm, artifacts, label, BOOT, |ocr| {
            if enabled {
                (ocr.contains("unsigned") && ocr.contains("not allowed"))
                    || ocr.contains("not allowed by the secure boot policy")
            } else {
                ocr.contains("ilex unsigned executed")
            }
        })?;
        controls.push(json!({"secure_boot":enabled,"elapsed_seconds":start.elapsed().as_secs_f64(),"vm":frame.vm}));
        provider.destroy_owned(&mut vm)?;
    }
    write(
        artifacts,
        "unsigned-controls.json",
        &json!({"controls":controls,"payload_sha256":ilex_devkit::core::digest(&payload)}),
    )?;
    media.cleanup()?;
    *stage = "complete";
    write(
        artifacts,
        "milestones.json",
        &json!({
            "resources":spec.resources,"installer_to_chooser_seconds":iso_elapsed,"installer_vm":booted.vm,
            "first_ssh_seconds":first_elapsed,"warm_ssh_seconds":warm_elapsed,"cold_ssh_seconds":cold_elapsed,
            "fresh_ssh_seconds":fresh_elapsed,"first_boot":first_id,"warm_boot":warm_id,"cold_boot":cold_id,
            "positive_vm":positive,"credential_sha256":sealed_hash,"cleanup":"All test VM UUIDs absent; owned VHDX/firmware/vTPM directories deleted"
        }),
    )?;
    Ok(())
}

fn exec(vm: &Vm, script: &str) -> Result<Vec<u8>> {
    vm.guest_exec(&[
        "timeout".into(),
        "65s".into(),
        "sh".into(),
        "-eu".into(),
        "-c".into(),
        script.into(),
    ])
}

fn unseal(vm: &Vm) -> Result<()> {
    ensure!(
        exec(vm, UNSEAL)? == b"ilex-gen2-tpm-proof",
        "unsealed test value differs"
    );
    Ok(())
}

fn recovery(vm: &Vm, artifacts: &Path, label: &str) -> Result<(String, Frame)> {
    let prompt = wait_screen(vm, artifacts, label, READY, |ocr| {
        ocr.starts_with("ilex gen") && ocr.contains("recovery key")
    })?;
    ensure!(
        vm.guest_exec(&["true".into()]).is_err(),
        "SSH accepted before recovery input"
    );
    vm.key(&Input::Text {
        text: RECOVERY.into(),
        layout: "us".into(),
        sensitive: true,
    })?;
    let boot = vm.wait_ssh(None, READY)?;
    let receipt = exec(vm, "cat /var/lib/ilex-gen2/recovery-ok")?;
    ensure!(
        String::from_utf8(receipt)?.trim() == boot,
        "SSH did not follow recovery input on the same guest boot"
    );
    Ok((boot, prompt))
}

fn wait_screen(
    vm: &Vm,
    artifacts: &Path,
    label: &str,
    deadline: Duration,
    matches: impl Fn(&str) -> bool,
) -> Result<Frame> {
    use windows::{Globalization::Language, Media::Ocr::OcrEngine, core::HSTRING};
    let engine =
        OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en-US"))?)
            .context("Windows en-US OCR is required for this firmware self-check")?;
    let mut last = None;
    let result = gen2::wait(deadline, || {
        let frame = vm.capture()?;
        let words = ocr(&engine, &frame)?;
        let normalized = words.to_ascii_lowercase();
        let accepted = matches(&normalized);
        last = Some((frame, words));
        ensure!(accepted, "waiting for {label}; last OCR: {normalized:?}");
        Ok(())
    });
    let (frame, words) = last.context("no WMI screenshot decoded")?;
    save_frame(artifacts, label, &frame, Some(&words))?;
    result?;
    Ok(frame)
}

fn ocr(engine: &windows::Media::Ocr::OcrEngine, frame: &Frame) -> Result<String> {
    use windows::{
        Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
        Storage::Streams::DataWriter,
    };
    let bgra: Vec<_> = frame
        .pixels
        .as_chunks::<3>()
        .0
        .iter()
        .flat_map(|p| [p[2], p[1], p[0], 255])
        .collect();
    let writer = DataWriter::new()?;
    writer.WriteBytes(&bgra)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &writer.DetachBuffer()?,
        BitmapPixelFormat::Bgra8,
        frame.width as i32,
        frame.height as i32,
    )?;
    Ok(engine.RecognizeAsync(&bitmap)?.join()?.Text()?.to_string())
}

fn image(frame: &Frame) -> ilex_vmctl::Frame {
    ilex_vmctl::Frame {
        width: frame.width,
        height: frame.height,
        rgb: frame.pixels.clone(),
    }
}

fn save_frame(root: &Path, label: &str, frame: &Frame, ocr: Option<&str>) -> Result<()> {
    let png = image(frame).encode_png()?;
    fs::write(root.join(format!("{label}.png")), &png)?;
    write(
        root,
        &format!("{label}-frame.json"),
        &json!({
            "source":gen2::SCREENSHOT_SOURCE,"vm":frame.vm,"captured_unix_ms":frame.captured_unix_ms,
            "view_generation":frame.generation,"width":frame.width,"height":frame.height,
            "png_sha256":digest(&png),"ocr":ocr
        }),
    )
}

fn file_digest(path: &Path) -> Result<String> {
    gen2::sha256_file(path)
}

fn write(root: &Path, name: &str, value: &impl serde::Serialize) -> Result<()> {
    fs::write(root.join(name), serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

struct Media(PathBuf);
impl Media {
    fn new(root: &Path, run: &str) -> Result<Self> {
        let dir = root.join(format!("media-{run}"));
        fs::create_dir(&dir)?;
        Ok(Self(dir))
    }
    fn cleanup(&self) -> Result<()> {
        // No recursive delete while a child VHDX may still reference a parent.
        for name in ["unsigned.raw", "unsigned.vhdx", "sealed.cred"] {
            let file = self.0.join(name);
            if file.try_exists()? {
                fs::remove_file(file)?;
            }
        }
        fs::remove_dir(&self.0)?;
        Ok(())
    }
}
