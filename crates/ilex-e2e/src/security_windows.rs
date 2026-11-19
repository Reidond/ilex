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

#[path = "security/gen2_install.rs"]
mod install;
#[path = "security/probes.rs"]
mod probes;
#[path = "security/unsigned.rs"]
mod unsigned;

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

pub fn transition_trial() -> Trial {
    Trial::test("security::iso_transition", || {
        (|| -> Result<()> {
            transport::install_cancel_handler()?;
            let ws = Workspace::discover()?;
            let resources: Resources =
                serde_json::from_str(&std::env::var("ILEX_GEN2_RESOURCES")?)?;
            let provider = Provider::new(ws.identity()?, &ws.ensure_state()?)?;
            let artifacts = ilex_devkit::qemu::artifact_dir(&ws, "gen2-iso-transition")?;
            install::transition(&ws, &provider, resources, &verified_iso()?, &artifacts)
        })()
        .map_err(|e| format!("{e:#}").into())
    })
}

fn verified_iso() -> Result<PathBuf> {
    let iso = PathBuf::from(std::env::var_os("ILEX_ISO").context("missing exact installer ISO")?);
    let hash = file_digest(&iso)?;
    ensure!(
        hash.eq_ignore_ascii_case(&format!("sha256:{}", std::env::var("ILEX_ISO_SHA256")?)),
        "installer ISO hash mismatch"
    );
    Ok(iso)
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
    let resources = spec.resources.clone();
    let mut result = qualify(&ws, &provider, spec, &artifacts, &selection, &mut stage);
    record.finish(&result)?;
    if result.is_ok() {
        stage = "live_runtime";
        result = qualify_live(&ws, &provider, resources, &artifacts);
        if result.is_err() {
            record.finish(&result)?;
        } else {
            stage = "complete";
        }
    }
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
    installer.iso = Some(iso.clone());
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

    let mut media = Media::new(&ws.ensure_state()?.join("gen2"), &spec.run)?;
    let secret = format!("q-{}", gen2::random_id()?);
    let secret_digest = ilex_devkit::core::digest(secret.as_bytes());
    let result = (|| -> Result<()> {
        *stage = "disk_validation";
        validate_external_storage(provider, &disk, &media, artifacts)?;
        *stage = "unsigned_control";
        unsigned_control(ws, provider, artifacts, &mut media, &spec.resources)?;

        *stage = "installed_disk";
        let parent = provider.installed_parent(&disk)?;
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
        media.track(&spec.run);
        let mut vm = provider.create_recovery_fixture(spec.clone(), &secret_digest[7..])?;
        let start = Instant::now();
        provider.start(&mut vm)?;
        let positive = provider.inspect(&vm)?;
        let first_config = vm.status()?;
        write(artifacts, "positive-vm.json", &first_config)?;
        *stage = "pre_ssh_keyboard";
        let (first_id, prompt) = recovery(&vm, artifacts, "recovery-prompt", &secret)?;
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
            vm.qualify_pointer(&session, session.width / 3, session.height * 2 / 3)?,
        )?;
        fs::write(
            artifacts.join("session-keyboard.txt"),
            vm.qualify_session_keyboard()?,
        )?;
        let controls = vm.capabilities()?;
        controls.require(Capability::SessionPointer)?;
        controls.require(Capability::SessionKeyboard)?;
        write(artifacts, "linux-control.json", &vm.capabilities()?)?;

        *stage = "tpm_warm_and_cold";
        exec(&vm, SEAL)?;
        unseal(&vm)?;
        let ciphertext = media.0.join("sealed.cred");
        vm.download(CREDENTIAL, &ciphertext)?;
        let sealed_hash = file_digest(&ciphertext)?;
        let start = Instant::now();
        let warm_id = vm.reboot(&first_id, READY)?;
        let warm_elapsed = start.elapsed().as_secs_f64();
        probes::guest(&vm, artifacts, "warm", digest, selection)?;
        unseal(&vm)?;
        vm.poweroff(BOOT)?;
        provider.stop(&mut vm)?;
        let start = Instant::now();
        provider.start(&mut vm)?;
        let cold_id = vm.wait_ssh(Some(&warm_id), READY)?;
        let cold_elapsed = start.elapsed().as_secs_f64();
        probes::guest(&vm, artifacts, "cold", digest, selection)?;
        unseal(&vm)?;
        let cold_config = vm.status()?;
        write(artifacts, "cold-vm.json", &cold_config)?;
        // The local protector is a host wrapping blob. Successful unseal with
        // the original credential proves that the TPM seed survived power-off.
        ensure!(
            provider.inspect(&vm)? == positive && first_id != cold_id,
            "cold boot did not preserve the VM UUID with a distinct guest boot"
        );
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
        media.track(&fresh_spec.run);
        let mut fresh = provider.create_recovery_fixture(fresh_spec, &secret_digest[7..])?;
        let start = Instant::now();
        provider.start(&mut fresh)?;
        let (_, _) = recovery(&fresh, artifacts, "fresh-recovery-prompt", &secret)?;
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
        let diagnostic = String::from_utf8_lossy(&failure).to_ascii_lowercase();
        ensure!(
            diagnostic.contains("0x0000018b")
                && diagnostic.contains("failed to unseal secret using tpm2"),
            "fresh vTPM did not reject the copied credential with TPM_RC_HANDLE"
        );
        exec(&fresh, "rm /var/tmp/ilex-gen2.cred")?;
        exec(&fresh, SEAL)?;
        unseal(&fresh)?;
        fs::write(artifacts.join("fresh-unseal.txt"), failure)?;
        write(artifacts, "fresh-vm.json", &fresh_config)?;
        provider.destroy_owned(&mut fresh)?;
        drop(fresh);

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
    })();
    media.finish(provider, result)?;
    *stage = "iso_to_disk";
    install::transition(ws, provider, spec.resources, &iso, artifacts)?;
    let files = grep_secret(artifacts, &secret)?;
    write(
        artifacts,
        "input-privacy.json",
        &json!({"files":files,"plaintext_matches":0,"png_ocr_matches":0,"capture_blocked_after_send":true}),
    )?;
    *stage = "complete";
    Ok(())
}

