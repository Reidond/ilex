//! Reviewed MSYS2 archive closure; no pacman, elevation, or global PATH edits.
use crate::host::*;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

const LOCK: &str = include_str!("../assets/runtime-lock.json");
#[derive(Debug, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub bytes: u64,
}
#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub id: String,
    pub architecture: ilex_tooling_core::Architecture,
    pub packages: Vec<Package>,
}
#[derive(Debug, Serialize)]
pub struct Status {
    pub schema: u32,
    pub locked_id: String,
    pub lock_sha256: String,
    pub active: Option<PathBuf>,
    pub ready: bool,
    pub path: PathBuf,
}
pub fn manifest() -> Result<Manifest> {
    let m: Manifest = serde_json::from_str(LOCK)?;
    ilex_tooling_core::check_version(m.schema)?;
    ensure!(
        m.architecture == ilex_tooling_core::Architecture::host()?,
        "runtime architecture mismatch"
    );
    for p in &m.packages {
        ensure!(
            p.url.starts_with("https://repo.msys2.org/mingw/ucrt64/")
                && p.sha256.len() == 64
                && p.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                && !p.name.is_empty()
                && !p.version.is_empty(),
            "invalid reviewed package lock"
        );
    }
    Ok(m)
}
pub fn locked_path() -> Result<PathBuf> {
    Ok(root()?.join("runtimes").join(format!(
        "{}-{}",
        manifest()?.id,
        &hash(LOCK.as_bytes())[..16]
    )))
}
pub fn status() -> Result<Status> {
    let path = locked_path()?;
    let active = fs::read(root()?.join("runtimes/active.json"))
        .ok()
        .map(|b| serde_json::from_slice(&b))
        .transpose()?;
    let ready = fs::read_to_string(path.join("ready.json")).is_ok_and(|s| s == LOCK);
    Ok(Status {
        schema: 1,
        locked_id: manifest()?.id,
        lock_sha256: hash(LOCK.as_bytes()),
        active,
        ready,
        path,
    })
}
pub fn command(path: &Path, exe: &str) -> Command {
    let bin = path.join("ucrt64/bin");
    let mut c = Command::new(bin.join(exe));
    // Limit DLL lookup to this runtime and Windows, independently of MSYS2/global PATH.
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    c.env(
        "PATH",
        std::env::join_paths([bin, PathBuf::from(system).join("System32")])
            .expect("valid runtime path"),
    );
    c
}
pub fn fetch() -> Result<PathBuf> {
    let packages = manifest()?.packages;
    let base = root()?.join("runtimes");
    secure_dir(&root()?)?;
    secure_dir(&base)?;
    let _guard = lock(&base.join("install.lock"))?;
    let path = locked_path()?;
    if status()?.ready {
        return Ok(path);
    }
    ensure!(
        fs2::available_space(&base)? >= 34 * 1024 * 1024 * 1024,
        "runtime bootstrap requires 34 GiB free (30 GiB reserve plus download/extraction peak)"
    );
    let cache = base.join("archives");
    fs::create_dir_all(&cache)?;
    let staging = path.with_extension("partial");
    if staging.exists() {
        remove_owned_tree(&staging, &base)?;
    }
    fs::create_dir(&staging)?;
    // Failed/interrupted extraction remains unpublished and is removed on the next fetch.
    for batch in packages.chunks(8) {
        std::thread::scope(|scope| -> Result<()> {
            let mut pending = Vec::new();
            for p in batch {
                let archive = cache.join(format!("{}.tar.zst", p.sha256));
                pending.push(scope.spawn(move || download(&p.url, &archive, &p.sha256, p.bytes)));
            }
            for result in pending {
                result
                    .join()
                    .map_err(|_| anyhow::anyhow!("runtime download worker panicked"))??;
            }
            Ok(())
        })?;
    }
    for p in packages {
        eprintln!("runtime: extracting {} {}", p.name, p.version);
        let archive = cache.join(format!("{}.tar.zst", p.sha256));
        verify(&archive, &p.sha256)?;
        extract(&archive, &staging, &p.name)?;
    }
    for exe in ["qemu-img.exe", "qemu-system-x86_64.exe"] {
        command_ok(command(&staging, exe).arg("--version"))?;
    }
    atomic(&staging.join("ready.json"), LOCK.as_bytes())?;
    ensure!(
        !path.exists(),
        "unready final runtime directory exists; refusing replacement: {}",
        path.display()
    );
    fs::rename(&staging, &path)?;
    Ok(path)
}
fn safe_name(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.starts_with('/') && !name.contains(['\\', ':', '\0']),
        "unsafe archive path: {name}"
    );
    for part in name.trim_end_matches('/').split('/') {
        ensure!(
            !part.is_empty() && part != "." && part != ".." && !part.ends_with(['.', ' ']),
            "unsafe archive component: {name}"
        );
        let stem = part
            .split('.')
            .next()
            .unwrap()
            .trim_end_matches(' ')
            .to_ascii_uppercase();
        let port = stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"));
        let device_port = port.is_some_and(|p| {
            matches!(
                p,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
        ensure!(
            !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ) && !device_port,
            "Windows device path in archive"
        );
    }
    Ok(())
}
fn extract(archive: &Path, destination: &Path, package: &str) -> Result<()> {
    let decoder = zstd::Decoder::new(File::open(archive)?)?;
    extract_tar(decoder, destination, package)
}
fn extract_tar(reader: impl Read, destination: &Path, package: &str) -> Result<()> {
    for entry in tar::Archive::new(reader).entries()? {
        let mut entry = entry?;
        let name = std::str::from_utf8(&entry.path_bytes())?.to_owned();
        safe_name(&name)?;
        let ty = entry.header().entry_type();
        if ty.is_symlink() || ty.is_hard_link() {
            let target = entry.link_name()?.context("link without target")?;
            let target = target.to_str().context("non-UTF8 archive link")?;
            // Walk a link's effective path; permit only in-root documentation links,
            // which are omitted because they require Windows symlink privileges.
            ensure!(
                !target.starts_with('/') && !target.contains(['\\', ':']),
                "absolute/escaping archive link"
            );
            let mut depth = if ty.is_hard_link() {
                0
            } else {
                name.split('/').count() - 1
            };
            for part in target.split('/') {
                if part == ".." {
                    ensure!(depth > 0, "escaping archive link");
                    depth -= 1;
                } else if part != "." && !part.is_empty() {
                    depth += 1;
                }
            }
            if ty.is_hard_link() {
                safe_name(target)?;
                ensure!(
                    target.starts_with("ucrt64/"),
                    "hard link outside runtime root"
                );
                let source = destination.join(target);
                ensure!(
                    source.is_file(),
                    "archive hard-link target is not an extracted regular file: {target}"
                );
                let output = destination.join(&name);
                fs::create_dir_all(output.parent().unwrap())?;
                ensure!(!output.exists(), "duplicate archive hard link: {name}");
                // Materialize bytes rather than creating a filesystem link.
                fs::copy(source, output)?;
                continue;
            }
            ensure!(
                name.starts_with("ucrt64/share/")
                    || name.starts_with("ucrt64/lib/")
                    || name.starts_with("ucrt64/include/"),
                "runtime executable link cannot be omitted: {name}"
            );
            continue;
        }
        ensure!(
            ty.is_file() || ty.is_dir(),
            "unsupported archive entry: {name}"
        );
        let out = if name.starts_with("ucrt64/") {
            destination.join(&name)
        } else {
            // Retain pacman hooks under usr/ as inert package metadata.
            destination
                .join("package-metadata")
                .join(package)
                .join(&name)
        };
        if ty.is_dir() {
            fs::create_dir_all(&out)?;
            continue;
        }
        fs::create_dir_all(out.parent().unwrap())?;
        // No links have been created, and staging was empty and ACL protected.
        if out.exists() {
            let mut incoming = Vec::new();
            entry.read_to_end(&mut incoming)?;
            ensure!(
                fs::read(&out)? == incoming,
                "conflicting package file: {name}"
            );
        } else {
            let mut file = File::create(out)?;
            std::io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}
fn remove_owned_tree(path: &Path, base: &Path) -> Result<()> {
    ensure!(
        path.parent() == Some(base) && path.extension().is_some_and(|x| x == "partial"),
        "unsafe runtime cleanup target"
    );
    use std::os::windows::fs::MetadataExt;
    ensure!(
        fs::symlink_metadata(path)?.file_attributes() & 0x400 == 0,
        "reparse point at runtime staging root"
    );
    fn reject_links(p: &Path) -> Result<()> {
        for e in fs::read_dir(p)? {
            let e = e?;
            let m = fs::symlink_metadata(e.path())?;
            ensure!(
                m.file_attributes() & 0x400 == 0,
                "reparse point in runtime staging"
            );
            if m.is_dir() {
                reject_links(&e.path())?;
            }
        }
        Ok(())
    }
    reject_links(path)?;
    fs::remove_dir_all(path)?;
    Ok(())
}
/// Firmware boots with the actual WHPX/SDL/virgl devices before activation.
/// This checks host GL initialization; application rendering is qualified by T4.
pub fn update() -> Result<PathBuf> {
    let path = fetch()?;
    gpu_smoke(&path)?;
    let base = root()?.join("runtimes");
    let _guard = lock(&base.join("install.lock"))?;
    let pointer = base.join("active.json");
    if pointer.exists() {
        atomic(&base.join("previous.json"), &fs::read(&pointer)?)?;
    }
    atomic(&pointer, &serde_json::to_vec(&path)?)?;
    Ok(path)
}
/// Exercise a real failed first update, then successful GPU activation in a
/// fresh per-process LOCALAPPDATA. The caller supplies its xtask executable.
pub fn activation_self_check(xtask: &Path) -> Result<serde_json::Value> {
    let candidate = fetch()?;
    let base = root()?.join("runtimes");
    let _guard = lock(&base.join("install.lock"))?;
    let pointer = base.join("active.json");
    let before = fs::read(&pointer).ok();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let fixture = base.join(format!(
        "activation-check-{}-{nonce}.partial",
        std::process::id()
    ));
    fs::create_dir(&fixture)?;
    let result = (|| -> Result<serde_json::Value> {
        let script = fixture.join("check.ps1");
        let request = fixture.join("request.json");
        fs::write(
            &script,
            include_str!("../assets/runtime-activation-check.ps1"),
        )?;
        fs::write(
            &request,
            serde_json::to_vec(&serde_json::json!({
                "schema":1,"fixture":fixture,"archives":base.join("archives"),
            "xtask":xtask,"candidate_name":candidate.file_name().and_then(|n| n.to_str()).context("non-UTF8 runtime name")?
            }))?,
        )?;
        let output = output_owned(&mut powershell(&script, &request), Stdio::inherit())?;
        ensure!(
            output.status.success(),
            "runtime activation self-check failed"
        );
        Ok(serde_json::from_slice(&output.stdout)?)
    })();
    remove_owned_tree(&fixture, &base)?;
    ensure!(
        fs::read(pointer).ok() == before,
        "normal runtime pointer changed during isolated check"
    );
    let mut value = result?;
    value["normal_pointer_unchanged"] = true.into();
    value["fixture_removed"] = true.into();
    Ok(value)
}
fn gpu_smoke(path: &Path) -> Result<()> {
    let available = available_memory()?;
    ensure!(
        available >= 14 * 1024 * 1024 * 1024 + 256 * 1024 * 1024,
        "GPU fixture needs 256 MiB plus the 14 GiB host reserve; only {:.1} GiB available",
        available as f64 / 1024.0 / 1024.0 / 1024.0
    );
    let reservation = TcpListener::bind(("127.0.0.1", 0))?;
    let port = reservation.local_addr()?.port();
    drop(reservation);
    let endpoint = format!("tcp:127.0.0.1:{port},server=on,wait=off");
    let fixture_name = format!("ilex-runtime-smoke-{}-{port}", std::process::id());
    let (_job, mut child) = spawn_owned(
        command(path, "qemu-system-x86_64.exe")
            .args([
                "-name",
                &fixture_name,
                "-accel",
                "whpx,kernel-irqchip=off",
                "-machine",
                "q35",
                "-cpu",
                "max",
                "-m",
                "256",
                "-smp",
                "1",
                "-nic",
                "none",
                "-device",
                "virtio-vga-gl",
                "-display",
                "sdl,gl=on",
                "-qmp",
                &endpoint,
                "-monitor",
                "none",
                "-serial",
                "none",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null()),
    )?;
    let result = (|| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let stream = loop {
            if let Ok(stream) = TcpStream::connect(("127.0.0.1", port)) {
                break stream;
            }
            ensure!(
                child.try_wait()?.is_none(),
                "WHPX/SDL/GL firmware fixture exited before QMP"
            );
            ensure!(
                Instant::now() < deadline,
                "GPU fixture QMP startup timed out (loopback port {port})"
            );
            std::thread::sleep(Duration::from_millis(100));
        };
        let mut client = ilex_tooling_qmp::Client::connect(
            TcpTransport(BufReader::new(stream)),
            // Allow cold SDL/GL initialization to reach QEMU's main loop on a
            // busy host, while keeping every I/O operation bounded.
            Duration::from_secs(30),
        )
        .context("GPU fixture QMP handshake")?;
        let identity = client
            .execute("query-name", serde_json::json!({}))
            .context("GPU fixture QMP identity")?;
        ensure!(
            identity["name"] == fixture_name,
            "QMP fixture identity mismatch; refusing VM control"
        );
        std::thread::sleep(Duration::from_secs(3));
        let status = client.execute("query-status", serde_json::json!({}))?;
        ensure!(
            status["running"] == true,
            "GPU fixture did not run firmware: {status}"
        );
        client.execute("quit", serde_json::json!({}))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while child.try_wait()?.is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        ensure!(
            child.try_wait()?.is_some(),
            "GPU fixture shutdown timed out"
        );
        ensure!(child.wait()?.success(), "GPU fixture failed");
        Ok(())
    })();
    if child.try_wait()?.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}
struct TcpTransport(BufReader<TcpStream>);
impl ilex_tooling_qmp::Transport for TcpTransport {
    fn read_message(&mut self, deadline: Instant) -> Result<serde_json::Value> {
        let mut bytes = Vec::new();
        loop {
            ensure!(Instant::now() < deadline, "QMP response deadline expired");
            self.0.get_mut().set_read_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))?;
            let buffer = self.0.fill_buf()?;
            ensure!(!buffer.is_empty(), "QMP endpoint closed");
            let count = buffer
                .iter()
                .position(|b| *b == b'\n')
                .map_or(buffer.len(), |i| i + 1);
            ensure!(
                (bytes.len() + count) as u64 <= ilex_tooling_qmp::MAX_MESSAGE_BYTES,
                "oversized QMP response"
            );
            let complete = buffer[count - 1] == b'\n';
            bytes.extend_from_slice(&buffer[..count]);
            self.0.consume(count);
            if complete {
                break;
            }
        }
        Ok(serde_json::from_slice(&bytes)?)
    }
    fn write_message(&mut self, message: &serde_json::Value, deadline: Instant) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.extend_from_slice(b"\r\n");
        let mut offset = 0;
        while offset < bytes.len() {
            ensure!(Instant::now() < deadline, "QMP write deadline expired");
            self.0.get_mut().set_write_timeout(Some(
                deadline
                    .saturating_duration_since(Instant::now())
                    .max(Duration::from_millis(1)),
            ))?;
            let count = self.0.get_mut().write(&bytes[offset..])?;
            ensure!(count != 0, "QMP endpoint closed during write");
            offset += count;
        }
        Ok(())
    }
}
pub fn self_check() -> Result<()> {
    let path = fetch()?;
    command_ok(command(&path, "qemu-img.exe").arg("--version"))?;
    // Exercise the acquisition boundary with damaged bytes and hostile archives.
    let base = root()?.join("runtimes");
    let _guard = lock(&base.join("install.lock"))?;
    let fixture = base.join("self-check.partial");
    if fixture.exists() {
        remove_owned_tree(&fixture, &base)?;
    }
    fs::create_dir(&fixture)?;
    let result = (|| -> Result<()> {
        fs::write(fixture.join("bad"), b"damaged")?;
        ensure!(
            verify(&fixture.join("bad"), &"0".repeat(64)).is_err(),
            "checksum negative accepted"
        );
        for name in [
            "../escape",
            "C:/escape",
            "ucrt64/bin/CON",
            "ucrt64/bin/../escape",
        ] {
            ensure!(safe_name(name).is_err(), "unsafe path accepted");
        }
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_link_name("../../../escape")?;
        header.set_cksum();
        archive.append_data(&mut header, "ucrt64/share/link", std::io::empty())?;
        ensure!(
            extract_tar(archive.into_inner()?.as_slice(), &fixture, "fixture").is_err(),
            "escaping link accepted"
        );
        Ok(())
    })();
    remove_owned_tree(&fixture, &base)?;
    result?;
    eprintln!(
        "runtime self-check: real qemu-img loaded; corrupt SHA256, traversal, device paths and link escape rejected"
    );
    Ok(())
}
