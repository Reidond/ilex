//! The image store and its runtime state: `install`, `remove`, `activate`,
//! `list`.
//!
//! Activating a tool mounts its image under `/run/drupe/priv` and publishes
//! its commands: an entry in `/run/drupe/cmd` and a symlink in
//! `/run/drupe/bin`. Images are mounted by `systemd-dissect`, which sets up
//! dm-verity and, because of [`POLICY`], refuses an image whose root hash is
//! not signed by a trusted certificate.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::manifest::{Manifest, valid_name};
use crate::paths::{BIN, CMD, DRUPE, PRIV, RUN, STORE, TOOLS};

/// The image must be a signed, verity-protected root partition (with its
/// hash and signature partitions) and nothing else.
const POLICY: &str = "root=signed:=absent+unused";

pub fn install(source: &Path) -> Result<(), String> {
    require_root()?;
    mkdir(Path::new(STORE), 0o755)?;
    runtime_dirs()?;

    // Take a copy first: what gets inspected is what gets installed.
    let incoming = Path::new(STORE).join(format!(".incoming-{}", std::process::id()));
    fs::copy(source, &incoming).map_err(|e| format!("copying {}: {e}", source.display()))?;
    let result = install_copy(&incoming);
    if result.is_err() {
        let _ = fs::remove_file(&incoming);
    }
    result
}

fn install_copy(incoming: &Path) -> Result<(), String> {
    fs::set_permissions(incoming, Permissions::from_mode(0o644)).map_err(|e| e.to_string())?;
    let probe = Path::new(PRIV).join(format!(".incoming-{}", std::process::id()));
    mount(incoming, &probe)?;
    // dm-verity checks a block when it is read, so reading every file now
    // turns a damaged image into a failed install instead of an I/O error in
    // the middle of some later run.
    let manifest = read_everything(&probe)
        .map_err(|e| format!("the image is damaged: {e}"))
        .and_then(|()| Manifest::read(&probe.join("drupe.json")));
    unmount(&probe)?;
    let manifest = manifest?;
    let name = &manifest.name;

    for cmd in manifest.commands.keys() {
        if let Some(owner) = command_owner(cmd).filter(|owner| owner != name) {
            return Err(format!("`{cmd}` is already provided by the tool `{owner}`"));
        }
    }
    let image = image_path(name);
    if image.exists() {
        deactivate(name)?;
    }
    fs::rename(incoming, &image).map_err(|e| format!("{}: {e}", image.display()))?;
    if let Err(why) = activate(name) {
        let _ = deactivate(name);
        let _ = fs::remove_file(&image);
        return Err(why);
    }
    println!(
        "installed {name} {}: {}",
        manifest.version,
        manifest.commands.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

pub fn remove(name: &str) -> Result<(), String> {
    require_root()?;
    let image = image_path(name);
    if !valid_name(name) || !image.exists() {
        return Err(format!("`{name}` is not installed"));
    }
    deactivate(name)?;
    fs::remove_file(&image).map_err(|e| format!("{}: {e}", image.display()))?;
    println!("removed {name}");
    Ok(())
}

/// Mount and publish every installed image. One broken image doesn't keep
/// the others from working, but it does fail the command (and the unit).
pub fn activate_all() -> Result<(), String> {
    require_root()?;
    runtime_dirs()?;
    let mut failed = Vec::new();
    for name in installed() {
        if let Err(why) = activate(&name) {
            eprintln!("drupe: {name}: {why}");
            let _ = deactivate(&name);
            failed.push(name);
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("could not activate: {}", failed.join(", ")))
    }
}

pub fn list() -> Result<(), String> {
    for name in installed() {
        let manifest = fs::read_to_string(Path::new(TOOLS).join(format!("{name}.json")))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        match manifest {
            Some(doc) => {
                let commands: Vec<&str> = doc["commands"]
                    .as_object()
                    .map(|c| c.keys().map(String::as_str).collect())
                    .unwrap_or_default();
                println!(
                    "{name}\t{}\t{}",
                    doc["version"].as_str().unwrap_or("?"),
                    commands.join(" ")
                );
            }
            None => println!("{name}\t-\t(not active: run `drupe activate` as root)"),
        }
    }
    Ok(())
}

/// Names of the images in the store, sorted.
fn installed() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(STORE)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let file = entry.file_name().into_string().ok()?;
            let name = file.strip_suffix(".raw")?;
            valid_name(name).then(|| name.to_string())
        })
        .collect();
    names.sort();
    names
}

fn activate(name: &str) -> Result<(), String> {
    let mnt = Path::new(PRIV).join(name);
    if !is_mount_point(&mnt) {
        mount(&image_path(name), &mnt)?;
    }
    let manifest = Manifest::read(&mnt.join("drupe.json"))?;
    if manifest.name != name {
        return Err(format!("the image calls itself `{}`", manifest.name));
    }
    for cmd in manifest.commands.keys() {
        if let Some(owner) = command_owner(cmd).filter(|owner| owner != name) {
            return Err(format!("`{cmd}` is already provided by the tool `{owner}`"));
        }
    }
    for (cmd, command) in &manifest.commands {
        let entry = command.entry(name, cmd).to_string();
        write(&Path::new(CMD).join(format!("{cmd}.json")), &entry)?;
        let link = Path::new(BIN).join(cmd);
        let _ = fs::remove_file(&link);
        symlink(DRUPE, &link).map_err(|e| format!("{}: {e}", link.display()))?;
    }
    write(&Path::new(TOOLS).join(format!("{name}.json")), &manifest.text)
}

