//! Native Gen2 provider. A VM UUID, firmware variables, disk and local vTPM
//! remain one allocation through guest reboot and cold restart.
use crate::{host, qemu, received::Directory, runtime, transport};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use ilex_tooling_core::*;
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const PROVIDER: &str = "windows-hyperv-gen2";
pub const TEMPLATE: &str = "MicrosoftUEFICertificateAuthority";
pub const SCREENSHOT_SOURCE: &str = "Hyper-V CIM GetVirtualSystemThumbnailImage (RGB565)";
const GIB: u64 = 1 << 30;
const WAIT: Duration = Duration::from_secs(10800);

pub struct Provider {
    identity: CheckoutIdentity,
    state: PathBuf,
    host: std::collections::BTreeMap<String, String>,
}

impl Provider {
    pub fn new(identity: CheckoutIdentity, checkout_state: &Path) -> Result<Self> {
        check_run(&identity.owner)?;
        // VMMS still has MAX_PATH limits under the name it appends to Path.
        // Keep configuration in a short private directory keyed by full owner.
        let state = state_path(&identity)?;
        host::secure_dir(&state)?;
        let receipt = state.join("owner");
        let owner = host::lock(&state.join("owner.lock"))?;
        if receipt.try_exists()? {
            identity.authorize(fs::read_to_string(&receipt)?.trim())?;
        } else {
            host::atomic(&receipt, identity.owner.as_bytes())?;
        }
        drop(owner);
        fs::create_dir_all(checkout_state.join("gen2"))?;
        let host = request(&state, &state, "probe", json!({}))
            .context("Hyper-V Gen2 requires management access and the existing Default Switch")?;
        Ok(Self {
            identity,
            state,
            host: serde_json::from_value(host)?,
        })
    }

    pub fn active(identity: CheckoutIdentity) -> Result<Vec<Vm>> {
        let state = state_path(&identity)?;
        if !state.try_exists()? {
            return Ok(vec![]);
        }
        let _bound = Directory::open(&state)?;
        identity.authorize(fs::read_to_string(state.join("owner"))?.trim())?;
        let mut vms = Vec::new();
        for entry in fs::read_dir(&state)? {
            let dir = entry?.path();
            if !dir.join("resource.json").is_file() {
                continue;
            }
            let Ok(bound) = Directory::open(&dir) else {
                continue;
            };
            let attached = (|| -> Result<Vm> {
                let spec: VmSpec = serde_json::from_reader(bound.read("spec.json")?.file)?;
                let resource: OwnedResource =
                    serde_json::from_reader(bound.read("resource.json")?.file)?;
                let profile: SecurityProfile =
                    serde_json::from_reader(bound.read("profile.json")?.file)?;
                check_version(spec.schema)?;
                check_run(&spec.run)?;
                validate_vm_purpose(&spec.purpose)?;
                resource.authorize(&identity, &spec.run, &resource.opaque_id)?;
                check_run(&resource.opaque_id)?;
                ensure!(
                    spec.owner == identity.owner
                        && resource.provider == PROVIDER
                        && resource.paths == [dir.as_path()]
                        && state.join(&spec.run[..12]) == dir,
                    "foreign Gen2 control receipt"
                );
                let vm = Vm {
                    identity: identity.clone(),
                    spec,
                    state: state.clone(),
                    dir: dir.clone(),
                    resource,
                    profile,
                    lease: None,
                    slot: None,
                    removed: false,
                    installation_media: None,
                    view: Mutex::default(),
                };
                let status = vm.status()?;
                vm.assert_configuration(&status)?;
                ensure!(status["state"] == "Running", "Gen2 VM is not running");
                Ok(vm)
            })();
            // An allocation may disappear while another process inventories it.
            if let Ok(vm) = attached {
                vms.push(vm);
            }
        }
        Ok(vms)
    }

    pub fn profile(&self, spec: &VmSpec) -> SecurityProfile {
        let mut host = self.host.clone();
        for name in ["machine", "boot", "hyperv"] {
            if let Some(value) = host.get_mut(name) {
                *value = digest(value.as_bytes());
            }
        }
        SecurityProfile {
            protocol: 3,
            accelerator: spec.accelerator,
            resources: spec.resources.clone(),
            cpu_model: "hyperv-native".into(),
            guest_arch: spec.guest_arch,
            host,
            firmware: [
                ("provider".into(), PROVIDER.into()),
                ("secure_boot_template".into(), TEMPLATE.into()),
                ("tpm".into(), "hyperv-vtpm2-local-protector".into()),
            ]
            .into(),
        }
    }

    pub fn installed_parent(&self, artifact: &DiskArtifact) -> Result<PathBuf> {
        check_version(artifact.schema)?;
        ensure!(
            artifact.format == DiskFormat::Qcow2
                && artifact.location.executor_id
                    == format!("windows-owner:{}", self.identity.owner),
            "invalid Gen2 installed-disk receipt"
        );
        let source = &artifact.location.path;
        let checksum = &artifact.location.sha256;
        let hash = checksum
            .strip_prefix("sha256:")
            .context("Gen2 source checksum needs SHA256")?;
        ensure!(
            hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid Gen2 source checksum"
        );
        let mut delivered = bound_file(source)?;
        ensure!(
            host::hash_reader(&mut delivered.file)? == hash,
            "Gen2 delivered disk checksum mismatch"
        );
        delivered.file.rewind()?;
        let work = self.state.join("parents");
        fs::create_dir_all(&work)?;
        let runtime_lock = runtime::status()?.lock_sha256;
        let virtual_bytes = verify_qcow2_header(&mut delivered.file)?;
        ensure!(
            virtual_bytes == artifact.virtual_bytes,
            "qcow2 virtual size differs from receipt"
        );
        let key = digest(format!("gen2-standalone-v2:{checksum}:{runtime_lock}").as_bytes());
        let base = &key[7..];
        let parent = work.join(format!("{base}.vhdx"));
        let receipt = parent.with_extension("json");
        let _guard = host::lock(&parent.with_extension("lock"))?;
        if receipt.try_exists()? {
            let recorded: Value = serde_json::from_slice(&fs::read(&receipt)?)?;
            ensure!(
                recorded["schema"] == 1
                    && recorded["owner"] == self.identity.owner
                    && recorded["source_sha256"] == checksum.as_str()
                    && recorded["runtime"] == runtime_lock,
                "Gen2 immutable parent receipt mismatch"
            );
            host::verify(
                &parent,
                recorded["parent_sha256"]
                    .as_str()
                    .context("missing parent SHA256")?,
            )?;
        } else {
            ensure!(
                !parent.try_exists()?,
                "unreceipted Gen2 parent; inspect {}",
                parent.display()
            );
            let temporary = work.join(format!("{}.vhdx", random_id()?));
            let built = (|| -> Result<()> {
                convert(source, &temporary, "qcow2", Some(hash))?;
                let sha256 = host::hash_file(&temporary)?;
                fs::rename(&temporary, &parent)?;
                host::atomic(
                    &receipt,
                    &serde_json::to_vec(&json!({
                        "schema":1,"owner":self.identity.owner,"source_sha256":checksum,"parent_sha256":sha256,"runtime":runtime_lock,
                    }))?,
                )
            })();
            if temporary.try_exists()? {
                fs::remove_file(&temporary)?;
            }
            built?;
        }
        Ok(parent)
    }

    /// Attach OEMDRV as an ISO or make a private writable copy of its VHDX.
    pub fn create_installer(&self, spec: VmSpec, answers: &Path) -> Result<Vm> {
        ensure!(
            spec.iso.is_some() && matches!(spec.boot, BootSource::BlankDisk { .. }),
            "Gen2 installer needs an ISO and blank target"
        );
        ensure!(
            matches!(
                answers.extension().and_then(|e| e.to_str()),
                Some("iso" | "vhdx")
            ),
            "OEMDRV must be ISO or VHDX"
        );
        self.create_with_media(spec, Some(answers), None)
    }

