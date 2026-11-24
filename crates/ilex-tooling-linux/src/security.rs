//! Linux security provisioning. Platform availability is not guest qualification:
//! consumers must also prove enforcement, replay PCRs and exercise persistence.
use crate::{Workspace, cmd};
use anyhow::{Context, Result, bail, ensure};
use ilex_tooling_core::{Requirement, SCHEMA_VERSION, TpmLifecycle, VmSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Debug, Serialize)]
pub struct HostProbe {
    pub versions: std::collections::BTreeMap<String, String>,
    pub kvm_smm: Option<i32>,
    pub kvm_error: Option<String>,
    pub firmware_descriptor: Option<PathBuf>,
    pub firmware_error: Option<String>,
    pub swtpm: bool,
    pub swtpm_setup: bool,
    /// A probe never claims that a guest has passed security qualification.
    pub qualification: &'static str,
}

pub fn host_probe() -> HostProbe {
    let kvm = kvm_smm();
    let firmware = firmware();
    HostProbe {
        versions: [
            ("kernel", "uname", "-r"),
            ("qemu", "qemu-system-x86_64", "--version"),
            ("swtpm", "swtpm", "--version"),
            ("swtpm_setup", "swtpm_setup", "--version"),
        ]
        .into_iter()
        .map(|(name, program, arg)| {
            (
                name.into(),
                cmd::probe(Command::new(program).arg(arg)).unwrap_or_else(|| "unavailable".into()),
            )
        })
        .collect(),
        kvm_smm: kvm.as_ref().ok().copied(),
        kvm_error: kvm.err().map(|e| format!("{e:#}")),
        firmware_descriptor: firmware.as_ref().ok().map(|f| f.descriptor.clone()),
        firmware_error: firmware.err().map(|e| format!("{e:#}")),
        swtpm: cmd::require("swtpm").is_ok(),
        swtpm_setup: cmd::require("swtpm_setup").is_ok(),
        qualification: "not_yet_qualified; run security::firmware",
    }
}

/// KVM_CHECK_EXTENSION(KVM_CAP_X86_SMM), on this host, every invocation.
#[cfg(target_os = "linux")]
pub fn kvm_smm() -> Result<i32> {
    use std::os::{
        fd::AsRawFd,
        raw::{c_int, c_ulong},
    };
    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
    let file = OpenOptions::new().read(true).write(true).open("/dev/kvm")?;
    // SAFETY: KVM_CHECK_EXTENSION takes an integer, not a userspace pointer.
    let result = unsafe { ioctl(file.as_raw_fd(), 0xae03, 117 as c_ulong) };
    ensure!(
        result >= 0,
        "KVM_CHECK_EXTENSION: {}",
        std::io::Error::last_os_error()
    );
    Ok(result)
}
#[cfg(not(target_os = "linux"))]
pub fn kvm_smm() -> Result<i32> {
    bail!("KVM/SMM probe requires Linux")
}

pub fn requested(spec: &VmSpec) -> bool {
    spec.secure_boot == Requirement::Required
        || !matches!(spec.tpm, TpmLifecycle::Off)
        || spec.security_launch == ilex_tooling_core::SecurityLaunch::QualificationFixture
}