/// Unpublish a tool's commands and unmount its image. Fine to call for a
/// tool that is only half active.
fn deactivate(name: &str) -> Result<(), String> {
    for entry in fs::read_dir(CMD).into_iter().flatten().flatten() {
        let file = entry.file_name().to_string_lossy().into_owned();
        let Some(cmd) = file.strip_suffix(".json") else { continue };
        if command_owner(cmd).as_deref() == Some(name) {
            let _ = fs::remove_file(Path::new(BIN).join(cmd));
            let _ = fs::remove_file(entry.path());
        }
    }
    let _ = fs::remove_file(Path::new(TOOLS).join(format!("{name}.json")));
    let mnt = Path::new(PRIV).join(name);
    if is_mount_point(&mnt) {
        unmount(&mnt)?;
    } else {
        let _ = fs::remove_dir(&mnt);
    }
    Ok(())
}

/// The active tool that provides `cmd`, if any.
fn command_owner(cmd: &str) -> Option<String> {
    let text = fs::read_to_string(Path::new(CMD).join(format!("{cmd}.json"))).ok()?;
    let doc: Value = serde_json::from_str(&text).ok()?;
    doc["tool"].as_str().map(str::to_string)
}

fn image_path(name: &str) -> PathBuf {
    Path::new(STORE).join(format!("{name}.raw"))
}

fn mount(image: &Path, at: &Path) -> Result<(), String> {
    mkdir(at, 0o755)?;
    let out = Command::new("systemd-dissect")
        .args(["--mount", "--read-only", "--image-policy", POLICY])
        .arg(image)
        .arg(at)
        .output()
        .map_err(|e| format!("systemd-dissect: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let _ = fs::remove_dir(at);
    // The kernel keyring is tried first and libdevmapper logs that miss even
    // when the certificate in verity.d then verifies the signature; the last
    // line is systemd-dissect's own verdict.
    let stderr = String::from_utf8_lossy(&out.stderr);
    let why = stderr.lines().last().unwrap_or("systemd-dissect failed");
    Err(format!("{}: {}", image.display(), strip_ansi(why)))
}

fn unmount(at: &Path) -> Result<(), String> {
    let out = Command::new("systemd-dissect")
        .arg("--umount")
        .arg(at)
        .output()
        .map_err(|e| format!("systemd-dissect: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "unmounting {}: {}",
            at.display(),
            strip_ansi(stderr.lines().last().unwrap_or("systemd-dissect failed"))
        ));
    }
    let _ = fs::remove_dir(at);
    Ok(())
}

/// systemd colours its errors even when stderr is a pipe under SSH.
fn strip_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Read every regular file under `dir` to the end.
fn read_everything(dir: &Path) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            read_everything(&entry.path())?;
        } else if kind.is_file() {
            io::copy(&mut fs::File::open(entry.path())?, &mut io::sink())
                .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", entry.path().display())))?;
        }
    }
    Ok(())
}

/// A directory on another filesystem than its parent.
fn is_mount_point(dir: &Path) -> bool {
    let (Ok(this), Some(Ok(parent))) = (fs::metadata(dir), dir.parent().map(fs::metadata)) else {
        return false;
    };
    this.is_dir() && this.dev() != parent.dev()
}

fn runtime_dirs() -> Result<(), String> {
    mkdir(Path::new(RUN), 0o755)?;
    mkdir(Path::new(PRIV), 0o700)?;
    for dir in [CMD, BIN, TOOLS] {
        mkdir(Path::new(dir), 0o755)?;
    }
    Ok(())
}

/// Create a directory (and its parents) and set its mode whatever the umask.
fn mkdir(dir: &Path, mode: u32) -> Result<(), String> {
    let done = DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(dir)
        .and_then(|()| fs::set_permissions(dir, Permissions::from_mode(mode)));
    done.map_err(|e| format!("{}: {e}", dir.display()))
}

/// Write a world-readable file through a rename, so a reader never sees half
/// of it.
fn write(path: &Path, text: &str) -> Result<(), String> {
    let tmp = path.with_extension("tmp");
    let done: io::Result<()> = fs::write(&tmp, text)
        .and_then(|()| fs::set_permissions(&tmp, Permissions::from_mode(0o644)))
        .and_then(|()| fs::rename(&tmp, path));
    done.map_err(|e| format!("{}: {e}", path.display()))
}

fn require_root() -> Result<(), String> {
    // SAFETY: no arguments, cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        Ok(())
    } else {
        Err("this needs root".to_string())
    }
}