fn unsigned_control(
    ws: &Workspace,
    provider: &Provider,
    artifacts: &Path,
    media: &mut Media,
    resources: &Resources,
) -> Result<()> {
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
            resources.clone(),
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
        media.track(&control.run);
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
        if !enabled {
            vm.key(&Input::Chord { codes: vec![0x25] })?;
            wait_screen(&vm, artifacts, "uefi-keyboard", BOOT, |ocr| {
                // Windows OCR reads the firmware font's V as U here.
                ocr.contains("ilex uefi key received") || ocr.contains("ilex uefi key receiued")
            })?;
        }
        controls.push(json!({"secure_boot":enabled,"elapsed_seconds":start.elapsed().as_secs_f64(),"vm":frame.vm}));
        provider.destroy_owned(&mut vm)?;
    }
    write(
        artifacts,
        "unsigned-controls.json",
        &json!({"controls":controls,"payload_sha256":ilex_devkit::core::digest(&payload)}),
    )
}

fn validate_external_storage(
    provider: &Provider,
    original: &DiskArtifact,
    media: &Media,
    artifacts: &Path,
) -> Result<()> {
    use ilex_tooling_windows::runtime;
    let runtime_dir = runtime::fetch()?;
    for args in [
        vec!["create", "-f", "raw", "backing.raw", "1M"],
        vec![
            "create",
            "-f",
            "qcow2",
            "-F",
            "raw",
            "-b",
            "backing.raw",
            "backed.qcow2",
            "1M",
        ],
        vec![
            "create",
            "-f",
            "qcow2",
            "-o",
            "data_file=external.raw",
            "external.qcow2",
            "1M",
        ],
    ] {
        ilex_devkit::cmd::run(
            runtime::command(&runtime_dir, "qemu-img.exe")
                .current_dir(&media.0)
                .args(args),
        )?;
    }
    for (name, reason) in [
        ("backed.qcow2", "qcow2 has backing storage"),
        ("external.qcow2", "qcow2 has an external data file"),
    ] {
        let mut artifact = original.clone();
        artifact.location.path = media.0.join(name);
        artifact.location.sha256 = file_digest(&artifact.location.path)?;
        artifact.virtual_bytes = 1 << 20;
        ensure!(
            provider
                .installed_parent(&artifact)
                .is_err_and(|e| e.to_string() == reason),
            "delivered external-storage fixture was not rejected before conversion"
        );
    }
    write(
        artifacts,
        "qcow2-validation.json",
        &json!({"backing_file_rejected":true,"external_data_rejected":true}),
    )
}

