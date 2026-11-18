//! Native nonsecure WHPX development VM. No Linux/WSL execution or viewer.
use crate::{host, qmp::Qmp, runtime, transport};
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use ilex_tooling_core::{
    Architecture, BootPhase, Capability, CapabilityStatus, CheckoutIdentity, VmCapabilities,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::{self, File},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use windows::Win32::{
    Foundation::{CloseHandle, FILETIME},
    System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    },
};

pub fn process_start(pid: u32) -> Result<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)?;
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let result = GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user);
        let mut status = 0;
        let live = GetExitCodeProcess(handle, &mut status);
        let _ = CloseHandle(handle);
        result?;
        live?;
        ensure!(status == 259, "QEMU process has exited");
        Ok(format!(
            "{}",
            ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64
        ))
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmInfo {
    pub schema: u32,
    pub owner: String,
    pub run: String,
    pub architecture: Architecture,
    pub process_start: String,
    pub name: String,
    pub purpose: String,
    pub pid: u32,
    pub hwnd: isize,
    pub ssh_port: u16,
    pub ssh_key: PathBuf,
    pub known_hosts: PathBuf,
    pub host_key_alias: String,
    pub qmp_endpoint: String,
    pub input_endpoint: String,
    pub width: u32,
    pub height: u32,
    pub display: String,
    pub gpu: bool,
    pub clipboard: bool,
    pub runtime: PathBuf,
    pub disk: PathBuf,
    #[serde(skip)]
    pub dir: PathBuf,
}
impl VmInfo {
    pub fn read(dir: &Path) -> Result<Self> {
        let mut info: Self = serde_json::from_slice(&fs::read(dir.join("vm.json"))?)?;
        ilex_tooling_core::check_version(info.schema)?;
        info.dir = dir.into();
        Ok(info)
    }
    pub fn is_running(&self) -> bool {
        process_start(self.pid).is_ok_and(|value| value == self.process_start)
    }
    pub fn validate(&self) -> Result<()> {
        ilex_tooling_core::check_version(self.schema)?;
        ensure!(
            self.is_running(),
            "VM process identity changed; attach again"
        );
        if self.hwnd != 0 {
            crate::capture::validate_window(self)?;
        }
        Ok(())
    }
    pub fn ssh(&self) -> Command {
        self.ssh_session(false)
    }
    pub fn interactive_ssh(&self) -> Command {
        self.ssh_session(true)
    }
    fn ssh_session(&self, tty: bool) -> Command {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("ssh.exe");
        if !tty {
            command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        command
            .args([
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
                "LogLevel=ERROR",
                "-o",
                "ConnectTimeout=3",
                "-o",
                "ServerAliveInterval=3",
                "-o",
                "ServerAliveCountMax=2",
            ])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .arg("-o")
            .arg(format!("HostKeyAlias={}", self.host_key_alias))
            .arg("-p")
            .arg(self.ssh_port.to_string())
            .arg("-i")
            .arg(&self.ssh_key);
        if tty {
            command.arg("-t");
        }
        command.arg("root@127.0.0.1");
        command
    }
    pub fn ssh_command_line(&self) -> String {
        format!(
            "ssh -p {} -i \"{}\" -o StrictHostKeyChecking=yes -o UserKnownHostsFile=\"{}\" -o HostKeyAlias={} root@127.0.0.1",
            self.ssh_port,
            self.ssh_key.display(),
            self.known_hosts.display(),
            self.host_key_alias
        )
    }
    pub fn scp(&self) -> Command {
        let mut command = Command::new("scp.exe");
        command
            .args([
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
                "LogLevel=ERROR",
                "-o",
                "ConnectTimeout=3",
            ])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()))
            .arg("-o")
            .arg(format!("HostKeyAlias={}", self.host_key_alias))
            .arg("-P")
            .arg(self.ssh_port.to_string())
            .arg("-i")
            .arg(&self.ssh_key);
        command
    }
    pub fn capabilities(&self) -> VmCapabilities {
        use Capability::*;
        use CapabilityStatus::*;
        let mut features = BTreeMap::new();
        for feature in [
            PrebootKeyboard,
            SessionKeyboard,
            SessionPointer,
            Screenshot,
            Serial,
            Start,
            Stop,
            Destroy,
        ] {
            features.insert(feature, Available);
        }
        features.insert(
            GpuRendering,
            if self.gpu {
                Available
            } else {
                Unavailable("explicit --no-gl/headless software display".into())
            },
        );
        let session_active = self.clipboard
            && fs::read(self.dir.join("session-status.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|state| state["active"] == true);
        features.insert(
            Clipboard,
            if session_active {
                Available
            } else {
                Unavailable("no active unlocked development session".into())
            },
        );
        if self.hwnd != 0
            && let Err(error) = crate::capture::geometry(self)
        {
            features.insert(Screenshot, Unavailable(format!("{error:#}")));
        }
        features.insert(
            PrebootPointer,
            NotYetQualified("use firmware keyboard navigation".into()),
        );
        for feature in [FirmwareEnforcement, PersistentTpm, Suspend] {
            features.insert(
                feature,
                Unavailable(
                    "WHPX development profile is nonsecure; Gen2 security is a separate provider"
                        .into(),
                ),
            );
        }
        VmCapabilities {
            schema: 1,
            provider: "windows-qemu-whpx".into(),
            vm_id: Some(format!("{}:{}", self.pid, self.process_start)),
            phase: BootPhase::Unknown,
            features,
            screenshot_source: Some(
                if self.hwnd != 0 {
                    "windows-graphics-capture-client"
                } else {
                    "qmp-screendump"
                }
                .into(),
            ),
            screenshot_dimensions: None,
            text_layout: Some(
                "US physical keyboard; ASCII text only (Unicode via dev clipboard)".into(),
            ),
        }
    }
}
pub fn running_vms(root: &Path, owner: Option<&str>) -> Vec<VmInfo> {
    let mut vms: Vec<_> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| VmInfo::read(&entry.path()).ok())
        .filter(|info| owner.is_none_or(|o| o == info.owner) && info.is_running())
        .collect();
    vms.sort_by(|a, b| a.purpose.cmp(&b.purpose));
    vms
}

