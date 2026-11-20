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
use ilex_enroll::Secret;
use ilex_vmctl::Vmctl;
use libtest_mimic::Trial;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const GUEST: &str = include_str!("encrypted/guest.py");
const PRIVACY: &str = include_str!("encrypted/privacy.py");
const DIAGNOSTICS: &str = include_str!("encrypted/diagnostics.py");
const LOG_STREAM: &str = include_str!("encrypted/log_stream.py");

pub fn trials() -> Vec<Trial> {
    let mut trials = vec![
        common::trial("security::encrypted_install", || run(false, false, false)),
        common::trial("security::encrypted_existing_space", || {
            run(true, false, false)
        }),
        common::trial("security::encrypted_interrupted", || {
            run(false, true, false)
        }),
    ];
    if std::env::var_os("ILEX_ENROLLMENT_LOCAL_DIAGNOSTICS").is_some() {
        trials.push(common::trial(
            "security::enrollment_diagnostic_install",
            || run(false, false, true),
        ));
        trials.push(common::trial(
            "security::enrollment_diagnostic_existing_space",
            || run(true, false, true),
        ));
        trials.push(common::trial(
            "security::enrollment_diagnostic_interrupted",
            || run(false, true, true),
        ));
        trials.push(common::trial(
            "security::enrollment_diagnostic_layout_refusal",
            || run_layout(false, false, true, true),
        ));
    }
    if std::env::var_os("ILEX_ENROLLMENT_CHECKPOINT").is_some() {
        // Explicit local diagnosis, outside CI's security::encrypted_ filter.
        // Even successful boot proofs cannot promote an incomplete ISO gate.
        trials.push(common::trial("security::enrollment_checkpoint", checkpoint));
    }
    trials
}

fn restore_files(source: &Path, target: &Path) -> Result<()> {
    for file in evidence_files(source)? {
        let destination = target.join(file.strip_prefix(source)?);
        fs::create_dir_all(
            destination
                .parent()
                .context("checkpoint file has no parent")?,
        )?;
        fs::copy(file, destination)?;
    }
    Ok(())
}