    /// Build the optional Anaconda OEMDRV DVD from a caller-owned directory.
    pub fn write_oemdrv(&self, source: &Path, output: &Path) -> Result<()> {
        let work = output
            .parent()
            .context("OEMDRV needs an owned output directory")?;
        let _source = Directory::open(source)?;
        let _target = Directory::open(work)?;
        ensure!(
            source.parent() == Some(work),
            "OEMDRV sources must be beside the output"
        );
        request(
            &self.state,
            work,
            "seed",
            json!({"seed_directory":source, "seed":output, "label":"OEMDRV"}),
        )?;
        Ok(())
    }

    /// Switch the original powered-off VM to its installed disk, with no
    /// firmware, disk or vTPM recreation. Retrying an interrupted eject is safe.
    pub fn boot_installed(&self, vm: &mut Vm, keep_ssh_seed: bool) -> Result<VmHandle> {
        ensure!(
            vm.lease.is_some()
                && vm.spec.iso.is_some()
                && matches!(vm.spec.boot, BootSource::BlankDisk { .. }),
            "expected owned Gen2 installer handle"
        );
        let handle = self.inspect(vm)?;
        let seed = keep_ssh_seed.then(|| vm.dir.join("seed.iso"));
        let status = vm.call(
            "installed-boot",
            json!({
                "disk":vm.dir.join("fixture.vhdx"),
                "answer_disk":vm.dir.join("answers.vhdx"),
                "answers":vm.installation_media,
                "iso":vm.spec.iso,
                "seed":seed,
                "original_seed":vm.dir.join("seed.iso"),
            }),
        )?;
        vm.assert_configuration(&status)?;
        Ok(handle)
    }

    /// Read the detached, guest-written OEMDRV as a raw disk before booting
    /// the installed system. The snapshot lives in private provider state
    /// for the callback only. Callers extract their private recovery envelope
    /// and select evidence under their own publication policy.
    pub fn with_installer_evidence<T>(
        &self,
        vm: &mut Vm,
        read: impl FnOnce(&Path) -> Result<T>,
    ) -> Result<T> {
        ensure!(
            vm.lease.is_some()
                && vm.installation_media.as_deref().and_then(Path::extension)
                    == Some(std::ffi::OsStr::new("vhdx")),
            "Gen2 installer evidence requires an owned writable OEMDRV"
        );
        self.inspect(vm)?;
        let source = vm.dir.join("answers.vhdx");
        vm.call("installer-evidence", json!({"answer_disk":source}))?;
        let _source = bound_file(&source)?;
        let private = vm.dir.join(format!("installer-evidence-{}", random_id()?));
        fs::create_dir(&private)?;
        let raw = private.join("oemdrv.raw");
        let result = (|| {
            let runtime_dir = runtime::fetch()?;
            let output = host::output_owned(
                runtime::command(&runtime_dir, "qemu-img.exe")
                    .current_dir(&vm.dir)
                    .args(["convert", "-f", "vhdx", "-O", "raw", "answers.vhdx"])
                    .arg(&raw),
                Stdio::null(),
            )?;
            ensure!(output.status.success(), "Gen2 OEMDRV readback failed");
            read(&raw)
        })();
        let cleanup = remove_tree(&vm.dir, &private);
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => {
                Err(error.context(format!("private OEMDRV cleanup failed: {cleanup:#}")))
            }
        }
    }

    /// Supply only the recovery digest to the disposable installed-disk fixture.
    pub fn create_recovery_fixture(&self, spec: VmSpec, digest: &str) -> Result<Vm> {
        ensure!(
            spec.ssh.is_some()
                && digest.len() == 64
                && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid recovery fixture digest"
        );
        self.create_with_media(spec, None, Some(digest))
    }

    fn create_with_media(
        &self,
        spec: VmSpec,
        answers: Option<&Path>,
        recovery_digest: Option<&str>,
    ) -> Result<Vm> {
        self.validate(&spec)?;
        let run = spec.run.clone();
        let dir = self.state.join(&run[..12]);
        fs::create_dir(&dir)?;
        let prepare = (|| -> Result<Vm> {
            let lease = host::lock(&dir.join("lease.lock"))?;
            host::atomic(&dir.join("spec.json"), &serde_json::to_vec(&spec)?)?;
            let profile = self.profile(&spec);
            fs::write(dir.join("profile.json"), serde_json::to_vec(&profile)?)?;
            let mut resource = OwnedResource {
                schema: SCHEMA_VERSION,
                owner: spec.owner.clone(),
                run: spec.run.clone(),
                provider: PROVIDER.into(),
                opaque_id: String::new(),
                created_unix_ms: now()?,
                paths: vec![dir.clone()],
            };
            host::atomic(&dir.join("intent.json"), &serde_json::to_vec(&resource)?)?;
            let (parent, blank) = match &spec.boot {
                BootSource::Disk { path, .. } => (Some(regular_file(path)?), 0),
                BootSource::BlankDisk { bytes, .. } => (None, *bytes),
                _ => unreachable!(),
            };
            let iso = spec.iso.as_deref().map(regular_file).transpose()?;
            let answers = answers.map(regular_file).transpose()?;
            let _answers = answers.as_deref().map(bound_file).transpose()?;
            let seed = spec
                .ssh
                .as_ref()
                .map(|ssh| prepare_seed(&dir, &spec.run, ssh, recovery_digest))
                .transpose()?;
            let receipt = dir.join("resource.json");
            let mut body = owned_request(&self.identity, &spec, &resource, &dir);
            body.as_object_mut().unwrap().extend(json!({
                "receipt":receipt, "disk":dir.join("fixture.vhdx"), "parent":parent, "blank_bytes":blank,
                "iso":iso, "answers":answers, "network":!matches!(spec.network, NetworkIntent::Isolated),
                "seed":seed, "seed_directory":dir.join("seed"),
                "secure_boot":spec.secure_boot == Requirement::Required, "tpm":spec.tpm != TpmLifecycle::Off,
                "memory_bytes":spec.resources.memory_bytes, "vcpus":spec.resources.vcpus,
            }).as_object().unwrap().clone());
            let created = request(&self.state, &dir, "create", body);
            if let Ok(bytes) = fs::read(&receipt) {
                resource = serde_json::from_slice(&bytes)?;
                self.check_resource(&resource, &spec, &dir)?;
            }
            let status = created?;
            self.check_resource(&resource, &spec, &dir)?;
            let vm = Vm {
                identity: self.identity.clone(),
                spec,
                state: self.state.clone(),
                dir: dir.clone(),
                resource,
                lease: Some(lease),
                slot: None,
                removed: false,
                profile,
                installation_media: answers,
                view: Mutex::default(),
            };
            vm.assert_configuration(&status)?;
            if seed.is_some() {
                remove_tree(&dir, &dir.join("seed"))?;
                fs::remove_file(dir.join("host"))?;
            }
            Ok(vm)
        })();
        if prepare.is_err() && dir.exists() {
            if dir.join("intent.json").try_exists()? {
                if let Err(error) = self.cleanup_run(&run) {
                    eprintln!("Gen2 cleanup: {error:#}; retained run {run}");
                }
            } else {
                remove_tree(&self.state, &dir)?;
            }
        }
        prepare
    }

    pub fn begin_qualification(
        &self,
        spec: &VmSpec,
        artifacts: &Path,
    ) -> Result<qualification::Run> {
        self.identity.authorize(&spec.owner)?;
        let profile = self.profile(spec);
        qualification::Run::begin(
            qualification::record_path(&self.state, &profile)?,
            spec,
            profile,
            artifacts,
            qemu::process_start,
        )
    }

    /// Reconcile an interrupted run only when its lease is free and the
    /// journal has the exact allocated ID. The Hyper-V helper also checks Notes.
    pub fn cleanup_run(&self, run: &str) -> Result<()> {
        check_run(run)?;
        let dir = self.state.join(&run[..12]);
        let _bound = Directory::open(&dir)?;
        let _lease = host::lock(&dir.join("lease.lock"))?;
        let spec: VmSpec = serde_json::from_slice(&fs::read(dir.join("spec.json"))?)?;
        check_version(spec.schema)?;
        self.identity.authorize(&spec.owner)?;
        ensure!(spec.run == run, "Gen2 cleanup run mismatch");
        validate_vm_purpose(&spec.purpose)?;
        let receipt = dir.join("resource.json");
        let resource: OwnedResource = if receipt.try_exists()? {
            let resource = serde_json::from_slice(&fs::read(receipt)?)?;
            self.check_resource(&resource, &spec, &dir)?;
            resource
        } else {
            let intent: OwnedResource =
                serde_json::from_slice(&fs::read(dir.join("intent.json"))?)?;
            intent.authorize(&self.identity, run, "")?;
            ensure!(
                intent.provider == PROVIDER && intent.paths == [dir.as_path()],
                "invalid Gen2 allocation intent"
            );
            intent
        };
        let mut body = owned_request(&self.identity, &spec, &resource, &dir);
        body["disk"] = json!(dir.join("fixture.vhdx"));
        request(&self.state, &dir, "reconcile", body)?;
        if !resource.opaque_id.is_empty() {
            reconcile_slots(&resource)?;
        }
        drop(_lease);
        drop(_bound);
        remove_tree(&self.state, &dir)
    }

    pub fn reconcile_if_present(&self, run: &str) -> Result<()> {
        check_run(run)?;
        if self.state.join(&run[..12]).try_exists()? {
            self.cleanup_run(run)?;
        }
        Ok(())
    }

    fn check_resource(&self, resource: &OwnedResource, spec: &VmSpec, dir: &Path) -> Result<()> {
        check_run(&resource.opaque_id)?;
        resource.authorize(&self.identity, &spec.run, &resource.opaque_id)?;
        ensure!(
            resource.provider == PROVIDER
                && resource.paths == [dir]
                && resource.created_unix_ms > 0,
            "invalid Gen2 resource receipt"
        );
        Ok(())
    }
}