fn qualify_live(
    ws: &Workspace,
    provider: &Provider,
    resources: Resources,
    artifacts: &Path,
) -> Result<()> {
    let spec = gen2::spec(
        &ws.identity()?,
        "security-live-status",
        resources,
        BootSource::BlankDisk {
            bytes: 1 << 30,
            format: DiskFormat::Vhdx,
        },
    )?;
    provider
        .probe(&spec)?
        .require(Capability::FirmwareEnforcement)?;
    let mut vm = provider.create(spec)?;
    provider.start(&mut vm)?;
    let running = vm.capabilities()?;
    running.require(Capability::FirmwareEnforcement)?;
    running.require(Capability::PersistentTpm)?;
    provider.stop(&mut vm)?;
    let stopped = vm.capabilities()?;
    ensure!(
        stopped.runtime == VmRuntimeStatus::Stopped
            && [Capability::FirmwareEnforcement, Capability::PersistentTpm]
                .iter()
                .all(|cap| stopped.require(*cap).is_err()
                    && stopped.features.get(cap) != Some(&CapabilityStatus::Available)),
        "live stopped VM advertised historical capabilities"
    );
    write(
        artifacts,
        "live-runtime.json",
        &json!({"running":running,"stopped":stopped}),
    )?;
    // Windows sharing without DELETE holds the sidecar just long enough
    // to verify that a failed release retains its allocation journal.
    use std::os::windows::fs::OpenOptionsExt;
    provider.start(&mut vm)?;
    let handle = vm.handle();
    let admission =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("missing LOCALAPPDATA")?)
            .join("ilex/admission");
    let mut receipt = None;
    for entry in fs::read_dir(admission)? {
        let path = entry?.path();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with(".gen2.json"))
            && let Ok(bytes) = fs::read(&path)
            && let Ok(resource) = serde_json::from_slice::<OwnedResource>(&bytes)
            && resource.owner == handle.owner
            && resource.opaque_id == handle.opaque_id
        {
            ensure!(
                receipt.replace(path).is_none(),
                "ambiguous owned slot receipt"
            );
        }
    }
    let receipt = receipt.context("no slot receipt for live fixture")?;
    let sharing = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&receipt)?;
    ensure!(
        provider.destroy_owned(&mut vm).is_err(),
        "sharing fault did not block receipt removal"
    );
    drop(vm);
    ensure!(
        receipt.try_exists()?,
        "slot receipt disappeared during injected fault"
    );
    drop(sharing);
    provider.cleanup_run(&handle.run)?;
    ensure!(
        !receipt.try_exists()?,
        "reconciliation left an owned slot receipt"
    );
    write(
        artifacts,
        "slot-recovery.json",
        &json!({"vm":handle,"deleted_vm":true,"receipt_reconciled":true}),
    )
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