pub fn validate(spec: &VmSpec) -> Result<()> {
    crate::require_linux()?;
    ensure!(
        kvm_smm()? > 0,
        "KVM_CAP_X86_SMM is unavailable; secure guests require KVM with SMM"
    );
    firmware()?;
    cmd::require("qemu-img")?;
    if !matches!(spec.tpm, TpmLifecycle::Off) {
        cmd::require("swtpm")?;
        cmd::require("swtpm_setup")?;
    }
    ensure!(
        spec.firmware_state.is_none(),
        "security VMs own firmware_state inside the TPM/vars lifecycle bundle"
    );
    if let TpmLifecycle::Persistent { bundle_id } = &spec.tpm {
        valid_id(bundle_id)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct FirmwarePair {
    descriptor: PathBuf,
    code: PathBuf,
    vars: PathBuf,
    code_format: String,
    vars_format: String,
}

fn image_size(path: &Path, format: &str) -> Result<u64> {
    let data = cmd::output(
        Command::new("qemu-img")
            .args(["info", "--output=json", "-f", format])
            .arg(path),
    )?;
    serde_json::from_str::<serde_json::Value>(&data)?["virtual-size"]
        .as_u64()
        .context("missing firmware virtual size")
}

/// Use the distro's declared matching pair, enrolled keys and SMM requirement.
/// Reject the older 2M layout, blank templates and stateless/confidential builds.
fn firmware() -> Result<FirmwarePair> {
    let mut descriptors: Vec<_> = fs::read_dir("/usr/share/qemu/firmware")?
        .flatten()
        .map(|e| e.path())
        .collect();
    descriptors.sort();
    for descriptor in descriptors {
        let Ok(data) = fs::read(&descriptor) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data) else {
            continue;
        };
        let has = |feature: &str| {
            value["features"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == feature))
        };
        let description = value["description"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !(has("secure-boot")
            && has("enrolled-keys")
            && has("requires-smm")
            && (description.contains("ms cert") || description.contains("microsoft"))
            && value["mapping"]["device"] == "flash"
            && value["targets"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t["architecture"] == "x86_64")))
        {
            continue;
        }
        let mapping = &value["mapping"];
        let Some(code) = mapping["executable"]["filename"].as_str() else {
            continue;
        };
        let Some(vars) = mapping["nvram-template"]["filename"].as_str() else {
            continue;
        };
        let pair = FirmwarePair {
            descriptor,
            code: code.into(),
            vars: vars.into(),
            code_format: mapping["executable"]["format"]
                .as_str()
                .unwrap_or("raw")
                .into(),
            vars_format: mapping["nvram-template"]["format"]
                .as_str()
                .unwrap_or("raw")
                .into(),
        };
        if ![pair.code_format.as_str(), pair.vars_format.as_str()]
            .iter()
            .all(|f| matches!(*f, "raw" | "qcow2"))
        {
            continue;
        }
        if pair.code.is_file()
            && pair.vars.is_file()
            && image_size(&pair.code, &pair.code_format)?
                + image_size(&pair.vars, &pair.vars_format)?
                == 4 * 1024 * 1024
        {
            return Ok(pair);
        }
    }
    bail!(
        "no matching Microsoft-enrolled 4M OVMF CODE/VARS descriptor with secure-boot/requires-smm; install distro ovmf/edk2-ovmf (ordinary ILEX_OVMF overrides do not qualify security)"
    )
}

fn valid_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 100
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid security bundle id"
    );
    Ok(())
}
pub fn bundle_path(ws: &Workspace, spec: &VmSpec) -> Result<PathBuf> {
    let id = match &spec.tpm {
        TpmLifecycle::Persistent { bundle_id } => bundle_id,
        TpmLifecycle::Off => &spec.run,
    };
    valid_id(id)?;
    Ok(ws.state_dir().join("security").join(id))
}

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    schema: u32,
    owner: String,
    run: String,
    purpose: String,
    disk: PathBuf,
    secure_boot: Requirement,
    tpm: TpmLifecycle,
    descriptor: PathBuf,
    code_sha256: String,
    #[serde(default)]
    firmware_identity: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    vars_template_sha256: String,
    #[serde(default)]
    vars_initial_sha256: String,
    #[serde(default)]
    sb_disabled_byte: Option<usize>,
}

pub(crate) fn firmware_identity(
    ws: &Workspace,
    spec: &VmSpec,
) -> Result<std::collections::BTreeMap<String, String>> {
    let root = bundle_path(ws, spec)?;
    let mut identity = if root.join("bundle.json").exists() {
        let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("bundle.json"))?)?;
        manifest.authorize(ws, spec)?;
        ensure!(
            manifest.code_sha256 == hash(&fs::read(root.join("CODE.fd"))?),
            "bundle CODE changed"
        );
        ensure!(
            !manifest.firmware_identity.is_empty(),
            "old bundle has no qualification identity; recreate this disposable fixture"
        );
        manifest.firmware_identity
    } else {
        let pair = firmware()?;
        std::collections::BTreeMap::from([
            (
                "descriptor_sha256".into(),
                hash(&fs::read(&pair.descriptor)?),
            ),
            ("code_source_sha256".into(), hash(&fs::read(&pair.code)?)),
            (
                "enrolled_vars_source_sha256".into(),
                hash(&fs::read(&pair.vars)?),
            ),
            ("pcr_banks".into(), "sha256".into()),
            (
                "off_control".into(),
                "same-code-enrolled-vars-SecureBootEnable-only-v1".into(),
            ),
        ])
    };
    identity.insert(
        "secure_boot".into(),
        if spec.secure_boot == Requirement::Required {
            "required"
        } else {
            "off"
        }
        .into(),
    );
    identity.insert(
        "tpm".into(),
        if matches!(spec.tpm, TpmLifecycle::Off) {
            "off"
        } else {
            "persistent"
        }
        .into(),
    );
    Ok(identity)
}