pub fn spec(
    identity: &CheckoutIdentity,
    purpose: &str,
    resources: Resources,
    boot: BootSource,
) -> Result<VmSpec> {
    let run = random_id()?;
    Ok(VmSpec {
        schema: SCHEMA_VERSION,
        owner: identity.owner.clone(),
        purpose: purpose.into(),
        run: run.clone(),
        host_arch: Architecture::host()?,
        guest_arch: Architecture::X86_64,
        boot,
        iso: None,
        additional_disks: vec![],
        resources,
        display: VmDisplay::Headless {
            width: 1024,
            height: 768,
        },
        firmware: Firmware::Uefi,
        firmware_state: None,
        secure_boot: Requirement::Required,
        tpm: TpmLifecycle::Persistent { bundle_id: run },
        network: NetworkIntent::Isolated,
        ssh: None,
        require: vec![],
        accelerator: Accelerator::Native,
        security_launch: SecurityLaunch::RequireQualified,
    })
}

pub struct Vm {
    identity: CheckoutIdentity,
    spec: VmSpec,
    state: PathBuf,
    dir: PathBuf,
    resource: OwnedResource,
    lease: Option<File>,
    slot: Option<Slot>,
    removed: bool,
    profile: SecurityProfile,
    installation_media: Option<PathBuf>,
    view: Mutex<((u32, u32), u64)>,
}

/// A private display generation scoped to one VM and one concealment.
pub struct ConsoleRedaction {
    vm: String,
    generation: String,
}

impl Vm {
    pub fn purpose(&self) -> &str {
        &self.spec.purpose
    }
    pub fn name(&self) -> String {
        name(&self.identity, &self.spec)
    }
    pub fn generation(&self) -> u64 {
        self.view.lock().unwrap().1
    }
    /// Release capture only after an authenticated guest-side acknowledgement
    /// that the no-echo prompt consumed the input and the display is safe.
    pub fn acknowledge_sensitive_input(
        &self,
        redaction: &ConsoleRedaction,
        check: &[String],
        expected: &[u8],
    ) -> Result<bool> {
        ensure!(
            self.lease.is_some() && redaction.vm == self.resource.opaque_id && !expected.is_empty(),
            "sensitive display acknowledgement requires the owning VM and a public marker"
        );
        let actual = self
            .guest_exec(check)
            .map_err(|_| anyhow::anyhow!("sensitive console acknowledgement failed"))?;
        ensure!(
            actual == expected,
            "missing sensitive console acknowledgement"
        );
        let _console = host::lock(&self.dir.join("console.lock"))?;
        let latch = self.dir.join("sensitive-display");
        if !latch.try_exists()? || fs::read(&latch)? != redaction.generation.as_bytes() {
            return Ok(false);
        }
        fs::remove_file(latch)?;
        Ok(true)
    }

    /// Latch screenshot redaction before a guest displays or receives a secret.
    pub fn conceal_console(&self) -> Result<ConsoleRedaction> {
        let _console = host::lock(&self.dir.join("console.lock"))?;
        self.latch_capture()
    }

    fn latch_capture(&self) -> Result<ConsoleRedaction> {
        let generation = random_id()?;
        host::atomic(&self.dir.join("sensitive-display"), generation.as_bytes())?;
        Ok(ConsoleRedaction {
            vm: self.resource.opaque_id.clone(),
            generation,
        })
    }

    pub fn send_sensitive_text(&self, text: String) -> Result<ConsoleRedaction> {
        self.send_key(&Input::Text {
            text,
            layout: "us".into(),
            sensitive: true,
        })?
        .context("sensitive input did not obtain a display generation")
    }