/// Capability diagnostics must not report Linux/WSL as the Windows provider,
/// or claim session/capture availability before an owned launch exists.
pub fn idle_capabilities() -> VmCapabilities {
    use Capability::*;
    use CapabilityStatus::*;
    let mut features = BTreeMap::new();
    for feature in [
        Start,
        Stop,
        Destroy,
        PrebootKeyboard,
        SessionKeyboard,
        SessionPointer,
        Serial,
        Screenshot,
        GpuRendering,
        Clipboard,
    ] {
        features.insert(feature, NotYetQualified("no owned native VM running; launch cargo xtask vm --gui, then inspect its capabilities".into()));
    }
    features.insert(
        PrebootPointer,
        NotYetQualified("use firmware keyboard navigation".into()),
    );
    for feature in [FirmwareEnforcement, PersistentTpm, Suspend] {
        features.insert(
            feature,
            Unavailable(
                "WHPX development profile is nonsecure; Gen2 security is a separate provider"
                    .into(),
            ),
        );
    }
    VmCapabilities {
        schema: 1,
        provider: "windows-qemu-whpx".into(),
        vm_id: None,
        phase: BootPhase::Unknown,
        features,
        screenshot_source: None,
        screenshot_dimensions: None,
        text_layout: Some(
            "US physical keyboard; ASCII text only (Unicode via dev clipboard)".into(),
        ),
    }
}
pub struct Config {
    pub identity: CheckoutIdentity,
    pub runtime_dir: PathBuf,
    pub state_dir: PathBuf,
    pub disk: PathBuf,
    pub purpose: String,
    pub gui: bool,
    pub gl: bool,
    pub memory_bytes: u64,
    pub vcpus: u32,
    pub persist: bool,
}

pub fn max_slots() -> Result<usize> {
    Ok(std::env::var("ILEX_VM_SLOTS")
        .ok()
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(4)
        .clamp(1, 32))
}