/// Fresh enrolled x64 EDK2 authenticated variable stores only. Keep every
/// certificate and other byte intact; flip the single NV/BS enable boolean.
/// Layout: edk2 MdeModulePkg/Include/Guid/VariableFormat.h (60-byte header,
/// x64 name/data alignment=1, VAR_ADDED=0x3f).
fn disable_secure_boot(path: &Path) -> Result<usize> {
    let mut bytes = fs::read(path)?;
    let name: Vec<u8> = "SecureBootEnable\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let guid = [
        0xc7, 0x0b, 0xa3, 0xf0, 0x08, 0xaf, 0x56, 0x45, 0x99, 0xc4, 0x00, 0x10, 0x09, 0xc9, 0x3a,
        0x44,
    ];
    let store_guid = [
        0x78, 0x2c, 0xf3, 0xaa, 0x7b, 0x94, 0x9a, 0x43, 0xa1, 0x80, 0x2e, 0x14, 0x4e, 0xc3, 0x77,
        0x92,
    ];
    let stores: Vec<_> = bytes
        .windows(16)
        .enumerate()
        .filter_map(|(at, value)| {
            (value == store_guid && bytes.get(at + 20..at + 22) == Some(&[0x5a, 0xfe]))
                .then_some(at)
        })
        .collect();
    ensure!(
        stores.len() == 1,
        "expected one healthy authenticated variable store"
    );
    let number = |b: &[u8], at: usize| -> Result<usize> {
        Ok(u32::from_le_bytes(
            b.get(at..at + 4)
                .context("truncated variable header")?
                .try_into()?,
        ) as usize)
    };
    let store = stores[0];
    let limit = store
        .checked_add(number(&bytes, store + 16)?)
        .context("variable store overflow")?;
    ensure!(limit <= bytes.len(), "invalid variable store size");
    let mut at = (store + 28 + 3) & !3;
    let mut candidates = Vec::new();
    while at + 60 <= limit && bytes[at..at + 2] != [0xff, 0xff] {
        ensure!(bytes[at..at + 2] == [0xaa, 0x55], "invalid variable entry");
        let name_size = number(&bytes, at + 36)?;
        let data_size = number(&bytes, at + 40)?;
        let data = (at + 60)
            .checked_add(name_size)
            .context("variable name overflow")?;
        let end = data
            .checked_add(data_size)
            .context("variable data overflow")?;
        ensure!(
            name_size.is_multiple_of(2) && end <= limit,
            "invalid variable size"
        );
        if bytes[at + 2] == 0x3f && bytes[at + 44..at + 60] == guid && bytes[at + 60..data] == name
        {
            ensure!(
                bytes[at + 4..at + 8] == 3u32.to_le_bytes() && data_size == 1 && bytes[data] == 1,
                "unexpected SecureBootEnable attributes/value"
            );
            candidates.push(data);
        }
        at = (end + 3) & !3;
    }
    ensure!(
        candidates.len() <= 1,
        "duplicate SecureBootEnable variables"
    );
    let data_offset = if let Some(offset) = candidates.first() {
        *offset
    } else {
        // Ubuntu's enrolled template omits this boolean (firmware defaults to
        // enabled with PK present). Append only that NV/BS variable; retain
        // PK/KEK/db/dbx and every existing variable byte unchanged.
        let size = (60 + name.len() + 1 + 3) & !3;
        ensure!(
            at + size <= limit && bytes[at..at + size].iter().all(|b| *b == 0xff),
            "no free space for SB-off boolean"
        );
        bytes[at..at + 60].fill(0);
        bytes[at..at + 4].copy_from_slice(&[0xaa, 0x55, 0x3f, 0]);
        bytes[at + 4..at + 8].copy_from_slice(&3u32.to_le_bytes());
        bytes[at + 36..at + 40].copy_from_slice(&(name.len() as u32).to_le_bytes());
        bytes[at + 40..at + 44].copy_from_slice(&1u32.to_le_bytes());
        bytes[at + 44..at + 60].copy_from_slice(&guid);
        bytes[at + 60..at + 60 + name.len()].copy_from_slice(&name);
        at + 60 + name.len()
    };
    bytes[data_offset] = 0;
    fs::write(path, bytes)?;
    Ok(data_offset)
}