    /// Inspect controller files during the live, redacted input self-check.
    /// Guest disks and provisioning keys are not controller diagnostics.
    pub fn audit_console_state(&self, secret: &[u8]) -> Result<()> {
        ensure!(!secret.is_empty(), "privacy probe requires a canary");
        let _console = host::lock(&self.dir.join("console.lock"))?;
        for entry in fs::read_dir(&self.dir)? {
            let file = entry?.path();
            if matches!(
                file.extension().and_then(|s| s.to_str()),
                Some("json" | "ps1" | "log")
            ) {
                let bytes = fs::read(file)?;
                ensure!(
                    !bytes.windows(secret.len()).any(|window| window == secret)
                        && !bytes.windows(8).any(|window| window == b"\"events\""),
                    "Gen2 controller state contains sensitive keyboard evidence"
                );
            }
        }
        Ok(())
    }
    pub fn interactive(&self) -> Result<()> {
        let (mut ssh, endpoint) = self.ssh_command("ssh.exe")?;
        let (_job, mut child) = host::spawn_owned(ssh.arg("-t").arg(endpoint))?;
        loop {
            if let Some(status) = child.try_wait()? {
                ensure!(status.success(), "Gen2 guest shell exited {status}");
                return Ok(());
            }
            ensure!(!transport::cancelled(), "Gen2 guest shell cancelled");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn call(&self, op: &str, options: Value) -> Result<Value> {
        let mut body = owned_request(&self.identity, &self.spec, &self.resource, &self.dir);
        body.as_object_mut()
            .unwrap()
            .extend(options.as_object().unwrap().clone());
        request(&self.state, &self.dir, op, body)
    }

    pub fn status(&self) -> Result<Value> {
        self.call("status", json!({}))
    }

    pub fn handle(&self) -> VmHandle {
        VmHandle {
            schema: SCHEMA_VERSION,
            owner: self.spec.owner.clone(),
            run: self.spec.run.clone(),
            provider: PROVIDER.into(),
            opaque_id: self.resource.opaque_id.clone(),
            guest_arch: self.spec.guest_arch,
        }
    }

    fn ssh_endpoint(&self) -> Result<GuestEndpoint> {
        let provision = self
            .spec
            .ssh
            .as_ref()
            .context("no SSH provisioned for this Gen2 VM")?;
        let found = self.call("address", json!({}))?;
        let host = found["addresses"]
            .as_array()
            .context("missing Gen2 address list")?
            .iter()
            .filter_map(Value::as_str)
            .find(|s| {
                s.parse::<Ipv4Addr>().is_ok_and(|addr| {
                    !addr.is_loopback() && !addr.is_unspecified() && !addr.is_link_local()
                })
            })
            .with_context(|| {
                format!(
                    "no IPv4 address for Gen2 VM {} on Default Switch (MAC {})",
                    self.resource.opaque_id, found["mac"]
                )
            })?;
        let public = fs::read_to_string(self.dir.join("host.pub"))?;
        Ok(GuestEndpoint {
            host: host.into(),
            port: 22,
            user: provision.user.clone(),
            host_key: Some(public.trim().into()),
        })
    }

    fn ssh_command(&self, program: &str) -> Result<(Command, String)> {
        let endpoint = self.ssh_endpoint()?;
        let mut cmd = Command::new(program);
        cmd.args([
            "-F",
            "NUL",
            "-o",
            "GlobalKnownHostsFile=NUL",
            "-o",
            "BatchMode=yes",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=3",
            "-o",
            "ServerAliveInterval=3",
            "-o",
            "ServerAliveCountMax=2",
            "-o",
            "LogLevel=ERROR",
            "-o",
        ])
        .arg(format!(
            "UserKnownHostsFile={}",
            self.dir.join("known_hosts").display()
        ))
        .arg("-o")
        .arg(format!("HostKeyAlias={}", self.spec.run))
        .arg("-i")
        .arg(self.dir.join("access"));
        Ok((cmd, format!("{}@{}", endpoint.user, endpoint.host)))
    }

    pub fn wait_ssh(&self, old_boot: Option<&str>, timeout: Duration) -> Result<String> {
        wait(timeout, || {
            let boot =
                self.guest_exec(&["cat".into(), "/proc/sys/kernel/random/boot_id".into()])?;
            let boot = String::from_utf8(boot)?.trim().to_owned();
            check_run(&boot)?;
            ensure!(
                Some(boot.as_str()) != old_boot,
                "guest is still on the previous boot"
            );
            Ok(boot)
        })
    }

    pub fn wait_off(&self, timeout: Duration) -> Result<()> {
        wait(timeout, || {
            ensure!(
                self.status()?["state"] == "Off",
                "Gen2 guest has not powered off"
            );
            Ok(())
        })
    }

    pub fn reboot(&mut self, old_boot: &str, timeout: Duration) -> Result<String> {
        let requested =
            self.guest_exec(&["systemctl".into(), "--no-block".into(), "reboot".into()]);
        self.view.lock().unwrap().0 = (0, 0);
        let boot = self.wait_ssh(Some(old_boot), timeout);
        power_transition(requested, boot)
    }

    pub fn poweroff(&self, timeout: Duration) -> Result<()> {
        let requested =
            self.guest_exec(&["systemctl".into(), "--no-block".into(), "poweroff".into()]);
        power_transition(requested, self.wait_off(timeout))
    }

    /// Acknowledgement of a WMI method alone is insufficient. The fixture
    /// observes guest input events for this exact button and position first.
    pub fn qualify_pointer(&self, frame: &Frame, x: u32, y: u32) -> Result<Vec<u8>> {
        self.guest_exec(&[
            "sh".into(),
            "-c".into(),
            "test -e /sys/module/hid_hyperv; grep -q 'Microsoft Vmbus HID' /proc/bus/input/devices"
                .into(),
        ])?;
        ensure!(
            frame.width > 1 && frame.height > 1 && x < frame.width && y < frame.height,
            "invalid Gen2 pointer probe"
        );
        let x_axis = x * 32767 / (frame.width - 1);
        let y_axis = y * 32767 / (frame.height - 1);
        ensure!(
            x_axis.abs_diff(y_axis) > 128,
            "use independent pointer targets on X and Y"
        );
        let result = self.observe_input(
            "pointer-boot",
            &["pointer".into(), x_axis.to_string(), y_axis.to_string()],
            || self.click(frame, frame.generation, x, y),
        )?;
        Ok(result)
    }

    pub fn qualify_session_keyboard(&self) -> Result<Vec<u8>> {
        wait(Duration::from_secs(60), || {
            self.guest_exec(&[
                "systemctl".into(),
                "is-active".into(),
                "display-manager.service".into(),
            ])
        })?;
        self.observe_input("keyboard-boot", &["keyboard".into()], || {
            self.key(&Input::Chord { codes: vec![0x58] })
        })
    }

    fn observe_input(
        &self,
        receipt: &str,
        arguments: &[String],
        send: impl FnOnce() -> Result<()>,
    ) -> Result<Vec<u8>> {
        let (mut ssh, target) = self.ssh_command("ssh.exe")?;
        let mut script = vec![
            "python3".into(),
            "-I".into(),
            "-c".into(),
            include_str!("gen2-input.py").into(),
        ];
        script.extend_from_slice(arguments);
        let (job, mut child) = host::spawn_owned(
            ssh.arg(target)
                .arg(shell_command(&script)?)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )?;
        use std::io::{BufRead, BufReader, Read};
        let mut stdout = BufReader::new(child.stdout.take().context("no input probe stdout")?);
        let mut ready = String::new();
        let setup = (|| -> Result<_> {
            stdout.read_line(&mut ready)?;
            ensure!(
                ready.trim() == "ready",
                "guest input probe did not become ready"
            );
            send()?;
            let mut evidence = Vec::new();
            stdout.read_to_end(&mut evidence)?;
            Ok(evidence)
        })();
        let bytes = match setup {
            Ok(bytes) => bytes,
            Err(error) => {
                drop(job);
                let status = child.wait_with_output()?;
                return Err(
                    error.context(String::from_utf8_lossy(&status.stderr).trim().to_owned())
                );
            }
        };
        let status = child.wait_with_output()?;
        ensure!(
            status.status.success() && !bytes.is_empty(),
            "Linux did not receive the expected Hyper-V input: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        let boot = std::str::from_utf8(&bytes)?
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("boot "))
            .context("input observer did not report its boot")?;
        check_run(boot)?;
        // This ID comes from the same observer as the evdev events. A reboot
        // between its exit and the local write cannot attest the next boot.
        let _console = host::lock(&self.dir.join("console.lock"))?;
        host::atomic(&self.dir.join(receipt), format!("{boot}\n").as_bytes())?;
        Ok(bytes)
    }

    fn input_available(&self, receipt: &str) -> bool {
        // Attached clients verify the journaled delivery against the live boot.
        // Mere existence of Msvm_SyntheticMouse never qualifies preboot input.
        fs::read(self.dir.join(receipt)).ok().is_some_and(|boot| {
            self.guest_exec(&["cat".into(), "/proc/sys/kernel/random/boot_id".into()])
                .is_ok_and(|current| current == boot)
        })
    }

    fn send_key(&self, input: &Input) -> Result<Option<ConsoleRedaction>> {
        input.validate()?;
        let event = |code, down, delay| json!({"code":code,"down":down,"delay":delay});
        let mut events = Vec::new();
        match input {
            Input::PhysicalKey { code, down } => events.push(event(scan_to_vk(*code)?, *down, 0)),
            Input::Chord { codes } => {
                let vk = codes
                    .iter()
                    .copied()
                    .map(scan_to_vk)
                    .collect::<Result<Vec<_>>>()?;
                for &code in &vk {
                    events.push(event(code, true, 80));
                }
                for &code in vk.iter().rev() {
                    events.push(event(code, false, 25));
                }
            }
            Input::Text { text, layout, .. } => {
                ensure!(
                    layout == "us" && text.len() <= 128,
                    "Gen2 supports batches of up to 128 US-ASCII keystrokes"
                );
                for b in text.bytes() {
                    let (code, shift) = ascii_vk(b)?;
                    if shift {
                        events.push(event(0x10, true, 15));
                    }
                    events.push(event(code, true, 35));
                    events.push(event(code, false, 15));
                    if shift {
                        events.push(event(0x10, false, 15));
                    }
                }
            }
        };
        let _console = host::lock(&self.dir.join("console.lock"))?;
        let sensitive = matches!(
            input,
            Input::Text {
                sensitive: true,
                ..
            }
        );
        let redaction = if sensitive {
            // Publish the redaction latch before sending any keystroke. It
            // persists across controller processes and failed sends.
            Some(self.latch_capture()?)
        } else {
            None
        };
        let mut body = owned_request(&self.identity, &self.spec, &self.resource, &self.dir);
        body["events"] = json!(events);
        body["leave_held"] = json!(matches!(input, Input::PhysicalKey { down: true, .. }));
        request_inner(&self.dir, "keys", body, sensitive)?;
        Ok(redaction)
    }

    fn click(&self, frame: &Frame, generation: u64, x: u32, y: u32) -> Result<()> {
        let view = self.view.lock().unwrap();
        frame.validate_pointer(&self.handle(), view.1, x, y)?;
        ensure!(generation == view.1, "stale Gen2 pointer generation");
        self.call(
            "pointer",
            json!({
                "width":frame.width, "height":frame.height,
                "x":x, "y":y,
            }),
        )?;
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        if !self.removed {
            self.call("destroy", json!({}))?;
            self.removed = true;
        }
        if let Some(slot) = &self.slot {
            slot.release(&self.resource)?;
        }
        self.slot = None;
        self.lease = None;
        if self.dir.exists() {
            remove_tree(&self.state, &self.dir)?;
        }
        Ok(())
    }
}
impl Drop for Vm {
    fn drop(&mut self) {
        if self.lease.is_none() {
            return;
        }
        if let Err(error) = self.cleanup() {
            eprintln!(
                "Gen2 teardown failed: {error:#}; retained run {} at {}",
                self.spec.run,
                self.dir.display()
            );
        }
    }
}

impl VmProvider for Provider {
    type Vm = Vm;
    fn probe(&self, spec: &VmSpec) -> Result<VmCapabilities> {
        self.identity.authorize(&spec.owner)?;
        let profile = self.profile(spec);
        let record = qualification::record_path(&self.state, &profile)?;
        let mut caps = capabilities(spec);
        qualification::apply(
            &mut caps,
            qualification::load(&record, &spec.owner, &profile)?,
        );
        Ok(caps)
    }

    fn validate(&self, spec: &VmSpec) -> Result<()> {
        spec.validate(&self.identity, &self.probe(spec)?)?;
        check_run(&spec.run)?;
        ensure!(
            spec.purpose.len() <= 28,
            "Gen2 VM purpose exceeds 28 ASCII characters"
        );
        ensure!(
            spec.accelerator == Accelerator::Native
                && spec.host_arch == Architecture::X86_64
                && spec.guest_arch == Architecture::X86_64,
            "Gen2 requires native x86_64"
        );
        ensure!(
            spec.firmware == Firmware::Uefi && spec.firmware_state.is_none(),
            "Gen2 requires internal UEFI state"
        );
        ensure!(
            (2 * GIB..=6 * GIB).contains(&spec.resources.memory_bytes)
                && (1..=4).contains(&spec.resources.vcpus),
            "Gen2 requires 2–6 GiB and 1–4 vCPUs"
        );
        ensure!(
            matches!(spec.display, VmDisplay::Headless { .. }),
            "Gen2 control uses the WMI framebuffer"
        );
        ensure!(
            spec.additional_disks.is_empty(),
            "Gen2 additional disks are unsupported"
        );
        ensure!(
            matches!(
                spec.boot,
                BootSource::Disk {
                    format: DiskFormat::Vhdx,
                    writable: true,
                    ..
                } | BootSource::BlankDisk {
                    format: DiskFormat::Vhdx,
                    ..
                }
            ),
            "Gen2 requires a blank or writable differencing VHDX"
        );
        if let TpmLifecycle::Persistent { bundle_id } = &spec.tpm {
            ensure!(
                bundle_id == &spec.run && spec.secure_boot == Requirement::Required,
                "persistent Gen2 vTPM belongs to the Secure Boot VM run"
            );
        }
        ensure!(
            matches!(
                spec.network,
                NetworkIntent::Isolated | NetworkIntent::User { .. }
            ),
            "Gen2 supports an isolated guest or Default Switch only"
        );
        if let Some(ssh) = &spec.ssh {
            ensure!(
                matches!(spec.network, NetworkIntent::User { ssh: true }) && ssh.user == "root",
                "Gen2 fixture SSH needs the Default Switch and the configured guest root key"
            );
        }
        Ok(())
    }

    fn create(&self, spec: VmSpec) -> Result<Vm> {
        self.create_with_media(spec, None, None)
    }

    fn start(&self, vm: &mut Vm) -> Result<VmHandle> {
        ensure!(
            vm.lease.is_some(),
            "attached Gen2 controls cannot start VMs"
        );
        let handle = self.inspect(vm)?;
        self.validate(&vm.spec)?;
        ensure!(vm.slot.is_none(), "Gen2 VM already holds a slot");
        let (slot, admission) = admit(vm.spec.resources.memory_bytes, &vm.resource)?;
        // Keep the slot even if the start reply is lost after VMMS starts it.
        vm.slot = Some(slot);
        vm.call("start", json!({}))?;
        drop(admission);
        vm.view.lock().unwrap().0 = (0, 0);
        Ok(handle)
    }

    fn inspect(&self, vm: &Vm) -> Result<VmHandle> {
        self.check_resource(&vm.resource, &vm.spec, &vm.dir)?;
        ensure!(
            vm.state == self.state,
            "Gen2 VM belongs to another provider state"
        );
        vm.assert_configuration(&vm.status()?)?;
        Ok(vm.handle())
    }

    fn stop(&self, vm: &mut Vm) -> Result<()> {
        ensure!(vm.lease.is_some(), "attached Gen2 controls cannot stop VMs");
        self.inspect(vm)?;
        vm.call("stop", json!({}))?;
        if let Some(slot) = &vm.slot {
            slot.release(&vm.resource)?;
        }
        vm.slot = None;
        vm.view.lock().unwrap().0 = (0, 0);
        Ok(())
    }

    fn destroy_owned(&self, vm: &mut Vm) -> Result<()> {
        ensure!(
            vm.lease.is_some(),
            "attached Gen2 controls cannot destroy VMs"
        );
        self.check_resource(&vm.resource, &vm.spec, &vm.dir)?;
        ensure!(
            vm.state == self.state,
            "Gen2 VM belongs to another provider state"
        );
        vm.cleanup()
    }

    fn guest_endpoint(&self, vm: &Vm) -> Result<GuestEndpoint> {
        self.inspect(vm)?;
        vm.ssh_endpoint()
    }
}

impl Vm {
    fn assert_configuration(&self, status: &Value) -> Result<()> {
        ensure!(
            status["id"] == self.resource.opaque_id
                && status["memory_bytes"] == self.spec.resources.memory_bytes
                && status["vcpus"] == self.spec.resources.vcpus
                && status["dynamic_memory"] == false
                && status["automatic_checkpoints"] == false
                && status["secure_boot"] == (self.spec.secure_boot == Requirement::Required)
                && status["template"] == TEMPLATE
                && status["tpm"] == (self.spec.tpm != TpmLifecycle::Off)
                && status["bind_to_host_tpm"] == false,
            "Gen2 settings differ from the requested profile: {status}"
        );
        Ok(())
    }
    pub fn provisional_capabilities(&self, run: &qualification::Run) -> Result<VmCapabilities> {
        let record = run.provisional_pass()?;
        ensure!(
            self.lease.is_some()
                && self.spec.security_launch == SecurityLaunch::QualificationFixture
                && self.profile == record.profile
                && record
                    .evidence
                    .as_ref()
                    .is_some_and(|e| e.owner == self.spec.owner),
            "provisional capabilities require an owned qualification fixture of the exact profile"
        );
        self.live_capabilities(record)
    }

    fn live_capabilities(&self, record: SecurityQualification) -> Result<VmCapabilities> {
        let status = self.status()?;
        self.assert_configuration(&status)?;
        let mut caps = capabilities(&self.spec);
        caps.vm_id = Some(self.resource.opaque_id.clone());
        caps.runtime = match status["state"].as_str() {
            Some("Running") => VmRuntimeStatus::Running,
            Some("Off") => VmRuntimeStatus::Stopped,
            Some("Paused" | "Saved") => VmRuntimeStatus::Paused(status["state"].to_string()),
            _ => VmRuntimeStatus::Unknown,
        };
        let session_pointer =
            caps.runtime == VmRuntimeStatus::Running && self.input_available("pointer-boot");
        let session_keyboard =
            caps.runtime == VmRuntimeStatus::Running && self.input_available("keyboard-boot");
        caps.phase = if caps.runtime != VmRuntimeStatus::Running {
            BootPhase::Unknown
        } else if session_pointer || session_keyboard {
            BootPhase::Session
        } else {
            BootPhase::Boot
        };
        qualification::apply(&mut caps, record);
        qualification::restrict_live(&mut caps);
        if session_keyboard {
            caps.features
                .insert(Capability::SessionKeyboard, CapabilityStatus::Available);
        }
        if caps.phase == BootPhase::Boot
            && caps
                .qualification
                .as_ref()
                .is_some_and(|q| q.outcome == QualificationStatus::Passed)
        {
            caps.features
                .insert(Capability::PrebootKeyboard, CapabilityStatus::Available);
        }
        if session_pointer {
            caps.features
                .insert(Capability::SessionPointer, CapabilityStatus::Available);
        }
        let _console = host::lock(&self.dir.join("console.lock"))?;
        if self.dir.join("sensitive-display").try_exists()? {
            caps.features.insert(
                Capability::Screenshot,
                CapabilityStatus::Unavailable(
                    "capture withheld after sensitive console input".into(),
                ),
            );
        } else if caps.runtime != VmRuntimeStatus::Running {
            caps.features.insert(
                Capability::Screenshot,
                CapabilityStatus::Unavailable("capture requires a running VM".into()),
            );
        } else {
            let size = self.view.lock().unwrap().0;
            caps.screenshot_dimensions = (size.0 != 0).then_some(size);
        }
        Ok(caps)
    }
}

impl VmControl for Vm {
    fn capabilities(&self) -> Result<VmCapabilities> {
        let path = qualification::record_path(&self.state, &self.profile)?;
        self.live_capabilities(qualification::load(&path, &self.spec.owner, &self.profile)?)
    }

    fn capture(&self) -> Result<Frame> {
        let _console = host::lock(&self.dir.join("console.lock"))?;
        ensure!(
            !self.dir.join("sensitive-display").try_exists()?,
            "Gen2 capture withheld after sensitive console input"
        );
        let reply = self.call("capture", json!({}))?;
        let width: u32 = reply["width"]
            .as_u64()
            .context("no thumbnail width")?
            .try_into()?;
        let height: u32 = reply["height"]
            .as_u64()
            .context("no thumbnail height")?
            .try_into()?;
        ensure!(
            (1..=4096).contains(&width) && (1..=2160).contains(&height),
            "invalid Hyper-V thumbnail dimensions"
        );
        let bytes = STANDARD.decode(reply["rgb565"].as_str().context("no thumbnail bytes")?)?;
        ensure!(
            bytes.len() == 4 + (width * height * 2) as usize,
            "invalid Hyper-V thumbnail stride"
        );
        let mut pixels = Vec::with_capacity((width * height * 3) as usize);
        for &pair in bytes[4..].as_chunks::<2>().0 {
            let color = u16::from_le_bytes(pair);
            pixels.extend([
                (((color >> 11) & 31) * 255 / 31) as u8,
                (((color >> 5) & 63) * 255 / 63) as u8,
                ((color & 31) * 255 / 31) as u8,
            ]);
        }
        let mut view = self.view.lock().unwrap();
        if view.0 != (width, height) {
            view.0 = (width, height);
            view.1 += 1;
        }
        Ok(Frame {
            vm: self.handle(),
            captured_unix_ms: reply["captured_unix_ms"]
                .as_u64()
                .context("no thumbnail timestamp")?,
            generation: view.1,
            width,
            height,
            viewport: Viewport {
                x: 0,
                y: 0,
                width,
                height,
            },
            coordinate_space: CoordinateSpace::FramebufferPixels,
            pixels,
        })
    }

    fn key(&self, input: &Input) -> Result<()> {
        self.send_key(input).map(|_| ())
    }

    fn pointer(&self, frame: &Frame, generation: u64, x: u32, y: u32) -> Result<()> {
        ensure!(
            self.input_available("pointer-boot"),
            "Gen2 mouse has not been qualified in this guest boot"
        );
        self.click(frame, generation, x, y)
    }

    fn serial(&self) -> Result<Vec<u8>> {
        bail!("Gen2 serial is not provisioned")
    }

    fn guest_exec(&self, argv: &[String]) -> Result<Vec<u8>> {
        let remote = shell_command(argv)?;
        let (mut ssh, target) = self.ssh_command("ssh.exe")?;
        let output = host::output_owned(ssh.arg(target).arg(remote), Stdio::piped())?;
        ensure!(
            output.status.success(),
            "Gen2 SSH exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }

    fn upload(&self, local: &Path, guest: &str) -> Result<()> {
        validate_guest_path(guest)?;
        let (mut cmd, target) = self.ssh_command("scp.exe")?;
        host::command_ok(cmd.arg("--").arg(local).arg(format!("{target}:{guest}")))
    }
    fn download(&self, guest: &str, local: &Path) -> Result<()> {
        validate_guest_path(guest)?;
        let (mut cmd, target) = self.ssh_command("scp.exe")?;
        host::command_ok(cmd.arg("--").arg(format!("{target}:{guest}")).arg(local))
    }
}

fn capabilities(spec: &VmSpec) -> VmCapabilities {
    use Capability::*;
    use CapabilityStatus::*;
    let unavailable = |why: &str| Unavailable(why.into());
    let features = [
        (
            GpuRendering,
            unavailable("Gen2 has no qualified accelerated GPU"),
        ),
        (Screenshot, Available),
        (
            PrebootKeyboard,
            NotYetQualified("requires a passing UEFI keystroke test".into()),
        ),
        (
            SessionKeyboard,
            NotYetQualified(
                "requires a graphical Linux keyboard delivery probe on this boot".into(),
            ),
        ),
        (
            PrebootPointer,
            unavailable("synthetic mouse is a Linux session capability"),
        ),
        (
            SessionPointer,
            NotYetQualified("requires delivered Linux input events on this boot".into()),
        ),
        (Clipboard, unavailable("no Gen2 clipboard integration")),
        (
            Suspend,
            unavailable("Gen2 saved-state lifecycle is unqualified"),
        ),
        (Serial, unavailable("no Gen2 serial route configured")),
        (Start, Available),
        (Stop, Available),
        (Destroy, Available),
    ]
    .into();
    VmCapabilities {
        schema: SCHEMA_VERSION,
        provider: PROVIDER.into(),
        vm_id: None,
        phase: BootPhase::Unknown,
        features,
        provisioning: [
            (
                FirmwareEnforcement,
                if spec.secure_boot == Requirement::Required {
                    Available
                } else {
                    unavailable("Secure Boot disabled")
                },
            ),
            (
                PersistentTpm,
                if spec.tpm != TpmLifecycle::Off {
                    Available
                } else {
                    unavailable("no guest vTPM requested")
                },
            ),
        ]
        .into(),
        qualification: None,
        runtime: VmRuntimeStatus::Unknown,
        screenshot_source: Some(SCREENSHOT_SOURCE.into()),
        screenshot_dimensions: None,
        text_layout: Some("us".into()),
    }
}

fn name(identity: &CheckoutIdentity, spec: &VmSpec) -> String {
    format!("ilex-{}-{}-{}", identity.slug, spec.purpose, &spec.run[..8])
}

fn state_path(identity: &CheckoutIdentity) -> Result<PathBuf> {
    check_run(&identity.owner)?;
    Ok(host::root()?
        .join("g2")
        .join(identity.owner.replace('-', "")))
}

fn owned_request(
    identity: &CheckoutIdentity,
    spec: &VmSpec,
    resource: &OwnedResource,
    dir: &Path,
) -> Value {
    json!({"resource":resource, "config":dir.join("config"), "name":name(identity,spec),
        "notes":format!("ilex-gen2:{}/{}", spec.owner,spec.run)})
}

fn request(_state: &Path, dir: &Path, operation: &str, body: Value) -> Result<Value> {
    request_inner(dir, operation, body, false)
}

fn request_inner(dir: &Path, operation: &str, mut body: Value, redact: bool) -> Result<Value> {
    let script = dir.join(format!("{}-controller.ps1", random_id()?));
    let fields = body.as_object_mut().unwrap();
    fields.insert("operation".into(), json!(operation));
    fields.insert("schema".into(), json!(SCHEMA_VERSION));
    let result = (|| -> Result<Value> {
        File::create_new(&script)?.write_all(include_bytes!("../assets/gen2.ps1"))?;
        let mut command = host::powershell_script(&script);
        let output = || {
            if redact {
                Stdio::null()
            } else {
                Stdio::piped()
            }
        };
        let (_job, mut child) = host::spawn_owned(
            command
                .stdin(Stdio::piped())
                .stdout(output())
                .stderr(output()),
        )?;
        // This anonymous pipe is the only transport for request data. In
        // particular, key events never enter argv, a file, or a PS command.
        let sent = serde_json::to_writer(
            child.stdin.take().context("opening Gen2 request pipe")?,
            &body,
        );
        let output = child.wait_with_output()?;
        if redact {
            ensure!(
                sent.is_ok() && output.status.success(),
                "Gen2 sensitive keyboard input failed"
            );
            return Ok(json!({}));
        }
        ensure!(
            output.status.success(),
            "Gen2 {operation}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        sent?;
        serde_json::from_slice(&output.stdout)
            .with_context(|| format!("decoding Gen2 {operation} reply"))
    })();
    let _ = fs::remove_file(script);
    if redact {
        result.map_err(|_| anyhow::anyhow!("Gen2 sensitive keyboard input failed"))
    } else {
        result
    }
}

fn regular_file(path: &Path) -> Result<PathBuf> {
    let path = std::path::absolute(path)?;
    let dir = Directory::open(path.parent().context("artifact has no parent")?)?;
    let leaf = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid artifact filename")?;
    let _held = dir.read(leaf)?;
    Ok(path)
}

fn bound_file(path: &Path) -> Result<crate::received::BoundFile> {
    let dir = Directory::open(path.parent().context("artifact has no parent")?)?;
    dir.read(
        path.file_name()
            .and_then(|name| name.to_str())
            .context("invalid artifact filename")?,
    )
}

fn remove_tree(root: &Path, dir: &Path) -> Result<()> {
    ensure!(
        dir.parent() == Some(root),
        "Gen2 teardown path escaped its parent"
    );
    drop(Directory::open(dir)?);
    fs::remove_dir_all(dir).with_context(|| format!("removing {}", dir.display()))
}

fn prepare_seed(
    dir: &Path,
    alias: &str,
    ssh: &SshProvisioning,
    recovery_digest: Option<&str>,
) -> Result<PathBuf> {
    let seed = dir.join("seed");
    fs::create_dir(&seed)?;
    for key in ["access", "host"] {
        let mut command = Command::new("ssh-keygen.exe");
        use std::os::windows::process::CommandExt;
        host::command_ok(
            command
                .args(["-q", "-t", "ed25519"])
                .raw_arg("-N \"\"")
                .arg("-f")
                .arg(dir.join(key)),
        )?;
    }
    let access = fs::read_to_string(dir.join("access.pub"))?;
    ensure!(
        ssh.public_key.is_empty() || ssh.public_key.trim() == access.trim(),
        "unexpected Gen2 SSH public key request"
    );
    let identity = fs::read_to_string(dir.join("host.pub"))?;
    fs::write(
        dir.join("known_hosts"),
        format!("{alias} {}\n", identity.trim()),
    )?;
    fs::copy(dir.join("host"), seed.join("host.key"))?;
    fs::copy(dir.join("access.pub"), seed.join("access.pub"))?;
    if let Some(digest) = recovery_digest {
        fs::write(seed.join("recovery.sha256"), digest)?;
    }
    Ok(dir.join("seed.iso"))
}

struct Slot {
    _guard: File,
    receipt: PathBuf,
}
impl Slot {
    fn release(&self, resource: &OwnedResource) -> Result<()> {
        ensure!(
            same_allocation(&self.receipt, resource)?,
            "Gen2 slot receipt changed"
        );
        fs::remove_file(&self.receipt)?;
        Ok(())
    }
}

fn slot_receipt(root: &Path, index: usize) -> PathBuf {
    root.join(format!("slot-{index}.gen2.json"))
}
pub(crate) fn slot_reserved(root: &Path, index: usize) -> Result<bool> {
    Ok(slot_receipt(root, index).try_exists()?)
}
fn same_allocation(receipt: &Path, expected: &OwnedResource) -> Result<bool> {
    let resource: OwnedResource = serde_json::from_slice(&fs::read(receipt)?)?;
    check_version(resource.schema)?;
    Ok(resource.provider == PROVIDER
        && resource.owner == expected.owner
        && resource.run == expected.run
        && resource.opaque_id == expected.opaque_id)
}
fn reconcile_slots(resource: &OwnedResource) -> Result<()> {
    let root = host::root()?.join("admission");
    for i in 0..32 {
        let receipt = slot_receipt(&root, i);
        if receipt.try_exists()? && same_allocation(&receipt, resource)? {
            let lock = host::lock(&root.join(format!("slot-{i}.lock")))?;
            Slot {
                _guard: lock,
                receipt,
            }
            .release(resource)?;
        }
    }
    Ok(())
}

fn admit(memory: u64, resource: &OwnedResource) -> Result<(Slot, File)> {
    let root = host::root()?.join("admission");
    host::secure_dir(&root)?;
    let slots = qemu::max_slots()?;
    let (lock, index) = wait(WAIT, || {
        for i in 0..slots {
            if let Some(file) = host::try_lock(&root.join(format!("slot-{i}.lock")))?
                && !slot_reserved(&root, i)?
            {
                return Ok((file, i));
            }
        }
        bail!("all {slots} native VM slots are occupied")
    })?;
    let guard = wait(WAIT, || {
        if let Some(lock) = host::try_lock(&root.join("host.lock"))? {
            ensure!(
                host::available_memory()? >= memory + 14 * GIB,
                "waiting for Gen2 memory plus 14 GiB host reserve"
            );
            return Ok(lock);
        }
        bail!("native host admission is busy")
    })?;
    let receipt = slot_receipt(&root, index);
    host::atomic(&receipt, &serde_json::to_vec(resource)?)?;
    Ok((
        Slot {
            _guard: lock,
            receipt,
        },
        guard,
    ))
}

pub fn wait<T>(timeout: Duration, mut poll: impl FnMut() -> Result<T>) -> Result<T> {
    let start = Instant::now();
    loop {
        ensure!(!transport::cancelled(), "Gen2 operation cancelled");
        match poll() {
            Ok(value) => return Ok(value),
            Err(e) if start.elapsed() >= timeout => {
                return Err(e.context(format!("Gen2 deadline {timeout:?} exceeded")));
            }
            Err(_) => thread::sleep(Duration::from_millis(850)),
        }
    }
}

fn validate_guest_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/') && !path.contains(['\r', '\n', '\0']),
        "expected an absolute guest path"
    );
    Ok(())
}

fn shell_command(argv: &[String]) -> Result<String> {
    ensure!(
        !argv.is_empty() && argv.iter().all(|s| !s.contains('\0')),
        "invalid Gen2 guest argv"
    );
    Ok(argv
        .iter()
        .map(|s| format!("'{}'", s.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" "))
}

fn power_transition<T>(requested: Result<Vec<u8>>, observed: Result<T>) -> Result<T> {
    // sshd may be stopped before it can acknowledge a successful power request.
    // The observed VM state or changed authenticated boot ID is authoritative.
    observed.map_err(|error| match requested {
        Ok(_) => error,
        Err(request) => error.context(format!("power request also failed: {request:#}")),
    })
}

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_millis()
        .try_into()?)
}
pub fn random_id() -> Result<String> {
    let id = unsafe { windows::Win32::System::Com::CoCreateGuid()? };
    Ok(format!("{id:?}")
        .trim_matches(['{', '}'])
        .to_ascii_lowercase())
}
fn check_run(id: &str) -> Result<()> {
    ensure!(
        id.len() == 36
            && id.bytes().enumerate().all(|(i, b)| {
                if [8, 13, 18, 23].contains(&i) {
                    b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            }),
        "expected UUID for Gen2 identity"
    );
    Ok(())
}

fn scan_to_vk(code: u32) -> Result<u32> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VSC_TO_VK_EX, MapVirtualKeyW};
    ensure!(u16::try_from(code).is_ok(), "invalid Gen2 set-1 scancode");
    let vk = unsafe { MapVirtualKeyW(code, MAPVK_VSC_TO_VK_EX) };
    ensure!(vk != 0, "unsupported Gen2 set-1 scancode {code:#x}");
    Ok(vk)
}

fn ascii_vk(byte: u8) -> Result<(u32, bool)> {
    Ok(match byte {
        b'a'..=b'z' => (u32::from(byte.to_ascii_uppercase()), false),
        b'A'..=b'Z' => (byte.into(), true),
        b'0'..=b'9' | b' ' => (byte.into(), false),
        b'\n' => (0x0d, false),
        _ => {
            let normal = b"`-=[]\\;',./";
            let shifted = b"~_+{}|:\"<>?";
            let keys = [
                0xc0, 0xbd, 0xbb, 0xdb, 0xdd, 0xdc, 0xba, 0xde, 0xbc, 0xbe, 0xbf,
            ];
            if let Some(i) = normal.iter().position(|&b| b == byte) {
                (keys[i], false)
            } else if let Some(i) = shifted.iter().position(|&b| b == byte) {
                (keys[i], true)
            } else if let Some(i) = b")!@#$%^&*(".iter().position(|&b| b == byte) {
                (u32::from(b'0') + i as u32, true)
            } else {
                bail!("unsupported US-ASCII keyboard character")
            }
        }
    })
}

/// Convert a verified installed qcow2 into a standalone parent, retaining the
/// remote immutable source. Hyper-V receives a new writable child per fixture.
pub fn vhdx(source: &Path, destination: &Path, format: &str) -> Result<()> {
    convert(source, destination, format, None)
}

/// Inspect the raw header before starting QEMU, so even its read-only info
/// command cannot resolve a hostile external file. Validate extension bounds
/// within the first cluster and reject backing storage and external data.
fn verify_qcow2_header(file: &mut File) -> Result<u64> {
    let mut header = [0; 104];
    file.read_exact(&mut header)?;
    let field = |offset| u32::from_be_bytes(header[offset..offset + 4].try_into().unwrap());
    let wide = |offset| u64::from_be_bytes(header[offset..offset + 8].try_into().unwrap());
    ensure!(
        &header[..4] == b"QFI\xfb" && matches!(field(4), 2 | 3),
        "expected qcow2 v2/v3 header"
    );
    ensure!(wide(8) == 0 && field(16) == 0, "qcow2 has backing storage");
    let cluster_bits = field(20);
    ensure!(
        (9..=21).contains(&cluster_bits) && wide(24) > 0,
        "invalid qcow2 cluster or virtual size"
    );
    let limit = 1u64 << cluster_bits;
    let start = if field(4) == 2 {
        72
    } else {
        ensure!(wide(72) & 4 == 0, "qcow2 has an external data file");
        u64::from(field(100))
    };
    ensure!(
        start % 8 == 0 && start >= if field(4) == 2 { 72 } else { 104 } && start <= limit - 8,
        "invalid qcow2 extension offset"
    );
    let mut offset = start;
    loop {
        file.seek(SeekFrom::Start(offset))?;
        let mut entry = [0; 8];
        file.read_exact(&mut entry)?;
        let (kind, len) = (
            u32::from_be_bytes(entry[..4].try_into().unwrap()),
            u32::from_be_bytes(entry[4..].try_into().unwrap()),
        );
        if kind == 0 {
            ensure!(len == 0, "invalid qcow2 extension terminator");
            break;
        }
        ensure!(
            kind != 0x4441_5441 && kind != 0xe279_2aca,
            "qcow2 has external storage metadata"
        );
        offset += 8 + u64::from(len).next_multiple_of(8);
        ensure!(offset <= limit - 8, "invalid qcow2 extension size");
    }
    Ok(wide(24))
}

fn convert(source: &Path, destination: &Path, format: &str, checksum: Option<&str>) -> Result<()> {
    ensure!(
        matches!(format, "raw" | "qcow2"),
        "invalid Gen2 conversion format"
    );
    let source = std::path::absolute(source)?;
    let mut original = bound_file(&source)?;
    let work = destination
        .parent()
        .context("Gen2 conversion has no output directory")?;
    let bound = Directory::open(work)?;
    let target = destination
        .file_name()
        .context("Gen2 conversion has no output filename")?;
    let runtime = runtime::fetch()?;
    let staged = work.join(format!("convert-{}.{}", &random_id()?[..12], format));
    let input = if source.parent() == Some(work) {
        &source
    } else {
        &staged
    };
    let conversion = (|| -> Result<()> {
        let staged_hash = if input == &staged {
            ensure!(
                transport::available_space(work)? >= original.file.metadata()?.len() * 2 + 30 * GIB,
                "Gen2 conversion needs space for staging and its VHDX plus the 30 GiB reserve"
            );
            let expected = if let Some(hash) = checksum {
                hash.into()
            } else {
                let hash = host::hash_reader(&mut original.file)?;
                original.file.rewind()?;
                hash
            };
            let mut output = File::create_new(&staged)?;
            std::io::copy(&mut original.file, &mut output)?;
            Some(expected)
        } else {
            None
        };
        let leaf = input
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid Gen2 input filename")?;
        let mut input_guard = bound.read(leaf)?;
        if let Some(expected) = staged_hash {
            ensure!(
                host::hash_reader(&mut input_guard.file)? == expected,
                "Gen2 staged input checksum mismatch"
            );
            input_guard.file.rewind()?;
        }
        if format == "qcow2" {
            let virtual_bytes = verify_qcow2_header(&mut input_guard.file)?;
            let info = host::output_owned(
                runtime::command(&runtime, "qemu-img.exe")
                    .current_dir(work)
                    .args(["info", "-f", "qcow2", "--output=json", leaf]),
                Stdio::piped(),
            )?;
            ensure!(info.status.success(), "inspecting standalone qcow2 failed");
            let info: Value = serde_json::from_slice(&info.stdout)?;
            ensure!(
                info["format"] == "qcow2"
                    && info["virtual-size"] == virtual_bytes
                    && info["backing-filename"].is_null()
                    && info["full-backing-filename"].is_null()
                    && info["format-specific"]["data"]["data-file"].is_null(),
                "qcow2 inspection rejected external storage or inconsistent format/size"
            );
        }
        host::command_ok(
            runtime::command(&runtime, "qemu-img.exe")
                .current_dir(work)
                .args(["convert", "-S", "4096", "-f", format, "-O", "vhdx"])
                .arg(leaf)
                .arg(target),
        )
    })();
    if staged.try_exists()? {
        fs::remove_file(&staged)?;
    }
    conversion?;
    host::command_ok(
        Command::new("fsutil.exe")
            .args(["sparse", "setflag"])
            .arg(destination)
            .arg("0"),
    )?;
    use std::os::windows::fs::MetadataExt;
    ensure!(
        fs::metadata(destination)?.file_attributes() & (0x200 | 0x800 | 0x4000) == 0,
        "Gen2 parent cannot be sparse, compressed or EFS-encrypted"
    );
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    Ok(format!("sha256:{}", host::hash_file(path)?))
}