/// Explicit migration/research input (`ILEX_DISK`), requiring a matching owner
/// receipt and installed-disk checksum. Preserve the originating executor in
/// image identity; only the delivered location changes to native Windows.
pub fn import_disk(source: &Path, identity: &CheckoutIdentity, state: &Path) -> Result<PathBuf> {
    let source = std::path::absolute(source)?;
    {
        use std::os::windows::fs::MetadataExt;
        for path in source.ancestors() {
            ensure!(
                fs::symlink_metadata(path)?.file_attributes() & 0x400 == 0,
                "reparse point in imported disk path"
            );
        }
    }
    let filename = source
        .file_name()
        .context("disk filename missing")?
        .to_string_lossy();
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(
        source.with_file_name(format!("{filename}.ilex-owner")),
    )?)?;
    ensure!(
        receipt["schema"] == 1 && receipt["owner"] == identity.owner,
        "ILEX_DISK belongs to another checkout or is unreceipted"
    );
    let artifact: ilex_tooling_core::DiskArtifact = serde_json::from_slice(&fs::read(
        source.with_file_name(format!("{filename}.disk.json")),
    )?)?;
    cache_disk(&source, artifact, identity, state)
}

/// Promote a verified T3 receive into the native immutable disk cache. The
/// caller holds the receiver's no-follow file and ancestor handles throughout.
pub fn cache_delivered_disk(
    artifact: ilex_tooling_core::DiskArtifact,
    identity: &CheckoutIdentity,
    state: &Path,
) -> Result<PathBuf> {
    ensure!(
        artifact.location.executor_id == format!("windows-owner:{}", identity.owner),
        "delivered disk owner mismatch"
    );
    let source = artifact.location.path.clone();
    cache_disk(&source, artifact, identity, state)
}