/// Deliberate policy-mismatch negative on a stopped, owned disposable bundle.
/// This does not manufacture a TPM, change the manifest or confer qualification.
pub fn disable_fixture_secure_boot(ws: &Workspace, spec: &VmSpec) -> Result<()> {
    ensure!(
        spec.security_launch == ilex_tooling_core::SecurityLaunch::QualificationFixture
            && spec.secure_boot == Requirement::Required
            && matches!(spec.tpm, TpmLifecycle::Persistent { .. }),
        "explicit persistent-TPM negative fixture required"
    );
    let root = bundle_path(ws, spec)?;
    let lock = OpenOptions::new()
        .write(true)
        .open(root.join("bundle.lock"))?;
    lock.try_lock()
        .context("fixture must be stopped before changing firmware policy")?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("bundle.json"))?)?;
    manifest.authorize(ws, spec)?;
    ensure!(
        root.join("tpm/tpm2-00.permall").is_file(),
        "persistent fixture TPM missing"
    );
    disable_secure_boot(&root.join("VARS.fd"))?;
    Ok(())
}
impl Manifest {
    fn authorize(&self, ws: &Workspace, spec: &VmSpec) -> Result<()> {
        ilex_tooling_core::check_version(self.schema)?;
        ws.identity()?.authorize(&self.owner)?;
        ensure!(
            self.owner == spec.owner
                && self.run == spec.run
                && self.purpose == spec.purpose
                && self.secure_boot == spec.secure_boot
                && self.tpm == spec.tpm,
            "security bundle identity/policy differs; create a separate fixture for policy/TPM negatives"
        );
        let ilex_tooling_core::BootSource::Disk { path, .. } = &spec.boot else {
            bail!("security bundle requires disk")
        };
        ensure!(
            self.disk == fs::canonicalize(path)?,
            "security bundle disk differs; preserve disk, vars and TPM together"
        );
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct Bundle {
    pub code: PathBuf,
    pub vars: PathBuf,
    tpm: Option<Child>,
    // Held until QEMU and swtpm have both stopped.
    _lock: File,
}
impl Bundle {
    pub fn prepare(ws: &Workspace, spec: &VmSpec, runtime: &Path) -> Result<Self> {
        validate(spec)?;
        let root = bundle_path(ws, spec)?;
        fs::create_dir_all(&root)?;
        crate::provider::require_owned_path(&ws.state_dir(), &root)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        }
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("bundle.lock"))?;
        lock.try_lock()
            .context("security lifecycle bundle is in use")?;
        let code = root.join("CODE.fd");
        let vars = root.join("VARS.fd");
        let manifest_path = root.join("bundle.json");
        if manifest_path.exists() {
            let manifest: Manifest = serde_json::from_slice(&fs::read(&manifest_path)?)?;
            manifest.authorize(ws, spec)?;
            ensure!(
                vars.is_file() && code.is_file() && manifest.code_sha256 == hash(&fs::read(&code)?),
                "incomplete/modified firmware bundle; refusing to recreate its identity"
            );
            if !matches!(spec.tpm, TpmLifecycle::Off) {
                ensure!(
                    root.join("tpm/tpm2-00.permall").is_file(),
                    "TPM state is missing; refusing to manufacture a replacement"
                );
            }
        } else {
            // An interrupted manufacture is never silently promoted to a fresh TPM.
            ensure!(
                !code.exists() && !vars.exists() && !root.join("tpm").exists(),
                "incomplete security bundle; destroy this disposable fixture explicitly"
            );
            let identity = firmware_identity(ws, spec)?;
            let pair = firmware()?;
            convert(&pair.code, &pair.code_format, &code)?;
            convert(&pair.vars, &pair.vars_format, &vars)?;
            let vars_template_sha256 = hash(&fs::read(&vars)?);
            let sb_disabled_byte = if spec.secure_boot == Requirement::Off {
                Some(disable_secure_boot(&vars)?)
            } else {
                None
            };
            if !matches!(spec.tpm, TpmLifecycle::Off) {
                fs::create_dir(root.join("tpm"))?;
                let setup_log = File::create(runtime.join("swtpm-setup.log"))?;
                cmd::run(
                    Command::new("swtpm_setup")
                        .args(["--tpm2", "--tpmstate"])
                        .arg(root.join("tpm"))
                        .args(["--pcr-banks", "sha256", "--createek", "--create-spk"])
                        .stdin(Stdio::null())
                        .stdout(setup_log.try_clone()?)
                        .stderr(setup_log),
                )
                .with_context(|| {
                    format!(
                        "manufacturing TPM; see {}",
                        runtime.join("swtpm-setup.log").display()
                    )
                })?;
            }
            let ilex_tooling_core::BootSource::Disk { path, .. } = &spec.boot else {
                bail!("security bundle requires disk")
            };
            let manifest = Manifest {
                schema: SCHEMA_VERSION,
                owner: spec.owner.clone(),
                run: spec.run.clone(),
                purpose: spec.purpose.clone(),
                disk: fs::canonicalize(path)?,
                secure_boot: spec.secure_boot,
                tpm: spec.tpm.clone(),
                descriptor: pair.descriptor,
                code_sha256: hash(&fs::read(&code)?),
                firmware_identity: identity,
                vars_template_sha256,
                vars_initial_sha256: hash(&fs::read(&vars)?),
                sb_disabled_byte,
            };
            fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        }
        let mut bundle = Self {
            code,
            vars,
            tpm: None,
            _lock: lock,
        };
        if !matches!(spec.tpm, TpmLifecycle::Off) {
            let socket = runtime.join("tpm.sock");
            ensure!(
                socket.as_os_str().len() < 108,
                "swtpm socket path exceeds Unix limit"
            );
            let log = File::create(runtime.join("swtpm.log"))?;
            let mut emulator = Command::new("swtpm");
            #[cfg(target_os = "linux")]
            {
                use std::os::unix::process::CommandExt;
                unsafe extern "C" {
                    fn prctl(option: i32, ...) -> i32;
                    fn getppid() -> i32;
                }
                let parent = std::process::id() as i32;
                // SAFETY: only async-signal-safe syscalls between fork/exec.
                unsafe {
                    emulator.pre_exec(move || {
                        if prctl(1, 15u64, 0u64, 0u64, 0u64) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                        if getppid() != parent {
                            return Err(std::io::Error::from_raw_os_error(4));
                        }
                        Ok(())
                    });
                }
            }
            bundle.tpm = Some(
                emulator
                    .args(["socket", "--tpm2", "--tpmstate"])
                    .arg(format!("dir={},mode=0600,lock", root.join("tpm").display()))
                    .arg("--ctrl")
                    .arg(format!(
                        "type=unixio,path={},mode=0600,terminate",
                        socket.display()
                    ))
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .spawn()
                    .with_context(|| {
                        format!(
                            "starting swtpm; see {}",
                            runtime.join("swtpm.log").display()
                        )
                    })?,
            );
            let start = Instant::now();
            while !socket.exists() {
                if let Some(status) = bundle.tpm.as_mut().unwrap().try_wait()? {
                    bail!(
                        "swtpm exited ({status}); see {}",
                        runtime.join("swtpm.log").display()
                    );
                }
                ensure!(
                    start.elapsed() < Duration::from_secs(10),
                    "swtpm socket deadline exceeded"
                );
                thread::sleep(Duration::from_millis(25));
            }
        }
        Ok(bundle)
    }
}
impl Drop for Bundle {
    fn drop(&mut self) {
        if let Some(child) = &mut self.tpm {
            // --ctrl terminate exits when QEMU disconnects and persists state.
            let deadline = Instant::now() + Duration::from_secs(3);
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(25));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn convert(source: &Path, format: &str, target: &Path) -> Result<()> {
    cmd::run(
        Command::new("qemu-img")
            .args(["convert", "-f", format, "-O", "raw"])
            .arg(source)
            .arg(target),
    )
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn destroy_bundle(ws: &Workspace, spec: &VmSpec) -> Result<()> {
    ws.identity()?.authorize(&spec.owner)?;
    let root = bundle_path(ws, spec)?;
    if !root.exists() {
        return Ok(());
    }
    crate::provider::require_owned_path(&ws.state_dir(), &root)?;
    let manifest: Manifest = serde_json::from_slice(&fs::read(root.join("bundle.json"))?)?;
    manifest.authorize(ws, spec)?;
    let lock = OpenOptions::new()
        .write(true)
        .open(root.join("bundle.lock"))?;
    lock.try_lock()
        .context("cannot destroy a live security bundle")?;
    fs::remove_dir_all(root)?;
    Ok(())
}

pub use ilex_enroll::pcr::{Pcr7Replay, replay_pcr7};