fn checkpoint() -> Result<()> {
    ilex_enroll::harden()?;
    let ws = common::workspace()?;
    let state_dir = ws.ensure_state()?.canonicalize()?;
    let source = PathBuf::from(
        std::env::var_os("ILEX_ENROLLMENT_CHECKPOINT").context("checkpoint required")?,
    )
    .canonicalize()?;
    ensure!(
        source.starts_with(&state_dir),
        "checkpoint must belong to this checkout's state"
    );
    let receipt: Value = serde_json::from_slice(&fs::read(source.join("checkpoint.json"))?)?;
    let run = receipt["run"].as_str().context("checkpoint run missing")?;
    let (pid, nonce) = run.split_once('-').context("invalid checkpoint run")?;
    ensure!(
        run.len() < 80
            && !pid.is_empty()
            && !nonce.is_empty()
            && pid.bytes().all(|b| b.is_ascii_digit())
            && nonce.bytes().all(|b| b.is_ascii_digit())
            && source.file_name().and_then(|p| p.to_str()) == Some(run),
        "invalid checkpoint run"
    );
    ensure!(
        receipt["owner"] == ws.identity()?.owner
            && receipt["stage"] == "checkpoint-complete"
            && receipt["qemu_exit_status"] == 0,
        "owned cleanly stopped installer checkpoint required"
    );
    ensure!(
        !Path::new("/proc").join(pid).exists(),
        "original test process must finish before checkpoint diagnosis"
    );
    let work = state_dir.join("encrypted-fixtures").join(run);
    let bundle = state_dir.join("security").join(run);
    ensure!(
        !work.exists() && !bundle.exists(),
        "checkpoint diagnosis must not overwrite active fixture state"
    );
    let original_manifest: Value =
        serde_json::from_slice(&fs::read(source.join("bundle/bundle.json"))?)?;
    ensure!(
        original_manifest["owner"] == receipt["owner"]
            && original_manifest["run"] == run
            && original_manifest["disk"].as_str() == work.join("root.qcow2").to_str(),
        "checkpoint bundle identity differs"
    );
    let purpose = original_manifest["purpose"]
        .as_str()
        .context("checkpoint purpose missing")?;
    ensure!(
        matches!(
            purpose,
            "encrypted-install" | "encrypted-space" | "encrypted-interrupted"
        ),
        "unsupported checkpoint purpose"
    );
    let interrupted = purpose == "encrypted-interrupted";
    fs::create_dir(&work)?;
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
    let mut bundle_created = false;
    let mut restored = false;
    let mut cleanup_spec = None;
    let mut core_proven = false;
    let mut interruption_proven = false;
    let mut stage = "restore";
    let mut credentials = Value::Null;
    let artifacts = work.join("evidence");
    let published = qemu::artifact_dir(&ws, &format!("enrollment-checkpoint-{purpose}"))?;
    clear_evidence(&published)?;
    let mut result = (|| -> Result<()> {
        restore_files(&source.join("work"), &work)?;
        fs::create_dir(&bundle)?;
        bundle_created = true;
        fs::set_permissions(&bundle, fs::Permissions::from_mode(0o700))?;
        restore_files(&source.join("bundle"), &bundle)?;
        restored = true;
        let expected: Value = serde_json::from_slice(&fs::read(artifacts.join("candidate.json"))?)?;
        ensure!(
            expected["interrupt_completion"] == interrupted,
            "checkpoint interruption mode differs"
        );
        let seed = work.join("oemdrv.img");
        let passphrase = Secret::read(File::open(work.join("seed/passphrase"))?.into())?;
        let recovery = recovery_secret(&seed, &work)?;
        credentials = fingerprints(&[&passphrase, &recovery]);
        let recorded: Value =
            serde_json::from_slice(&extract(&seed, &artifacts, "credentials.json")?)?;
        ensure!(
            recorded == credentials,
            "checkpoint credential fingerprints differ"
        );
        let installer_probe: Value =
            serde_json::from_slice(&extract(&seed, &artifacts, "probe.json")?)?;
        ensure!(
            installer_probe["verdict"] == "eligible",
            "checkpoint installer was not eligible"
        );
        let replay = security::replay_pcr7(&extract(&seed, &artifacts, "eventlog.bin")?)?;
        ensure!(
            installer_probe["pcr7"] == replay.sha256,
            "checkpoint installer replay differs"
        );
        let tokens: Value = serde_json::from_slice(&extract(&seed, &artifacts, "tokens.json")?)?;
        let disk = work.join("root.qcow2");
        let mut config = VmConfig::new(
            original_manifest["purpose"]
                .as_str()
                .context("checkpoint purpose missing")?,
            &disk,
        );
        config.snapshot = false;
        config.memory = "6G".into();
        let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
        spec.run = run.to_string();
        spec.accelerator = match std::env::var("ILEX_SECURITY_ACCELERATOR").as_deref() {
            Ok("tcg") => Accelerator::Tcg,
            Ok("kvm") => Accelerator::Native,
            _ => bail!("explicit checkpoint accelerator required"),
        };
        spec.secure_boot = Requirement::Required;
        spec.tpm = TpmLifecycle::Persistent {
            bundle_id: run.to_string(),
        };
        cleanup_spec = Some(spec.clone());
        let provider = LinuxProvider::new(ws.clone())?;
        let caps = provider.probe(&spec)?;
        caps.require(Capability::FirmwareEnforcement)?;
        caps.require(Capability::PersistentTpm)?;
        fs::write(
            artifacts.join("checkpoint-backend.json"),
            serde_json::to_vec_pretty(&caps)?,
        )?;
        let vm = provider.create(spec.clone())?;
        let mut fixture = Fixture {
            provider,
            vm,
            spec,
            console: None,
        };
        if !interrupted {
            stage = "first_installed_boot";
            fixture.disk_boot()?;
            let booted = assertions(&mut fixture, &artifacts, &expected, &installer_probe);
            fixture.logs(&artifacts, "checkpoint-installed")?;
            booted?;
            core_proven = true;
            guest_privacy(
                fixture.vm.running_mut()?,
                &artifacts,
                "checkpoint-after-finalisation",
                &credentials,
            )?;
            stage = "cold_restart";
            poweroff(&mut fixture, &artifacts, "checkpoint-installed")?;
            fixture.disk_boot()?;
            let cold = artifacts.join("cold");
            fs::create_dir_all(&cold)?;
            assertions(&mut fixture, &cold, &expected, &installer_probe)?;
            guest_privacy(
                fixture.vm.running_mut()?,
                &cold,
                "checkpoint-cold",
                &credentials,
            )?;
            poweroff(&mut fixture, &artifacts, "checkpoint-cold")?;
            stage = "same_tpm_policy_mismatch";
            same_tpm_mismatch(
                &ws,
                &mut fixture,
                &artifacts,
                &passphrase,
                &credentials,
                &installer_probe,
                &tokens,
            )?;
        }
        // The frozen original disk has never had an installed boot. It is
        // backing-only for these separate first-boot mismatch/recovery guests.
        let frozen = source.join("work/root.qcow2");
        stage = "pcr_mismatch_first_boot";
        fallback(
            &ws,
            &frozen,
            &fixture.spec,
            &work,
            &artifacts,
            &passphrase,
            &credentials,
            &installer_probe,
            &tokens,
            &expected,
            "pcr-mismatch",
        )?;
        stage = "recovery_key_boot";
        fallback(
            &ws,
            &frozen,
            &fixture.spec,
            &work,
            &artifacts,
            &recovery,
            &credentials,
            &installer_probe,
            &tokens,
            &expected,
            "recovery",
        )?;
        if interrupted {
            stage = "interrupted_completion_repair";
            let (reissued, repaired_tokens) = interrupted_repair(
                &mut fixture,
                &artifacts,
                &passphrase,
                &mut credentials,
                &tokens,
                &expected,
            )?;
            stage = "reissued_recovery_key_boot";
            fallback(
                &ws,
                &disk,
                &fixture.spec,
                &work,
                &artifacts,
                &reissued,
                &credentials,
                &installer_probe,
                &repaired_tokens,
                &expected,
                "reissued-recovery",
            )?;
            interruption_proven = true;
        } else {
            stage = "no_tpm_fallback";
            fallback(
                &ws,
                &disk,
                &fixture.spec,
                &work,
                &artifacts,
                &passphrase,
                &credentials,
                &installer_probe,
                &tokens,
                &expected,
                "no-tpm",
            )?;
        }
        fixture.provider.destroy_owned(&mut fixture.vm)?;
        stage = "post_install_gates_complete";
        Ok(())
    })();
    // Preserve only privacy-checked metadata on a partial diagnostic. Raw
    // failure logs/screens remain private, as for the full installer gate.
    if credentials.is_array() {
        let export = (|| -> Result<()> {
            artifact_privacy(&[&artifacts, &raw_logs(&artifacts)?], &credentials)?;
            for file in evidence_files(&artifacts)? {
                if !matches!(
                    file.extension().and_then(|s| s.to_str()),
                    Some("json" | "bin")
                ) {
                    continue;
                }
                let target = published.join(file.strip_prefix(&artifacts)?);
                fs::create_dir_all(target.parent().context("checkpoint export has no parent")?)?;
                fs::copy(file, target)?;
            }
            let privacy = artifact_privacy(&[&published], &credentials)?;
            fs::write(
                published.join("final-artifact-privacy.json"),
                serde_json::to_vec_pretty(&privacy)?,
            )?;
            Ok(())
        })();
        if export.is_err() {
            clear_evidence(&published)?;
            result = Err(anyhow::anyhow!("checkpoint privacy/export failed"));
            stage = "privacy_export";
            core_proven = false;
        }
    }
    if bundle_created && bundle.exists() {
        if let Some(spec) = cleanup_spec {
            security::destroy_bundle(&ws, &spec).context("owned checkpoint cleanup failed")?;
        } else {
            // No VM was created: this is only our incomplete restore.
            fs::remove_dir_all(&bundle)?;
        }
    }
    fs::remove_dir_all(&work)?;
    fs::write(
        published.join("result.json"),
        serde_json::to_vec_pretty(&json!({
        "qualification":"unqualified", "scope":"owned completed-installer checkpoint; this diagnosis cannot qualify the original live finalisation privacy gate",
            "core_boot_proof":if core_proven {"passed"} else if interrupted {"not-applicable-interrupted"} else {"unproven"}, "interruption_boot_proof":if interruption_proven {"passed"} else {"unproven"}, "post_install_gates":if result.is_ok() {"passed"} else {"unqualified"},
            "stage":stage, "source_run":run, "source_harness":receipt["source_head"], "restored_original_tpm_and_vars":restored,
            "diagnostic":result.as_ref().err().map(failure_code), "error":result.as_ref().err().map(|_|"private diagnostic details suppressed")
        }))?,
    )?;
    bail!(
        "checkpoint diagnostic completed at {stage}; full encrypted installer qualification remains unqualified"
    )
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
    let run = work
        .file_name()
        .and_then(|s| s.to_str())
        .context("fixture run missing")?;
    let name = ws.vm_name(&format!("enroll-registry-{run}"));
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

struct PrivateConsole {
    input: UnixStream,
    output: Option<thread::JoinHandle<std::io::Result<()>>>,
    path: PathBuf,
}
impl PrivateConsole {
    fn attach(info: &qemu::VmInfo) -> Result<Self> {
        let path = info.dir.join("input.sock");
        ensure!(!path.exists(), "private console endpoint already exists");
        let mut qmp = ilex_devkit::qmp::Qmp::connect(&info.qmp_sock)?;
        let devices = qmp.execute("query-chardev", json!({}))?;
        ensure!(
            devices
                .as_array()
                .context("invalid console inventory")?
                .iter()
                .any(|d| d["label"] == "serial0"
                    && d["filename"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("file"))),
            "expected the fixture's file-backed serial0"
        );
        // Keep the guest UART unchanged. This test-only backend has no QEMU
        // logfile: a separate reader records outgoing guest bytes exclusively.
        // Credential input never enters QMP, argv or that output writer.
        qmp.execute("chardev-change", json!({"id":"serial0", "backend":{
            "type":"socket", "data":{"addr":{"type":"unix","data":{"path":path}},"server":true,"wait":false}
        }}))?;
        let input = UnixStream::connect(&path)?;
        input.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = input.try_clone()?;
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(info.dir.join("serial.log"))?;
        let output = thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let result = (|| {
                loop {
                    let length = match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(length) => length,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    log.write_all(&buffer[..length])?;
                }
                log.sync_all()
            })();
            buffer.fill(0);
            result
        });
        Ok(Self {
            input,
            output: Some(output),
            path,
        })
    }
    fn credential(&mut self, secret: &Secret) -> Result<()> {
        self.input.write_all(secret.bytes())?;
        self.input.write_all(b"\n")?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        // Called after QEMU exits: drain through EOF before copying logs.
        if let Some(output) = self.output.take() {
            output
                .join()
                .map_err(|_| anyhow::anyhow!("private console reader failed"))??;
        }
        let _ = fs::remove_file(&self.path);
        Ok(())
    }
}
impl Drop for PrivateConsole {
    fn drop(&mut self) {
        // Cancellation must also wake a reader when a failed VM is still up.
        let _ = self.input.shutdown(Shutdown::Both);
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

struct Fixture {
    provider: LinuxProvider,
    vm: LinuxVm,
    spec: VmSpec,
    console: Option<PrivateConsole>,
}
impl Fixture {
    fn private_console(&mut self) -> Result<()> {
        self.console = Some(PrivateConsole::attach(&self.vm.running_mut()?.info)?);
        Ok(())
    }
    fn logs(&mut self, artifacts: &Path, label: &str) -> Result<()> {
        if self.vm.running_mut()?.exited()?.is_some()
            && let Some(mut console) = self.console.take()
        {
            console.finish()?;
        }
        let info = &self.vm.running_mut()?.info;
        let private = raw_logs(artifacts)?;
        fs::create_dir_all(&private)?;
        for file in ["serial.log", "qemu.log", "swtpm.log"] {
            if info.dir.join(file).is_file() {
                fs::copy(info.dir.join(file), private.join(format!("{label}-{file}")))?;
            }
        }
        Ok(())
    }
    fn disk_boot(&mut self) -> Result<()> {
        self.provider.stop(&mut self.vm)?;
        self.console.take();
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

fn seed(work: &Path, config: &Value) -> Result<(PathBuf, Secret)> {
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
    fs::write(dir.join("privacy.py"), PRIVACY)?;
    fs::write(dir.join("diagnostics.py"), DIAGNOSTICS)?;
    fs::write(dir.join("log_stream.py"), LOG_STREAM)?;
    // Test-only transport: no recovery plaintext is written to host media.
    let private_key = work.join("recovery.pem");
    ensure!(
        Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-out"
            ])
            .arg(&private_key)
            .output()?
            .status
            .success(),
        "private fixture key generation failed"
    );
    ensure!(
        Command::new("openssl")
            .args(["pkey", "-in"])
            .arg(&private_key)
            .args(["-pubout", "-out"])
            .arg(dir.join("recovery.pub"))
            .output()?
            .status
            .success(),
        "fixture public key generation failed"
    );
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
    let ks = if config["preserve_data"] == true {
        ks.replace(
            "clearpart --all --initlabel --drives=vda",
            "clearpart --none",
        )
    } else {
        ks.to_owned()
    };
    fs::write(dir.join("ks.cfg"), ks)?;
    let seed = work.join("oemdrv.img");
    File::create(&seed)?.set_len(32 << 20)?;
    cmd::run(
        Command::new("mkfs.ext4")
            .args(["-q", "-F", "-L", "OEMDRV", "-d"])
            .arg(&dir)
            .arg(&seed),
    )?;
    let secret = Secret::read(ilex_enroll::secret_fd(&passphrase)?)?;
    passphrase.fill(0);
    Ok((seed, secret))
}

fn recovery_secret(seed: &Path, work: &Path) -> Result<Secret> {
    let output = Command::new("debugfs")
        .args(["-R", "cat /recovery.oaep"])
        .arg(seed)
        .output()?;
    ensure!(
        output.status.success() && output.stdout.len() == 256,
        "private recovery envelope missing"
    );
    let envelope = work.join("recovery.oaep");
    fs::write(&envelope, output.stdout)?;
    let mut child = Command::new("openssl")
        .args(["pkeyutl", "-decrypt", "-inkey"])
        .arg(work.join("recovery.pem"))
        .args(["-in"])
        .arg(envelope)
        .args([
            "-pkeyopt",
            "rsa_padding_mode:oaep",
            "-pkeyopt",
            "rsa_oaep_md:sha256",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let secret = Secret::read(
        child
            .stdout
            .take()
            .context("private recovery pipe missing")?
            .into(),
    )?;
    ensure!(
        child.wait()?.success() && secret.bytes().len() == 71,
        "private recovery decryption failed"
    );
    ensure!(
        secret
            .bytes()
            .iter()
            .enumerate()
            .all(|(i, b)| if i % 9 == 8 {
                *b == b'-'
            } else {
                b"cbdefghijklnrtuv".contains(b)
            }),
        "invalid recovery credential; details suppressed"
    );
    Ok(secret)
}

fn fingerprints(secrets: &[&Secret]) -> Value {
    json!(
        secrets
            .iter()
            .map(|s| json!({"length":s.bytes().len(), "sha256":security::hash(s.bytes())}))
            .collect::<Vec<_>>()
    )
}

fn failure_code(error: &anyhow::Error) -> Value {
    // Error chains can contain guest output. Export only fixed classifications
    // and an OS errno; never the original text, matching path or line.
    let mut detail = format!("{error:#}").into_bytes();
    let patterns: &[(&[u8], &str)] = &[
        (b"QMP chardev-change failed", "console-backend-change"),
        (b"QMP query-chardev failed", "console-inventory"),
        (
            b"expected the fixture's file-backed serial0",
            "console-backend-type",
        ),
        (
            b"private console endpoint already exists",
            "console-endpoint-exists",
        ),
        (
            b"did not request fallback authentication",
            "fallback-prompt",
        ),
        (
            b"encrypted ISO installation did not finish",
            "installer-exit",
        ),
        (
            b"private encrypted fixture failed",
            "installer-guest-failure",
        ),
        (
            b"encrypted installer deadline exceeded",
            "installer-timeout",
        ),
        (b"read installer recovery envelope", "recovery-envelope"),
        (b"finaliser-leak-scan.json", "installer-finaliser-evidence"),
        (
            b"private guest secret scan failed",
            "installer-privacy-scan",
        ),
        (b"no-TPM mode was not preserved", "no-tpm-probe"),
        (b"start no-TPM completion", "no-tpm-unit-start"),
        (
            b"wait for no-TPM passphrase prompt",
            "no-tpm-completion-prompt",
        ),
        (
            b"wait for no-TPM completion confirmation",
            "no-tpm-completion-confirmation",
        ),
        (b"verify no-TPM completion exit", "no-tpm-completion-exit"),
        (
            b"no-TPM completion did not preserve",
            "no-tpm-completion-state",
        ),
        (
            b"no-TPM completion changed LUKS",
            "no-tpm-completion-tokens",
        ),
        (b"did not accept SSH", "ssh-deadline"),
        (b"qemu exited", "guest-exited"),
        (b"QMP command", "qmp-deadline"),
        (b"QMP response ID", "qmp-response-id"),
        (b"installed system health", "installed-system-health"),
        (b"installed enrollment self-check", "enrollment-self-check"),
        (b"PCR7 lacks meaningful", "positive-authority-log"),
        (
            b"present TPM / changed Secure Boot PCR",
            "mismatch-measurement",
        ),
        (
            b"fallback boot changed slots/tokens",
            "fallback-metadata-changed",
        ),
        (b"post-finalisation private scan", "target-privacy-scan"),
        (b"credential found or unreadable", "artifact-privacy-scan"),
    ];
    let codes: Vec<&str> = patterns
        .iter()
        .filter_map(|(pattern, code)| {
            detail
                .windows(pattern.len())
                .any(|w| w == *pattern)
                .then_some(*code)
        })
        .collect();
    detail.fill(0);
    let errno = error
        .chain()
        .find_map(|e| e.downcast_ref::<std::io::Error>()?.raw_os_error());
    json!({"codes":codes, "errno":errno})
}

fn private_scan(script: &str) -> Result<Value> {
    let script = format!("__name__ = 'ilex_private_matcher'\n{script}");
    let output = Command::new("python3")
        .args(["-I", "-S", "-c", &script])
        .output()?;
    ensure!(
        output.status.success(),
        "private artifact scan failed; diagnostics suppressed"
    );
    let result: Value =
        serde_json::from_slice(&output.stdout).context("invalid private scan result")?;
    ensure!(
        result["secret_found"] == false && result["read_errors"] == 0,
        "credential found or unreadable evidence; details suppressed"
    );
    Ok(result)
}

fn artifact_privacy(roots: &[&Path], credentials: &Value) -> Result<Value> {
    private_scan(&format!(
        "{LOG_STREAM}\n{PRIVACY}\nprint(json.dumps(scan(json.loads({:?}), json.loads({:?}))))",
        serde_json::to_string(roots)?,
        credentials.to_string()
    ))
}

fn raw_logs(artifacts: &Path) -> Result<PathBuf> {
    Ok(artifacts
        .parent()
        .context("evidence work directory missing")?
        .join("raw-logs"))
}

pub(crate) fn format_log(source: &Path, destination: &Path) -> Result<()> {
    let script = format!(
        "__name__ = 'ilex_fixture_diagnostics'\n{LOG_STREAM}\n{DIAGNOSTICS}\npathlib.Path(sys.argv[2]).write_bytes(tail(pathlib.Path(sys.argv[1])))"
    );
    ensure!(
        Command::new("python3")
            .args(["-I", "-S", "-c", &script])
            .arg(source)
            .arg(destination)
            .output()?
            .status
            .success(),
        "could not format private log"
    );
    Ok(())
}

fn diagnostics(artifacts: &Path, output: &Path, credentials: &Value) -> Result<()> {
    let mut command = Command::new("python3");
    let script = format!("{LOG_STREAM}\n{DIAGNOSTICS}");
    command.args(["-I", "-S", "-c", &script]).arg(output);
    let private = raw_logs(artifacts)?;
    if private.try_exists()? {
        command.args(evidence_files(&private)?);
    }
    for file in [
        "no-tpm-unit-journal.txt",
        "no-tpm-luksDump.txt",
        "no-tpm-failed-units.txt",
        "host-fixture-error.txt",
        "installer-leak-scan.json",
        "installer-finaliser-leak-scan.json",
        "installer-probe.json",
    ] {
        command.arg(artifacts.join(file));
    }
    ensure!(
        command.output()?.status.success(),
        "could not format private diagnostics"
    );
    artifact_privacy(&[output], credentials)?;
    Ok(())
}

fn guest_privacy(vm: &qemu::Vm, artifacts: &Path, label: &str, credentials: &Value) -> Result<()> {
    let script = format!(
        "__name__ = 'ilex_private_matcher'\n{LOG_STREAM}\n{PRIVACY}\nprint(json.dumps(scan(['/etc', '/tmp', '/var/tmp', '/var/log', '/var/lib/anaconda', '/var/lib/ilex', '/var/roothome', '/root', '/var/lib/systemd/coredump', '/var/spool/abrt', '/run/anaconda', '/run/ilex', '/run/ilex-enroll'], json.loads({:?}), journal=True)))",
        credentials.to_string()
    );
    let command = format!("python3 -I -S -c '{}'", script.replace('\'', "'\\''"));
    let output = vm.ssh(&command)?;
    ensure!(
        output.status.success(),
        "post-finalisation private scan failed; diagnostics suppressed"
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        result["secret_found"] == false
            && result["read_errors"] == 0
            && result["journal_scanned"] == true,
        "post-finalisation privacy failed; details suppressed"
    );
    fs::write(
        artifacts.join(format!("{label}-privacy.json")),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(())
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
    ensure!(["prepare", "drive", "finish", "finalise"].contains(&snapshot.phase.as_str()));
    ensure!(
        [
            "not-started",
            "wait-enrollment-start",
            "uinput-load",
            "uinput-open",
            "uinput-create",
            "input-observe",
            "wait-passphrase",
            "capture-before-input",
            "enter-passphrase",
            "wait-recovery",
            "export-recovery",
            "interrupt-completion",
            "check-interrupted-slots",
            "bind-interrupted-target",
            "verify-interrupted-target-binds",
            "order-interrupted-target-binds",
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
    ensure!(snapshot.helper_errors.len() <= 13);
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
            "btrfs-devices",
            "luks-backing",
            "btrfs-inspect",
            "mount-selection",
            "mount-id",
        ]
        .contains(&code.as_str())
    }));
    fs::write(
        published.join("failure.json"),
        serde_json::to_vec_pretty(&snapshot)?,
    )?;
    Ok(())
}

fn install(
    fixture: &mut Fixture,
    artifacts: &Path,
    deadline: Duration,
    interrupted: bool,
) -> Result<()> {
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
                    && screenshots == [true, !interrupted],
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

fn type_at_vt(vm: &qemu::Vm, console: &mut PrivateConsole, credential: &Secret) -> Result<()> {
    // Only the helper and the expected byte count cross SSH. The credential
    // travels on the output-only-logged UART, then through the guest keyboard.
    let script = format!(
        "__name__ = 'ilex_fixture_vt_input'\n{GUEST}\nharden()\nvt_input({})",
        credential.bytes().len()
    );
    let command = format!(
        "set -eu; systemctl stop serial-getty@ttyS0.service; \
        install -d -m 0700 /run/ilex-encrypted-test; \
        rm -f /run/ilex-encrypted-test/vt-input-ready; \
        systemd-run --quiet --remain-after-exit --unit=ilex-encrypted-vt-input \
        /usr/bin/python3 -I -S -c '{}'",
        script.replace('\'', "'\\''")
    );
    ensure!(vm.ssh(&command)?.status.success(), "start private VT input");
    vm.ssh_ok("timeout 30 sh -c 'until test -f /run/ilex-encrypted-test/vt-input-ready; do sleep 0.2; done'")?;
    console.credential(credential)?;
    vm.ssh_ok("set -eu; timeout 45 sh -c 'until test \"$(systemctl show -p SubState --value ilex-encrypted-vt-input)\" = exited; do test \"$(systemctl show -p ActiveState --value ilex-encrypted-vt-input)\" != failed || exit 1; sleep 0.2; done'; test \"$(systemctl show -p Result --value ilex-encrypted-vt-input)\" = success; systemctl stop ilex-encrypted-vt-input; rm -f /run/ilex-encrypted-test/vt-input-ready")?;
    Ok(())
}

fn poweroff(fixture: &mut Fixture, artifacts: &Path, label: &str) -> Result<()> {
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
    fixture.logs(artifacts, label)?;
    fixture.provider.stop(&mut fixture.vm)
}

#[allow(clippy::too_many_arguments)] // One explicit disposable negative boot profile.
fn fallback(
    ws: &Workspace,
    source: &Path,
    template: &VmSpec,
    work: &Path,
    artifacts: &Path,
    credential: &Secret,
    credentials: &Value,
    installer_probe: &Value,
    original_tokens: &Value,
    expected: &Value,
    mode: &str,
) -> Result<()> {
    let disk = work.join(format!("{mode}.qcow2"));
    cmd::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(source)
            .arg(&disk),
    )?;
    let mut spec = template.clone();
    spec.run.push_str(&format!("-{mode}"));
    // Runtime paths include the owner-qualified checkout slug. Keep QMP and
    // swtpm Unix sockets within sockaddr_un even for the interrupted case.
    let family = match template.purpose.as_str() {
        "ed-l" => "l",
        "encrypted-space" | "ed-s" => "s",
        "encrypted-interrupted" | "ed-i" => "i",
        _ => "a",
    };
    let suffix = match mode {
        "pcr-mismatch" => "pcr",
        "recovery" => "recovery",
        "reissued-recovery" => "reissue",
        "no-tpm" => "no-tpm",
        "layout-recovery" => "layout",
        _ => bail!("unknown fallback case"),
    };
    let local_diagnostic = template.secure_boot == Requirement::Off;
    let prefix = if local_diagnostic { "ed" } else { "enroll" };
    spec.purpose = format!("{prefix}-{family}-{suffix}");
    spec.iso = None;
    spec.additional_disks.clear();
    spec.secure_boot = if mode == "pcr-mismatch" || local_diagnostic {
        Requirement::Off
    } else {
        Requirement::Required
    };
    spec.tpm = if mode == "no-tpm" || local_diagnostic {
        TpmLifecycle::Off
    } else {
        TpmLifecycle::Persistent {
            bundle_id: spec.run.clone(),
        }
    };
    // This negative has a separate identity. It measures fallback behavior and
    // cannot mint a passing T6 firmware/TPM qualification for a different profile.
    if !local_diagnostic {
        spec.security_launch = ilex_devkit::core::SecurityLaunch::QualificationFixture;
    }
    spec.boot = ilex_devkit::core::BootSource::Disk {
        path: disk,
        format: ilex_devkit::core::DiskFormat::Qcow2,
        writable: true,
    };
    let provider = LinuxProvider::new(ws.clone())?;
    let vm = provider.create(spec.clone())?;
    let mut fixture = Fixture {
        provider,
        vm,
        spec,
        console: None,
    };
    fixture.provider.start(&mut fixture.vm)?;
    let result = check_fallback(
        &mut fixture,
        artifacts,
        credential,
        credentials,
        installer_probe,
        original_tokens,
        expected,
        mode,
        family == "s",
        local_diagnostic,
    );
    if mode == "no-tpm"
        && let Ok(vm) = fixture.vm.running_mut()
    {
        save_fallback_output(
            vm,
            artifacts,
            "no-tpm-unit-journal.txt",
            "journalctl -b -u ilex-enroll-no-tpm-completion.service --no-pager -n 200 -o short-monotonic",
        );
        save_fallback_output(
            vm,
            artifacts,
            "no-tpm-failed-units.txt",
            "systemctl --failed --no-legend --plain",
        );
    }
    if result.is_err() {
        let _ = fixture.logs(artifacts, mode);
    }
    result?;
    fixture.logs(artifacts, mode)?;
    poweroff(&mut fixture, artifacts, mode)?;
    fixture.provider.destroy_owned(&mut fixture.vm)
}

#[allow(clippy::too_many_arguments)] // Assertions for a single disposable fallback VM.
fn check_fallback(
    fixture: &mut Fixture,
    artifacts: &Path,
    credential: &Secret,
    credentials: &Value,
    installer_probe: &Value,
    original_tokens: &Value,
    expected: &Value,
    mode: &str,
    preserve_data: bool,
    local_diagnostic: bool,
) -> Result<()> {
    fixture.private_console()?;
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
    ctl.save_screenshot(&artifacts.join(format!("{mode}-passphrase-prompt.png")))?;
    fixture
        .console
        .as_mut()
        .context("private console unavailable")?
        .credential(credential)?;
    vm.wait_ssh(Duration::from_secs(900))?;
    let probe = guest_json(
        vm,
        artifacts,
        &format!("{mode}-probe.json"),
        "ilex-enroll probe --json",
    )?;
    let deferred_reason = if local_diagnostic {
        "secure-boot-off"
    } else {
        "no-tpm"
    };
    if mode == "no-tpm" || local_diagnostic {
        let firmware_matches = if local_diagnostic {
            probe["secure_boot"] != true
        } else {
            probe["secure_boot"] == true
        };
        ensure!(
            firmware_matches && probe["tpm2"] == false && probe["reason"] == deferred_reason,
            "no-TPM mode was not preserved"
        );
    } else if mode == "pcr-mismatch" {
        let pcr = vm.ssh_ok("cat /sys/class/tpm/tpm0/pcr-sha256/7")?;
        let eventlog = vm.ssh("cat /sys/kernel/security/tpm0/binary_bios_measurements")?;
        ensure!(eventlog.status.success(), "mismatch event log missing");
        ensure!(
            probe["secure_boot"] == false
                && probe["tpm2"] == true
                && pcr.len() == 64
                && pcr.bytes().all(|b| b.is_ascii_hexdigit())
                && pcr.bytes().any(|b| b != b'0')
                && installer_probe["pcr7"] != pcr.trim(),
            "present TPM / changed Secure Boot PCR was not measured"
        );
        // The positive replay deliberately requires Secure Boot authority
        // events. Disabled Secure Boot omits those events; it is not an
        // eligible profile. Preserve its raw log and measured PCR instead of
        // applying (or weakening) the positive qualification contract.
        fs::write(artifacts.join("mismatch-eventlog.bin"), eventlog.stdout)?;
        fs::write(
            artifacts.join("mismatch-pcr.json"),
            serde_json::to_vec_pretty(&json!({"pcr7":pcr,
            "installer_pcr7":installer_probe["pcr7"],
            "secure_boot":false,"tpm2":true,"qualification":"unqualified"}))?,
        )?;
    } else {
        ensure!(
            probe["secure_boot"] == true && probe["tpm2"] == true,
            "recovery boot must retain Secure Boot and a present, different TPM"
        );
    }
    if mode == "layout-recovery" {
        vm.ssh_ok("set -eu; test ! -e /run/ilex/root-unlock.json; journalctl -b -u ilex-enrollment-report.service --no-pager | grep -F 'TPM enrollment was skipped for this storage layout'; if layout=$(ilex-enroll layout --root / 2>&1); then exit 1; fi; printf '%s\n' \"$layout\" | grep -F 'unsupported device-mapper layout'")?;
    } else {
        let evidence = guest_json(
            vm,
            artifacts,
            &format!("{mode}-unlock.json"),
            "cat /run/ilex/root-unlock.json",
        )?;
        ensure!(
            evidence["method"] == "unknown"
                && evidence["uuid"] == original_tokens["uuid"]
                && evidence["boot_id"] == vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?.trim(),
            "fallback was incorrectly labeled as TPM unlock"
        );
        let layout = guest_json(
            vm,
            artifacts,
            &format!("{mode}-layout.json"),
            "ilex-enroll layout --root /",
        )?;
        ensure!(
            layout["uuid"] == evidence["uuid"],
            "fallback evidence does not describe encrypted root"
        );
    }
    let bootc = guest_json(
        vm,
        artifacts,
        &format!("{mode}-bootc.json"),
        "bootc status --json",
    )?;
    ensure!(
        bootc["status"]["booted"]["image"]["imageDigest"] == expected["digest"]
            && bootc["spec"]["image"]["signature"] == "containerPolicy",
        "fallback boot did not use the exact enforcing candidate"
    );
    // Reuse acknowledged recovery during a local completion retry with no
    // TPM. This must defer to passphrase-only mode without generating a key
    // or changing any slot/token. No credential enters SSH, argv or a unit.
    let before = guest_json(
        vm,
        artifacts,
        &format!("{mode}-enrollment-before.json"),
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        mode != "no-tpm" || before["recovery"]["acknowledged"] == true,
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
    let tokens_before = guest_json(
        vm,
        artifacts,
        &format!("{mode}-tokens-before.json"),
        &token_command,
    )?;
    if mode == "no-tpm" {
        save_fallback_output(
            vm,
            artifacts,
            "no-tpm-luksDump.txt",
            &format!("cryptsetup luksDump /dev/disk/by-uuid/{uuid}"),
        );
    }
    ensure!(
        tokens_before == *original_tokens,
        "fallback boot changed slots/tokens or resealed automatically"
    );
    if mode == "no-tpm" {
        vm.ssh_ok("chvt 8; systemd-run --quiet --remain-after-exit --unit=ilex-enroll-no-tpm-completion /usr/bin/ilex-enroll complete --root / --tty /dev/tty8").context("start no-TPM completion")?;
        vm.ssh_ok("timeout 60 sh -c 'until grep -aFq \"Re-enter the disk passphrase\" /dev/vcs8; do sleep 1; done'").context("wait for no-TPM passphrase prompt")?;
        type_at_vt(
            vm,
            fixture
                .console
                .as_mut()
                .context("private console unavailable")?,
            credential,
        )?;
        vm.ssh_ok("timeout 120 sh -c 'until grep -aFq \"Installation completion is ready.\" /dev/vcs8; do sleep 1; done'").context("wait for no-TPM completion confirmation")?;
        ctl.save_screenshot(&artifacts.join(format!("{mode}-passphrase-only.png")))?;
        ctl.key("ret")?;
        vm.ssh_ok("set -eu; timeout 30 sh -c 'until test \"$(systemctl show -p SubState --value ilex-enroll-no-tpm-completion)\" = exited; do sleep 1; done'; test \"$(systemctl show -p Result --value ilex-enroll-no-tpm-completion)\" = success; systemctl stop ilex-enroll-no-tpm-completion").context("verify no-TPM completion exit")?;
        let after = guest_json(
            vm,
            artifacts,
            &format!("{mode}-enrollment.json"),
            "cat /var/lib/ilex/security/enrollment.json",
        )?;
        let setup = guest_json(
            vm,
            artifacts,
            &format!("{mode}-setup.json"),
            "cat /etc/ilex/setup.json",
        )?;
        ensure!(
            after["state"] == "unavailable"
                && after["reason"] == deferred_reason
                && after["recovery"] == before["recovery"]
                && setup["security"]["tpm"]["status"] == "passphrase-only",
            "no-TPM completion did not preserve passphrase-only mode"
        );
        let tokens_after = guest_json(
            vm,
            artifacts,
            &format!("{mode}-tokens.json"),
            &token_command,
        )?;
        ensure!(
            tokens_after == tokens_before,
            "no-TPM completion changed LUKS slots/tokens"
        );
    }
    if mode == "layout-recovery" {
        ensure!(
            before["state"] == "unavailable"
                && before["reason"] == "unsupported-storage-layout"
                && before["recovery"]["acknowledged"] == true
                && before["tpm"].is_null()
                && tokens_before["slots"]
                    .as_array()
                    .is_some_and(|slots| slots.len() == 2)
                && tokens_before["tokens"].as_array().is_some_and(
                    |tokens| tokens.len() == 1 && tokens[0]["type"] == "systemd-recovery"
                ),
            "unsupported installation did not defer TPM enrollment and retain recovery"
        );
        vm.ssh_ok("set -eu; for attempt in 1 2; do report=$(timeout 30 ilex-enroll complete --root / --tty /dev/tty8 2>&1); printf '%s\n' \"$report\" | grep -F 'Installation can finish'; done; ! grep -Fq tpm2-device= /etc/crypttab")?;
        let after = guest_json(
            vm,
            artifacts,
            "layout-recovery-enrollment.json",
            "cat /var/lib/ilex/security/enrollment.json",
        )?;
        let setup = guest_json(
            vm,
            artifacts,
            "layout-recovery-setup.json",
            "cat /etc/ilex/setup.json",
        )?;
        let tokens_after =
            guest_json(vm, artifacts, "layout-recovery-tokens.json", &token_command)?;
        ensure!(
            before == after
                && tokens_before == tokens_after
                && setup["security"]["tpm"]["status"] == "passphrase-only"
                && setup["security"]["tpm"]["reason"] == "unsupported-storage-layout",
            "unsupported-layout retry changed recovery or enrollment state"
        );
    } else {
        vm.ssh_ok("ilex-enroll layout --root /")?;
    }
    vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; test \"$(stat -c %a /etc/ilex/setup.json)\" = 644; test -z \"$(systemctl --failed --no-legend --plain)\"")?;
    if preserve_data {
        preserved_data(vm, artifacts)?;
    }
    guest_privacy(vm, artifacts, mode, credentials)?;
    Ok(())
}

fn save_fallback_output(vm: &qemu::Vm, artifacts: &Path, file: &str, command: &str) {
    if let Ok(output) = vm.ssh(command) {
        let mut text = output.stdout;
        text.extend(output.stderr);
        let _ = fs::write(artifacts.join(file), text);
    }
}

fn interrupted_repair(
    fixture: &mut Fixture,
    artifacts: &Path,
    passphrase: &Secret,
    credentials: &mut Value,
    original_tokens: &Value,
    expected: &Value,
) -> Result<(Secret, Value)> {
    fixture.disk_boot()?;
    fixture.private_console()?;
    let vm = fixture.vm.running_mut()?;
    let ctl = Vmctl::from_info(vm.info.clone());
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
            vm.exited()?.is_none() && start.elapsed() < Duration::from_secs(1800),
            "interrupted install did not retain fallback prompt"
        );
        thread::sleep(Duration::from_secs(2));
    }
    ctl.save_screenshot(&artifacts.join("interrupted-first-boot-prompt.png"))?;
    fixture
        .console
        .as_mut()
        .context("private console unavailable")?
        .credential(passphrase)?;
    vm.wait_ssh(Duration::from_secs(1800))?;
    let bootc = guest_json(
        vm,
        artifacts,
        "interrupted-bootc.json",
        "bootc status --json",
    )?;
    ensure!(
        bootc["status"]["booted"]["image"]["imageDigest"] == expected["digest"]
            && bootc["spec"]["image"]["signature"] == "containerPolicy",
        "interrupted boot did not use the exact enforcing candidate"
    );
    vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; ilex-enroll layout --root /")?;
    let state = guest_json(
        vm,
        artifacts,
        "interrupted-first-boot-state.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        state["state"] == "recovery-incomplete"
            && state["recovery"]["acknowledged"] == false
            && state["tpm"].is_null(),
        "interrupted install claimed completed recovery/TPM enrollment"
    );
    let unlock = guest_json(
        vm,
        artifacts,
        "interrupted-first-boot-unlock.json",
        "cat /run/ilex/root-unlock.json",
    )?;
    ensure!(
        unlock["method"] == "unknown",
        "interrupted boot falsely claimed TPM unlock"
    );
    let uuid = state["uuid"].as_str().context("interrupted UUID missing")?;
    ensure!(uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'));
    let token_command = format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}");
    let before = guest_json(
        vm,
        artifacts,
        "interrupted-first-boot-tokens.json",
        &token_command,
    )?;
    ensure!(
        before == *original_tokens,
        "interrupted boot left half-enrolled slots or tokens"
    );
    vm.ssh_ok("python3 -I -S -c 'import pathlib; assert sorted(p.name for p in pathlib.Path(\"/var/lib/ilex/security\").iterdir()) == [\"enrollment.json\"]'")?;
    guest_privacy(vm, artifacts, "interrupted-after-finalisation", credentials)?;
    // Ordinary retry must refuse the unacknowledged slot, preserve it and
    // request explicit authenticated reissue; it must never show it again.
    vm.ssh_ok("chvt 8; systemd-run --quiet --unit=ilex-interrupted-retry /usr/bin/ilex-enroll complete --root / --tty /dev/tty8")?;
    vm.ssh_ok("timeout 120 sh -c 'until grep -aFq \"Re-enter the disk passphrase\" /dev/vcs8; do sleep 1; done'")?;
    type_at_vt(
        vm,
        fixture
            .console
            .as_mut()
            .context("private console unavailable")?,
        passphrase,
    )?;
    vm.ssh_ok("timeout 180 sh -c 'until test \"$(systemctl show -p Result --value ilex-interrupted-retry)\" = exit-code; do sleep 1; done'; systemctl reset-failed ilex-interrupted-retry")?;
    let unchanged = guest_json(
        vm,
        artifacts,
        "interrupted-retry-tokens.json",
        &token_command,
    )?;
    ensure!(unchanged == before, "ordinary retry changed fallback slots");
    vm.ssh_ok("systemd-run --quiet --remain-after-exit --unit=ilex-interrupted-reissue /usr/bin/ilex-enroll complete --root / --tty /dev/tty8 --reissue-recovery")?;
    vm.ssh_ok("timeout 120 sh -c 'until grep -aFq \"Re-enter the disk passphrase\" /dev/vcs8; do sleep 1; done'")?;
    type_at_vt(
        vm,
        fixture
            .console
            .as_mut()
            .context("private console unavailable")?,
        passphrase,
    )?;
    vm.ssh_ok("timeout 1800 sh -c 'until grep -aFq \"Press Enter when it is saved offline.\" /dev/vcs8; do sleep 1; done'")?;
    // Capture cells privately, move the key into locked host memory and erase
    // the transient buffers. Neither cells nor the key become an artifact or
    // screenshot; matched bytes/subprocess output are never formatted.
    let mut cells = vm.ssh("cat /dev/vcs8")?;
    ensure!(cells.status.success(), "private local recovery read failed");
    let key = cells
        .stdout
        .windows(71)
        .find(|w| {
            w.iter().enumerate().all(|(i, b)| {
                if i % 9 == 8 {
                    *b == b'-'
                } else {
                    b"cbdefghijklnrtuv".contains(b)
                }
            })
        })
        .context("reissued recovery key not shown locally")?;
    let recovery = Secret::read(ilex_enroll::secret_fd(key)?)?;
    cells.stdout.fill(0);
    cells.stderr.fill(0);
    credentials
        .as_array_mut()
        .context("missing credentials")?
        .push(json!({"length":71,"sha256":security::hash(recovery.bytes())}));
    fs::write(
        artifacts.join("installer-credentials.json"),
        serde_json::to_vec_pretty(credentials)?,
    )?;
    ctl.key("ret")?;
    vm.ssh_ok("timeout 120 sh -c 'until grep -aFq \"Enter the saved recovery key to confirm\" /dev/vcs8; do sleep 1; done'")?;
    type_at_vt(
        vm,
        fixture
            .console
            .as_mut()
            .context("private console unavailable")?,
        &recovery,
    )?;
    vm.ssh_ok("timeout 1800 sh -c 'until grep -aFq \"Installation completion is ready.\" /dev/vcs8; do sleep 1; done'")?;
    let mut cleared = vm.ssh("cat /dev/vcs8")?;
    let key_visible = cleared
        .stdout
        .windows(recovery.bytes().len())
        .any(|w| w == recovery.bytes());
    let readable = cleared.status.success();
    cleared.stdout.fill(0);
    cleared.stderr.fill(0);
    ensure!(
        readable && !key_visible,
        "reissued recovery screen was not cleared; screenshot suppressed"
    );
    ctl.save_screenshot(&artifacts.join("interrupted-reissue-confirmed.png"))?;
    ctl.key("ret")?;
    vm.ssh_ok("timeout 120 sh -c 'until test \"$(systemctl show -p SubState --value ilex-interrupted-reissue)\" = exited; do sleep 1; done'; test \"$(systemctl show -p Result --value ilex-interrupted-reissue)\" = success; systemctl stop ilex-interrupted-reissue")?;
    let after = guest_json(
        vm,
        artifacts,
        "interrupted-reissue-tokens.json",
        &token_command,
    )?;
    for slot in before["slots"]
        .as_array()
        .context("missing original slots")?
    {
        ensure!(
            after["slots"]
                .as_array()
                .context("missing repaired slots")?
                .contains(slot),
            "reissue removed an original fallback slot"
        );
    }
    for token in before["tokens"]
        .as_array()
        .context("missing original tokens")?
    {
        ensure!(
            after["tokens"]
                .as_array()
                .context("missing repaired tokens")?
                .contains(token),
            "reissue replaced an original recovery token"
        );
    }
    let repaired = guest_json(
        vm,
        artifacts,
        "interrupted-repaired-state.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        repaired["recovery"]["acknowledged"] == true
            && repaired["recovery"]["slot"] != state["recovery"]["slot"],
        "explicit reissue did not preserve the original recovery story"
    );
    guest_privacy(vm, artifacts, "interrupted-repaired", credentials)?;
    fixture.logs(artifacts, "interrupted-repaired")?;
    poweroff(fixture, artifacts, "interrupted-repaired")?;
    Ok((recovery, after))
}

fn same_tpm_mismatch(
    ws: &Workspace,
    fixture: &mut Fixture,
    artifacts: &Path,
    passphrase: &Secret,
    credentials: &Value,
    installer_probe: &Value,
    tokens: &Value,
) -> Result<()> {
    let mut negative = fixture.spec.clone();
    negative.security_launch = ilex_devkit::core::SecurityLaunch::QualificationFixture;
    let vars = security::bundle_path(ws, &negative)?.join("VARS.fd");
    let original_vars = fs::read(&vars)?;
    security::disable_fixture_secure_boot(ws, &negative)?;
    fixture.disk_boot()?;
    fixture.private_console()?;
    let vm = fixture.vm.running_mut()?;
    let ctl = Vmctl::from_info(vm.info.clone());
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
            vm.exited()?.is_none() && start.elapsed() < Duration::from_secs(1800),
            "same-TPM PCR mismatch did not retain a prompt"
        );
        thread::sleep(Duration::from_secs(2));
    }
    ctl.save_screenshot(&artifacts.join("same-tpm-pcr-mismatch-prompt.png"))?;
    fixture
        .console
        .as_mut()
        .context("private console unavailable")?
        .credential(passphrase)?;
    vm.wait_ssh(Duration::from_secs(1800))?;
    let probe = guest_json(
        vm,
        artifacts,
        "same-tpm-mismatch-probe.json",
        "ilex-enroll probe --json",
    )?;
    let pcr = vm.ssh_ok("cat /sys/class/tpm/tpm0/pcr-sha256/7")?;
    ensure!(
        probe["secure_boot"] == false
            && probe["tpm2"] == true
            && pcr.len() == 64
            && installer_probe["pcr7"] != pcr.trim(),
        "same-TPM Secure Boot/PCR policy was not changed"
    );
    fs::write(artifacts.join("same-tpm-mismatch-pcr.txt"), pcr)?;
    let unlock = guest_json(
        vm,
        artifacts,
        "same-tpm-mismatch-unlock.json",
        "cat /run/ilex/root-unlock.json",
    )?;
    ensure!(
        unlock["method"] == "unknown"
            && unlock["uuid"] == tokens["uuid"]
            && unlock["boot_id"] == vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?.trim(),
        "policy rejection lacks matching root fallback evidence"
    );
    let uuid = tokens["uuid"].as_str().context("root UUID missing")?;
    let after = guest_json(
        vm,
        artifacts,
        "same-tpm-mismatch-tokens.json",
        &format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}"),
    )?;
    ensure!(
        after == *tokens,
        "policy mismatch automatically resealed or changed fallback slots"
    );
    guest_privacy(vm, artifacts, "same-tpm-mismatch", credentials)?;
    fixture.logs(artifacts, "same-tpm-mismatch")?;
    poweroff(fixture, artifacts, "same-tpm-mismatch")?;
    // Restore only this stopped disposable firmware store; its TPM, LUKS
    // header and every enrolled slot remain intact throughout the negative.
    fs::write(vars, original_vars)?;
    Ok(())
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
    vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; test \"$(stat -c %a /etc/ilex/setup.json)\" = 644; grep -qw 'rd.luks.options=tpm2-device=auto' /proc/cmdline; ! grep -q headless /etc/crypttab")?;
    // Preserve structured health before asserting it; a partial diagnostic
    // can identify a failed unit without exposing raw command error chains.
    let failed = guest_json(
        vm,
        artifacts,
        "failed-units.json",
        "systemctl list-units --all --state=failed --output=json --no-pager",
    )?;
    ensure!(
        failed.as_array().is_some_and(Vec::is_empty),
        "installed system health: failed units"
    );
    let health = vm.ssh("systemctl is-system-running --wait")?;
    let state = String::from_utf8_lossy(&health.stdout).trim().to_string();
    fs::write(
        artifacts.join("system-health.json"),
        serde_json::to_vec_pretty(&json!({"state":state,"success":health.status.success()}))?,
    )?;
    ensure!(
        health.status.success() && state == "running",
        "installed system health"
    );
    vm.ssh_ok("ilex-enroll --self-check")
        .context("installed enrollment self-check")?;
    if expected["preserve_data"] == true {
        preserved_data(vm, artifacts)?;
    }
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