fn cache_disk(
    source: &Path,
    mut artifact: ilex_tooling_core::DiskArtifact,
    identity: &CheckoutIdentity,
    state: &Path,
) -> Result<PathBuf> {
    ilex_tooling_core::check_version(artifact.schema)?;
    let checksum = host::hash_file(source)?;
    ensure!(
        artifact.location.sha256 == format!("sha256:{checksum}"),
        "import disk checksum mismatch"
    );
    ensure!(
        artifact.provisioning_hash
            == ilex_tooling_core::digest(b"ilex-dev-session-v2;fwcfg-host-key;fwcfg-access-key"),
        "ILEX_DISK needs current disk --development provisioning"
    );
    let directory = state.join("disks");
    host::secure_dir(&directory)?;
    let disk = directory.join(format!("{checksum}.qcow2"));
    let _guard = host::lock(&directory.join(format!("{checksum}.import.lock")))?;
    if source != disk {
        if disk.exists() {
            ensure!(
                host::hash_file(&disk)? == checksum,
                "import cache checksum mismatch"
            );
        } else {
            ensure!(
                transport::available_space(&disk)? >= fs::metadata(source)?.len() + (30 << 30),
                "disk import needs its size plus 30 GiB free"
            );
            let partial = disk.with_extension("partial");
            fs::copy(source, &partial)?;
            ensure!(
                host::hash_file(&partial)? == checksum,
                "copied disk checksum mismatch"
            );
            fs::rename(partial, &disk)?;
        }
    }
    let name = disk.file_name().unwrap().to_string_lossy();
    host::atomic(
        &disk.with_file_name(format!("{name}.ilex-owner")),
        &serde_json::to_vec(&json!({"schema":1,"owner":identity.owner}))?,
    )?;
    artifact.location.path = disk.clone();
    artifact.location.executor_id = format!("windows-owner:{}", identity.owner);
    artifact.allocated_bytes = transport::allocated_bytes(&disk)?;
    host::atomic(
        &disk.with_file_name(format!("{name}.disk.json")),
        &serde_json::to_vec_pretty(&artifact)?,
    )?;
    Ok(disk)
}
pub struct Vm {
    pub info: VmInfo,
    child: Child,
    _job: host::ProcessJob,
    _lease: File,
    _slot: File,
    persistent: bool,
}
// Before Vm owns the launch, cancellation/failed admission/failed QEMU startup
// must still remove the newly created run's private keys and ephemeral files.
struct PendingRun(Option<PathBuf>);
impl Drop for PendingRun {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _ = fs::remove_dir_all(directory);
        }
    }
}
impl Vm {
    pub fn boot(config: Config) -> Result<Self> {
        ensure!(
            !config.purpose.is_empty()
                && config
                    .purpose
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid VM purpose"
        );
        ensure!(
            Architecture::host()? == Architecture::X86_64,
            "WHPX requires x86_64"
        );
        ensure!(
            config.memory_bytes >= 8 << 30,
            "desktop VM requires at least 8 GiB; reserve is never lowered"
        );
        ensure!(
            config.vcpus >= 4 && config.vcpus <= std::thread::available_parallelism()?.get() as u32,
            "desktop requires 4..host logical CPUs"
        );
        host::secure_dir(&config.runtime_dir)?;
        host::secure_dir(&config.state_dir)?;
        let dir = config.runtime_dir.join(&config.purpose);
        host::secure_dir(&dir)?;
        let lease = host::lock(&dir.join("lifetime.lock"))?;
        if dir.join("vm.json").exists() {
            let old = VmInfo::read(&dir)?;
            ensure!(
                old.owner == config.identity.owner && !old.is_running(),
                "foreign or live VM metadata; stop its owning launcher first"
            );
        }
        let runtime = runtime::fetch()?;
        let disk = std::path::absolute(&config.disk)?;
        ensure!(disk.is_file(), "development disk missing");
        let metadata: ilex_tooling_core::DiskArtifact =
            serde_json::from_slice(&fs::read(disk.with_file_name(format!(
                "{}.disk.json",
                disk.file_name().unwrap().to_string_lossy()
            )))?)?;
        ilex_tooling_core::check_version(metadata.schema)?;
        ensure!(
            metadata.location.executor_id == format!("windows-owner:{}", config.identity.owner),
            "disk delivery belongs to another checkout"
        );
        ensure!(
            metadata.provisioning_hash
                == ilex_tooling_core::digest(
                    b"ilex-dev-session-v2;fwcfg-host-key;fwcfg-access-key"
                ),
            "disk needs the private development composition; use disk --development"
        );
        ensure!(
            metadata.architecture == Architecture::X86_64
                && metadata.format == ilex_tooling_core::DiskFormat::Qcow2
                && metadata.firmware == ilex_tooling_core::Firmware::Uefi,
            "native development needs an x86_64 UEFI qcow2"
        );
        ensure!(
            metadata.location.sha256 == format!("sha256:{}", host::hash_file(&disk)?),
            "delivered disk checksum changed"
        );
        let inspection = host::output_owned(
            runtime::command(&runtime, "qemu-img.exe")
                .args(["info", "--output=json"])
                .arg(&disk),
            Stdio::null(),
        )?;
        ensure!(
            inspection.status.success(),
            "cannot inspect installed base disk"
        );
        let inspection: serde_json::Value = serde_json::from_slice(&inspection.stdout)?;
        ensure!(
            inspection["format"] == "qcow2"
                && inspection["virtual-size"].as_u64() == Some(metadata.virtual_bytes)
                && inspection["backing-filename"].is_null(),
            "installed base must be a standalone qcow2 matching its manifest"
        );
        let run = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        );
        let run_dir = dir.join(&run);
        fs::create_dir(&run_dir)?;
        let resolved_run = fs::canonicalize(&run_dir)?;
        ensure!(
            resolved_run.parent() == Some(fs::canonicalize(&dir)?.as_path()),
            "new run directory escaped its owned purpose"
        );
        let mut pending = PendingRun(Some(resolved_run));
        let key = run_dir.join("id_ed25519");
        let host_key = run_dir.join("host_ed25519");
        for path in [&key, &host_key] {
            host::command_ok(
                Command::new("ssh-keygen.exe")
                    .args(["-q", "-t", "ed25519", "-N", "", "-C", "ilex-dev"])
                    .arg("-f")
                    .arg(path),
            )?;
        }
        let public = fs::read(key.with_extension("pub"))?;
        let alias = format!("ilex-dev-{run}");
        let known_hosts = run_dir.join("known_hosts");
        fs::write(
            &known_hosts,
            format!(
                "{alias} {}",
                fs::read_to_string(host_key.with_extension("pub"))?
            ),
        )?;
        let overlay = if config.persist {
            config.state_dir.join(format!(
                "{}-{}.qcow2",
                config.purpose,
                &host::hash(metadata.location.sha256.as_bytes())[..16]
            ))
        } else {
            run_dir.join("overlay.qcow2")
        };
        if !overlay.exists() {
            host::command_ok(
                runtime::command(&runtime, "qemu-img.exe")
                    .args(["create", "-f", "qcow2", "-F", "qcow2", "-b"])
                    .arg(&disk)
                    .arg(&overlay),
            )?;
        }
        let vars = overlay.with_extension("vars.fd");
        if !vars.exists() {
            fs::copy(runtime.join("ucrt64/share/qemu/edk2-i386-vars.fd"), &vars)?;
        }
        let inspection = host::output_owned(
            runtime::command(&runtime, "qemu-img.exe")
                .args(["info", "--output=json"])
                .arg(&overlay),
            Stdio::null(),
        )?;
        ensure!(
            inspection.status.success(),
            "cannot inspect development overlay"
        );
        let inspection: serde_json::Value = serde_json::from_slice(&inspection.stdout)?;
        let backing = inspection["full-backing-filename"]
            .as_str()
            .or_else(|| inspection["backing-filename"].as_str())
            .context("overlay has no backing disk")?;
        ensure!(
            inspection["format"] == "qcow2"
                && fs::canonicalize(backing)? == fs::canonicalize(&disk)?,
            "persistent overlay does not belong to the delivered base disk"
        );
        // Serialize native admission; live available memory already counts the
        // fixed builder and any WSL/other VM allocations. Keep 14 GiB for host.
        let admission_dir = host::root()?.join("admission");
        host::secure_dir(&admission_dir)?;
        let deadline = Instant::now() + Duration::from_secs(10800);
        let limit = max_slots()?;
        let slot = loop {
            let mut slot = None;
            for index in 0..limit {
                if let Some(guard) =
                    host::try_lock(&admission_dir.join(format!("slot-{index}.lock")))?
                {
                    slot = Some(guard);
                    break;
                }
            }
            if let Some(slot) = slot {
                break slot;
            }
            ensure!(
                !transport::cancelled() && Instant::now() < deadline,
                "VM slot wait cancelled/timed out"
            );
            eprintln!("ilex: all {limit} native slots are occupied; waiting");
            thread::sleep(Duration::from_secs(2));
        };
        let admission = loop {
            if transport::cancelled() {
                bail!("native VM admission cancelled");
            }
            if let Some(guard) = host::try_lock(&admission_dir.join("host.lock"))?
                && host::available_memory()? >= config.memory_bytes + (14 << 30)
            {
                break guard;
            }
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for VM memory; host reserve unchanged"
            );
            eprintln!(
                "ilex: waiting for {:.0} GiB guest plus 14 GiB host reserve",
                config.memory_bytes as f64 / (1u64 << 30) as f64
            );
            thread::sleep(Duration::from_secs(10));
        };
        for attempt in 0..5 {
            let ports: Vec<_> = (0..3)
                .map(|_| TcpListener::bind(("127.0.0.1", 0)))
                .collect::<std::io::Result<_>>()?;
            let ssh_port = ports[0].local_addr()?.port();
            let control = format!("127.0.0.1:{}", ports[1].local_addr()?.port());
            let input = format!("127.0.0.1:{}", ports[2].local_addr()?.port());
            let name = format!("ilex-{}-{}", config.identity.slug, config.purpose);
            let mut command = runtime::command(&runtime, "qemu-system-x86_64.exe");
            command
                .env("SDL_WINDOWS_DPI_AWARENESS", "permonitorv2")
                // SDL's absolute-mouse notifier can run on a vCPU thread.
                // Its automatic grab updates the caption synchronously on
                // Windows, deadlocking against the UI thread's QEMU lock if
                // the pointer is already over a newly focused window. Show
                // without activation; the user's first click selects/grabs it.
                .env("SDL_WINDOW_ACTIVATE_WHEN_SHOWN", "0")
                .args([
                    "-name",
                    &name,
                    "-machine",
                    "q35",
                    "-accel",
                    "whpx,kernel-irqchip=off",
                    "-cpu",
                    "max",
                    "-mem-prealloc",
                    "-m",
                    &(config.memory_bytes >> 20).to_string(),
                    "-smp",
                    &config.vcpus.to_string(),
                ])
                .args([
                    "-device",
                    if config.gl {
                        "virtio-vga-gl,xres=1280,yres=800"
                    } else {
                        "virtio-vga,xres=1280,yres=800"
                    },
                    "-display",
                    if config.gui {
                        if config.gl {
                            "sdl,gl=on,grab-mod=rctrl"
                        } else {
                            "sdl,grab-mod=rctrl"
                        }
                    } else {
                        "none"
                    },
                ])
                .arg("-drive")
                .arg(format!(
                    "if=pflash,format=raw,readonly=on,file={}",
                    qemu_path(&runtime.join("ucrt64/share/qemu/edk2-x86_64-code.fd"))?
                ))
                .arg("-drive")
                .arg(format!("if=pflash,format=raw,file={}", qemu_path(&vars)?))
                .arg("-drive")
                .arg(format!(
                    "if=virtio,format=qcow2,file={}",
                    qemu_path(&overlay)?
                ))
                .args([
                    "-device",
                    // USB HID keeps its absolute handler active across guest
                    // driver shutdown. Virtio tablet deactivation invokes
                    // SDL's caption update on a vCPU thread and can deadlock
                    // Windows while grabbed (QEMU 11.1.3).
                    "qemu-xhci,id=ilex-usb",
                    "-device",
                    "usb-tablet,bus=ilex-usb.0",
                    "-device",
                    "virtio-keyboard-pci",
                    "-device",
                    "virtio-net-pci,netdev=net0",
                    "-netdev",
                    &format!("user,id=net0,hostfwd=tcp:127.0.0.1:{ssh_port}-:22"),
                ])
                .args([
                    "-qmp",
                    &format!("tcp:{control},server=on,wait=off"),
                    "-qmp",
                    &format!("tcp:{input},server=on,wait=off"),
                    "-monitor",
                    "none",
                ])
                .arg("-serial")
                .arg(format!("file:{}", qemu_path(&dir.join("serial.log"))?))
                .arg("-fw_cfg")
                .arg(format!(
                    "name=opt/ilex/ssh-host-key,file={}",
                    qemu_path(&host_key)?
                ))
                .arg("-fw_cfg")
                .arg(format!(
                    "name=opt/ilex/ssh-authorized-key,file={}",
                    qemu_path(&key.with_extension("pub"))?
                ))
                .arg("-smbios")
                .arg(format!(
                    "type=11,value=io.systemd.credential.binary:ssh.authorized_keys.root={}",
                    base64::engine::general_purpose::STANDARD.encode(public.trim_ascii())
                ))
                .stdin(Stdio::null())
                .stdout(File::create(dir.join("qemu-stdout.log"))?)
                .stderr(File::create(dir.join("qemu-stderr.log"))?);
            drop(ports); // QEMU has no socket-activation API; retry bind races.
            let (job, mut child) = host::spawn_owned(&mut command)?;
            let startup = (|| -> Result<VmInfo> {
                let mut info = VmInfo {
                    schema: 1,
                    owner: config.identity.owner.clone(),
                    run: run.clone(),
                    architecture: Architecture::X86_64,
                    process_start: process_start(child.id())?,
                    name,
                    purpose: config.purpose.clone(),
                    pid: child.id(),
                    hwnd: 0,
                    ssh_port,
                    ssh_key: key.clone(),
                    known_hosts: known_hosts.clone(),
                    host_key_alias: alias.clone(),
                    qmp_endpoint: control.clone(),
                    input_endpoint: input,
                    width: 1280,
                    height: 800,
                    display: if config.gui {
                        if config.gl { "sdl-gl" } else { "sdl" }
                    } else {
                        "none"
                    }
                    .into(),
                    gpu: config.gl,
                    clipboard: config.gui,
                    runtime: runtime.clone(),
                    disk: overlay.clone(),
                    dir: dir.clone(),
                };
                let until = Instant::now() + Duration::from_secs(30);
                loop {
                    ensure!(
                        child.try_wait()?.is_none(),
                        "QEMU exited during native startup; inspect {}",
                        dir.join("qemu-stderr.log").display()
                    );
                    let endpoints_match =
                        [&control, &info.input_endpoint]
                            .into_iter()
                            .all(|endpoint| {
                                Qmp::connect(endpoint)
                                    .and_then(|mut qmp| qmp.execute("query-name", json!({})))
                                    .is_ok_and(|reply| reply["name"] == info.name)
                            });
                    if endpoints_match
                        && (!config.gui
                            || crate::capture::find_window(info.pid).is_some_and(|h| {
                                info.hwnd = h;
                                true
                            }))
                    {
                        break;
                    }
                    ensure!(Instant::now() < until, "QEMU window/QMP startup timed out");
                    thread::sleep(Duration::from_millis(100));
                }
                host::atomic(&dir.join("vm.json"), &serde_json::to_vec_pretty(&info)?)?;
                Ok(info)
            })();
            match startup {
                Ok(info) => {
                    thread::sleep(Duration::from_secs(2));
                    drop(admission);
                    pending.0.take();
                    return Ok(Self {
                        info,
                        child,
                        _job: job,
                        _lease: lease,
                        _slot: slot,
                        persistent: config.persist,
                    });
                }
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    drop(job);
                    let log = fs::read_to_string(dir.join("qemu-stderr.log")).unwrap_or_default();
                    if attempt == 4
                        || !(log.contains("Address already in use")
                            || log.contains("Could not set up host forwarding"))
                    {
                        return Err(error).context(log);
                    }
                }
            }
        }
        unreachable!()
    }
    pub fn wait_ssh(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            ensure!(
                self.child.try_wait()?.is_none(),
                "QEMU exited before SSH was ready"
            );
            ensure!(!transport::cancelled(), "VM startup cancelled");
            if self
                .info
                .ssh()
                .arg("true")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()?
                .success()
            {
                return Ok(());
            }
            ensure!(
                Instant::now() < deadline,
                "pinned development SSH did not become ready; inspect serial.log"
            );
            thread::sleep(Duration::from_secs(1));
        }
    }
    pub fn exited(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }
}
impl Drop for Vm {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if let Ok(mut qmp) = Qmp::connect(&self.info.qmp_endpoint) {
                let _ = qmp.execute("system_powerdown", json!({}));
            }
            let until = Instant::now() + Duration::from_secs(15);
            while Instant::now() < until && self.child.try_wait().ok().flatten().is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Keep metadata and logs. Remove only exact run-owned ephemeral files;
        // persistent overlays and firmware always stay together.
        if !self.persistent {
            let _ = fs::remove_file(&self.info.disk);
            let _ = fs::remove_file(self.info.disk.with_extension("vars.fd"));
        }
        if let Some(directory) = self.info.ssh_key.parent() {
            for name in [
                "id_ed25519",
                "id_ed25519.pub",
                "host_ed25519",
                "host_ed25519.pub",
            ] {
                let _ = fs::remove_file(directory.join(name));
            }
        }
    }
}
fn qemu_path(path: &Path) -> Result<String> {
    let path = path.to_str().context("QEMU requires UTF-8 paths")?;
    ensure!(!path.contains(['\n', '\r']), "invalid QEMU path");
    Ok(path.replace('\\', "/").replace(',', ",,"))
}