fn recovery(vm: &Vm, artifacts: &Path, label: &str, secret: &str) -> Result<(String, Frame)> {
    let prompt = wait_screen(vm, artifacts, label, READY, |ocr| {
        ocr.starts_with("ilex gen") && ocr.contains("recovery key")
    })?;
    ensure!(
        vm.guest_exec(&["true".into()]).is_err(),
        "SSH accepted before recovery input"
    );
    vm.key(&Input::Text {
        text: format!("{secret}\n"),
        layout: "us".into(),
        sensitive: true,
    })?;
    ensure!(
        vm.capture()
            .is_err_and(|error| error.to_string().contains("capture withheld")),
        "sensitive input did not withhold captures"
    );
    vm.audit_console_state(secret.as_bytes())?;
    let boot = vm.wait_ssh(None, READY)?;
    vm.acknowledge_sensitive_input(
        &["cat".into(), "/var/lib/ilex-gen2/recovery-ok".into()],
        format!("{boot}\n").as_bytes(),
    )?;
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
        ensure!(accepted, "waiting for expected screen {label}");
        Ok(())
    });
    let (frame, words) = last.context("no WMI screenshot decoded")?;
    save_frame(artifacts, label, &frame, Some(&words))?;
    result?;
    Ok(frame)
}

fn ocr(engine: &windows::Media::Ocr::OcrEngine, frame: &Frame) -> Result<String> {
    ocr_pixels(engine, &frame.pixels, frame.width, frame.height)
}

fn ocr_pixels(
    engine: &windows::Media::Ocr::OcrEngine,
    pixels: &[u8],
    width: u32,
    height: u32,
) -> Result<String> {
    use windows::{
        Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap},
        Storage::Streams::DataWriter,
    };
    let bgra: Vec<_> = pixels
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
        width as i32,
        height as i32,
    )?;
    Ok(engine.RecognizeAsync(&bitmap)?.join()?.Text()?.to_string())
}

fn grep_secret(root: &Path, secret: &str) -> Result<usize> {
    use windows::{Globalization::Language, Media::Ocr::OcrEngine, core::HSTRING};
    let engine =
        OcrEngine::TryCreateFromLanguage(&Language::CreateLanguage(&HSTRING::from("en-US"))?)?;
    let utf16: Vec<_> = secret.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut pending = vec![root.to_owned()];
    let mut scanned = 0;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                pending.push(entry.path());
                continue;
            }
            let bytes = fs::read(entry.path())?;
            ensure!(
                [secret.as_bytes(), &utf16]
                    .iter()
                    .all(|needle| !bytes.windows(needle.len()).any(|window| window == *needle)),
                "sensitive canary found in qualification artifacts"
            );
            if entry.path().extension().is_some_and(|ext| ext == "png") {
                let frame = ilex_vmctl::Frame::decode(&bytes)?;
                let text = ocr_pixels(&engine, &frame.rgb, frame.width, frame.height)?;
                ensure!(
                    !text.to_ascii_lowercase().contains(secret),
                    "sensitive canary found in screenshot pixels"
                );
            }
            scanned += 1;
        }
    }
    Ok(scanned)
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

struct Media(PathBuf, Vec<String>);
impl Media {
    fn new(root: &Path, run: &str) -> Result<Self> {
        let dir = root.join(format!("media-{run}"));
        fs::create_dir(&dir)?;
        Ok(Self(dir, Vec::new()))
    }
    fn track(&mut self, run: &str) {
        self.1.push(run.to_owned());
    }
    fn finish(&self, provider: &Provider, result: Result<()>) -> Result<()> {
        let cleanup = (|| -> Result<()> {
            for run in &self.1 {
                provider.reconcile_if_present(run)?;
            }
            for name in [
                "unsigned.raw",
                "unsigned.vhdx",
                "sealed.cred",
                "answers.iso",
                "oemdrv/ks.cfg",
                "backing.raw",
                "backed.qcow2",
                "external.qcow2",
                "external.raw",
            ] {
                let file = self.0.join(name);
                if file.try_exists()? {
                    fs::remove_file(file)?;
                }
            }
            if self.0.join("oemdrv").try_exists()? {
                fs::remove_dir(self.0.join("oemdrv"))?;
            }
            fs::remove_dir(&self.0)?;
            Ok(())
        })();
        match (result, cleanup) {
            (Err(work), Err(teardown)) => {
                Err(work.context(format!("retained media {}: {teardown:#}", self.0.display())))
            }
            (work, cleanup) => work.and(cleanup),
        }
    }
}