fn copy_evidence(source: &Path, destination: &Path) -> Result<()> {
    let files = evidence_files(source)?;
    ensure!(
        files.iter().all(|path| {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_ascii_lowercase();
            !name.contains("serial") || name.ends_with("-serial.redacted.log")
        }),
        "raw serial log refused at evidence export"
    );
    for path in files {
        let target = destination.join(path.strip_prefix(source)?);
        fs::create_dir_all(target.parent().context("evidence has no parent")?)?;
        fs::copy(path, target)?;
    }
    Ok(())
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

fn preserved_data(vm: &qemu::Vm, artifacts: &Path) -> Result<()> {
    vm.ssh_ok(
        r#"set -eu
device=/dev/disk/by-partlabel/ilex-e2e-keep
test -b "$device"
mkdir -p /run/ilex-preserved-check
mount -o ro "$device" /run/ilex-preserved-check
trap 'umount /run/ilex-preserved-check' EXIT
test "$(cat /run/ilex-preserved-check/sentinel)" = ilex-public-sentinel-v1
"#,
    )?;
    fs::write(
        artifacts.join("preserved-data.json"),
        b"{\"sentinel_preserved\":true,\"partition\":\"ilex-e2e-keep\"}\n",
    )?;
    Ok(())
}

fn run(preserve_data: bool, interrupted: bool, local_diagnostic: bool) -> Result<()> {
    run_layout(preserve_data, interrupted, local_diagnostic, false)
}

fn run_layout(
    preserve_data: bool,
    interrupted: bool,
    local_diagnostic: bool,
    layout_refusal: bool,
) -> Result<()> {
    ilex_enroll::harden()?;
    ensure!(
        !local_diagnostic || std::env::var_os("CI").is_none(),
        "local diagnostic is outside the hosted qualification"
    );
    let ws = common::workspace()?;
    let tier = if layout_refusal {
        "enrollment-diagnostic-layout-refusal"
    } else if local_diagnostic && interrupted {
        "enrollment-diagnostic-interrupted"
    } else if local_diagnostic && preserve_data {
        "enrollment-diagnostic-existing-space"
    } else if local_diagnostic {
        "enrollment-diagnostic-install"
    } else if interrupted {
        "encrypted-interrupted"
    } else if preserve_data {
        "encrypted-existing-space"
    } else {
        "encrypted-install"
    };
    let published = qemu::artifact_dir(&ws, tier)?;
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
    let mut credentials = Value::Null;
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
        let mut config = VmConfig::new(
            if layout_refusal {
                "ed-l"
            } else {
                match (local_diagnostic, interrupted, preserve_data) {
                    (true, true, _) => "ed-i",
                    (true, false, true) => "ed-s",
                    (true, false, false) => "ed-a",
                    (false, true, _) => "encrypted-interrupted",
                    (false, false, true) => "encrypted-space",
                    (false, false, false) => "encrypted-install",
                }
            },
            &disk,
        );
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
        if !local_diagnostic {
            spec.secure_boot = Requirement::Required;
            spec.tpm = TpmLifecycle::Persistent {
                bundle_id: run.clone(),
            };
        }
        let provider = LinuxProvider::new(ws.clone())?;
        let caps = provider.probe(&spec)?;
        fs::write(
            artifacts.join("backend.json"),
            serde_json::to_vec_pretty(&caps)?,
        )?;
        if !local_diagnostic {
            caps.require(Capability::FirmwareEnforcement)?;
            caps.require(Capability::PersistentTpm)?; // Exact 6 GiB / 2 vCPU profile.
        }
        stage = "fixture";
        let image = common::image()?;
        let (_registry, mut expected) = registry(&ws, &image, &work)?;
        expected["preserve_data"] = json!(preserve_data);
        expected["interrupt_completion"] = json!(interrupted);
        expected["local_diagnostic"] = json!(local_diagnostic);
        expected["layout_refusal"] = json!(layout_refusal);
        fs::write(
            artifacts.join("candidate.json"),
            serde_json::to_vec_pretty(&expected)?,
        )?;
        let (seed_path, passphrase) = seed(&work, &expected)?;
        credentials = fingerprints(&[&passphrase]);
        spec.additional_disks.push(ilex_devkit::core::AttachedDisk {
            path: seed_path.clone(),
            format: ilex_devkit::core::DiskFormat::Raw,
            blank_bytes: None,
            read_only: false,
        });
        let vm = provider.create(spec.clone())?;
        let mut fixture = Fixture {
            provider,
            vm,
            spec,
            console: None,
        };
        stage = "installation";
        let deadline = Duration::from_secs(
            std::env::var("ILEX_ENCRYPTED_INSTALL_TIMEOUT")
                .unwrap_or_else(|_| "10800".into())
                .parse()?,
        );
        let installed = install(&mut fixture, &artifacts, deadline, interrupted);
        let installer_logs = fixture.logs(&artifacts, "installer");
        if installed.is_err() {
            fixture.provider.stop(&mut fixture.vm)?;
            // A missing/invalid snapshot adds no public data and never masks
            // the original failed qualification.
            let _ = failure_snapshot(&seed_path, &published);
            for file in ["leak-scan.json", "finaliser-leak-scan.json"] {
                let _ = extract(&seed_path, &artifacts, file);
            }
        }
        installed?;
        installer_logs?;
        fixture.provider.stop(&mut fixture.vm)?;
        stage = "installer_evidence";
        let recovery =
            recovery_secret(&seed_path, &work).context("read installer recovery envelope")?;
        credentials = fingerprints(&[&passphrase, &recovery]);
        let recorded: Value =
            serde_json::from_slice(&extract(&seed_path, &artifacts, "credentials.json")?)?;
        ensure!(
            recorded == credentials,
            "private credential fingerprints differ"
        );
        let installer_probe: Value =
            serde_json::from_slice(&extract(&seed_path, &artifacts, "probe.json")?)?;
        if local_diagnostic {
            ensure!(
                installer_probe["secure_boot"] != true && installer_probe["tpm2"] == false,
                "local diagnostic guest must use nonsecure firmware and no TPM"
            );
        } else {
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
        }
        for file in [
            "layout.json",
            "enrollment.json",
            "setup.json",
            "versions.txt",
            "leak-scan.json",
            "finaliser-leak-scan.json",
            "tokens.json",
            "preload-check.json",
        ] {
            extract(&seed_path, &artifacts, file)?;
        }
        let scan: Value =
            serde_json::from_slice(&fs::read(artifacts.join("installer-leak-scan.json"))?)?;
        let final_scan: Value = serde_json::from_slice(&fs::read(
            artifacts.join("installer-finaliser-leak-scan.json"),
        )?)?;
        if interrupted {
            ensure!(
                final_scan["target_mounts_retained"] == true
                    && final_scan["target_directories_checked"]
                        .as_u64()
                        .is_some_and(|count| count > 0),
                "interrupted shutdown did not retain the target scan roots"
            );
        }
        for scan in [scan, final_scan] {
            ensure!(
                scan["secret_found"] == false
                    && scan["journal_scanned"] == true
                    && scan["read_errors"] == 0
                    && scan["credentials_checked"] == 2,
                "private guest secret scan failed"
            );
        }
        let original_tokens: Value =
            serde_json::from_slice(&fs::read(artifacts.join("installer-tokens.json"))?)?;
        if local_diagnostic && !interrupted {
            stage = "diagnostic_fallback";
            fallback(
                &ws,
                &disk,
                &fixture.spec,
                &work,
                &artifacts,
                if layout_refusal {
                    &recovery
                } else {
                    &passphrase
                },
                &credentials,
                &installer_probe,
                &original_tokens,
                &expected,
                if layout_refusal {
                    "layout-recovery"
                } else {
                    "no-tpm"
                },
            )?;
            fixture.provider.destroy_owned(&mut fixture.vm)?;
            stage = "diagnostic_complete";
            return Ok(());
        }
        if !local_diagnostic {
            stage = "pcr_mismatch_first_boot";
            fallback(
                &ws,
                &disk,
                &fixture.spec,
                &work,
                &artifacts,
                &passphrase,
                &credentials,
                &installer_probe,
                &original_tokens,
                &expected,
                "pcr-mismatch",
            )?;
        }
        stage = "recovery_key_boot";
        fallback(
            &ws,
            &disk,
            &fixture.spec,
            &work,
            &artifacts,
            &recovery,
            &credentials,
            &installer_probe,
            &original_tokens,
            &expected,
            "recovery",
        )?;
        if interrupted {
            stage = "interrupted_completion_repair";
            let (reissued, repaired_tokens) = interrupted_repair(
                &mut fixture,
                &artifacts,
                &passphrase,
                &mut credentials,
                &original_tokens,
                &expected,
            )?;
            stage = "reissued_recovery_key_boot";
            fallback(
                &ws,
                &disk,
                &fixture.spec,
                &work,
                &artifacts,
                &reissued,
                &credentials,
                &installer_probe,
                &repaired_tokens,
                &expected,
                "reissued-recovery",
            )?;
        } else {
            stage = "first_installed_boot";
            fixture.disk_boot()?;
            let booted = assertions(&mut fixture, &artifacts, &expected, &installer_probe);
            fixture.logs(&artifacts, "installed")?;
            booted?;
            guest_privacy(
                fixture.vm.running_mut()?,
                &artifacts,
                "installed-after-finalisation",
                &credentials,
            )?;
            stage = "cold_restart";
            poweroff(&mut fixture, &artifacts, "installed")?;
            fixture.disk_boot()?;
            let cold = artifacts.join("cold");
            fs::create_dir_all(&cold)?;
            assertions(&mut fixture, &cold, &expected, &installer_probe)?;
            guest_privacy(fixture.vm.running_mut()?, &cold, "cold", &credentials)?;
            fixture.logs(&artifacts, "cold")?;
            poweroff(&mut fixture, &artifacts, "cold")?;
            stage = "same_tpm_policy_mismatch";
            same_tpm_mismatch(
                &ws,
                &mut fixture,
                &artifacts,
                &passphrase,
                &credentials,
                &installer_probe,
                &original_tokens,
            )?;
            stage = "no_tpm_fallback";
            fallback(
                &ws,
                &disk,
                &fixture.spec,
                &work,
                &artifacts,
                &passphrase,
                &credentials,
                &installer_probe,
                &original_tokens,
                &expected,
                "no-tpm",
            )?;
        }
        let iso_hash = cmd::output(Command::new("sha256sum").arg(&iso))?;
        fs::write(artifacts.join("iso-sha256.txt"), iso_hash)?;
        stage = "cleanup";
        // Drop remains the failure-path backstop. On success, propagating
        // destruction errors prevents a retained private TPM bundle going green.
        fixture.provider.destroy_owned(&mut fixture.vm)?;
        stage = "privacy_export";
        let scan = artifact_privacy(&[&artifacts, &raw_logs(&artifacts)?], &credentials)?;
        diagnostics(&artifacts, &artifacts.join("diagnostics"), &credentials)?;
        fs::write(
            artifacts.join("final-artifact-privacy.json"),
            serde_json::to_vec_pretty(&scan)?,
        )?;
        stage = "complete";
        Ok(())
    })();
    if let Err(error) = &result {
        let _ = fs::write(
            published.join("failure-code.json"),
            serde_json::to_vec_pretty(&failure_code(error))?,
        );
        let _ = fs::write(
            artifacts.join("host-fixture-error.txt"),
            format!("{error:#}\n"),
        );
    }
    if credentials.is_array() && result.is_err() {
        let diagnostic_text = artifacts.join("diagnostics");
        let target = published.join("diagnostics");
        let capture = (|| {
            diagnostics(&artifacts, &diagnostic_text, &credentials)?;
            if !evidence_files(&diagnostic_text)?.is_empty() {
                copy_evidence(&diagnostic_text, &target)?;
                artifact_privacy(&[&target], &credentials)?;
            }
            Ok::<(), anyhow::Error>(())
        })();
        if capture.is_err() {
            for directory in [&diagnostic_text, &target] {
                if directory.exists() {
                    clear_evidence(directory)?;
                }
            }
            let _ = fs::write(
                published.join("diagnostics-unavailable.json"),
                b"{\"reason\":\"capture or privacy verification failed\"}\n",
            );
        }
    }
    if result.is_ok() {
        let export = (|| -> Result<()> {
            copy_evidence(&artifacts, &published)?;
            // Scan the actual copied/exported set too, including other CI
            // artifact tiers. The workflow repeats this gate after all cases.
            let root = std::env::var_os("ILEX_ARTIFACTS")
                .map(PathBuf::from)
                .unwrap_or_else(|| published.clone());
            artifact_privacy(&[&root], &credentials)?;
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
            &json!({"qualification": if result.is_ok() && !local_diagnostic {"passed"} else {"unqualified"}, "stage":stage, "elapsed_seconds":start.elapsed().as_secs_f64(), "error":result.as_ref().err().map(|_|"private failure details suppressed"), "storage":if preserve_data {"existing-free-space"} else if interrupted {"interrupted-completion"} else {"automatic-empty-disk"}, "scope":if local_diagnostic {"local nonsecure guest with no TPM"} else {"exact ISO, signed local candidate, installed boot; physical hardware remains unqualified"}}),
        )?,
    )?;
    result.map_err(|_| {
        anyhow::anyhow!("encrypted qualification failed at {stage}; private diagnostics suppressed")
    })
}
