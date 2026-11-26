//! What an installation leaves on the new `/boot`, and what unattended mode
//! reads from a disk before it erases it again: the digests of the requests
//! ([`target::request_digest`]) that must not erase this installation.
//!
//! The file ([`target::REQUEST_STAMP`]) holds one digest per line. The newest
//! installation writes its own request's digest and keeps every digest the
//! disk's earlier stamp held, so that a person who installs again through the
//! interface does not make the answers that installed the disk before look new
//! (a machine that boots the stick first would erase the installation, every
//! time). It also keeps the digests of the answers that were *refused* in this
//! boot, before they called `Apply` ([`SEEN`]): answers with `reinstall = true`
//! that did not install (the disk was already theirs, the helper was not
//! there, the registry did not answer) and handed the machine to the
//! interface are not new answers after the person has installed by hand.
//!
//! A digest holds nothing secret and is not derived from a secret (see
//! [`target::request_digest`]).

use crate::{
    answers::{mounted_number, read_bounded},
    disks::node_number,
    exec,
    scratch::Scratch,
    target,
};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::probe::Disk;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

/// Where answers mode records the digest of the request it has built, as soon
/// as it has one, whatever becomes of it: root's file in `/run` (mode 0600),
/// one digest per line, gone at the next boot. The helper reads it when an
/// installation finishes.
pub const SEEN: &str = "/run/ilex-answers-seen";

/// The most digests one stamp holds: the newest are kept. Each is 65 bytes, so
/// the file is at most about 16 KiB; nobody installs a disk this many times.
pub const MAX_DIGESTS: usize = 256;

/// The longest stamp or record that is read.
const MAX_BYTES: u64 = 32 * 1024;

fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The digests in a stamp's text, in order: one 64-digit lowercase hexadecimal
/// number per line, blank lines aside. Anything else is an error, naming the
/// line: a stamp that is not made of digests may be exactly the installation
/// this exists to protect, and what cannot be read is not "no stamp". A text
/// without a digest is an error too.
pub fn parse(text: &str) -> Result<Vec<String>> {
    let mut digests = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        ensure!(is_digest(line), "line {} is not a digest", n + 1);
        if !digests.iter().any(|d| d == line) {
            digests.push(line.to_owned());
        }
    }
    ensure!(!digests.is_empty(), "it holds no digest");
    ensure!(
        digests.len() <= MAX_DIGESTS,
        "it holds more than {MAX_DIGESTS} digests"
    );
    Ok(digests)
}

/// What is written: one digest per line.
pub fn render(digests: &[String]) -> String {
    digests.iter().map(|d| format!("{d}\n")).collect()
}

/// The stamp of the installation that ends: what the disk's earlier stamp held,
/// then what answers mode saw in this boot, then its own digest, each once (the
/// first place counts), the oldest dropped past [`MAX_DIGESTS`]. The newest
/// installation's own digest is always there.
pub fn merge(carried: &[String], seen: &[String], own: &str) -> Vec<String> {
    let mut all: Vec<String> = Vec::new();
    for digest in carried.iter().chain(seen).map(String::as_str).chain([own]) {
        if !all.iter().any(|d| d == digest) {
            all.push(digest.to_owned());
        }
    }
    let excess = all.len().saturating_sub(MAX_DIGESTS);
    all.drain(..excess);
    all
}

/// Whether the digest of the request now being made is one the stamp holds:
/// these answers already installed this disk, or were refused and handed over
/// while it was installed by hand.
pub fn holds(stamp: &[String], digest: &str) -> bool {
    stamp.iter().any(|d| d == digest)
}

/// The digests answers mode saw in this boot, from the file `path` ([`SEEN`]).
/// Not there is no digest. The file must be a regular file of root's, not
/// readable or writable by anyone else, opened without following a link: it is
/// what a program that could write in `/run` would otherwise forge.
pub fn read_seen(path: &Path) -> Result<Vec<String>> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => bail!("looking at {}: {e}", path.display()),
    }
    // Checked on the descriptor that is read, not on the name.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .with_context(|| format!("{} cannot be opened (a link?)", path.display()))?;
    let meta = file.metadata().context("looking at the record")?;
    ensure!(
        meta.file_type().is_file()
            && meta.uid() == 0
            && meta.permissions().mode() & 0o077 == 0
            && meta.len() <= MAX_BYTES,
        "{} is not root's own regular file (mode 0600) of at most {MAX_BYTES} bytes",
        path.display()
    );
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "{} grew while it was read",
        path.display()
    );
    let text = std::str::from_utf8(&bytes).context("the record is not text")?;
    parse(text).with_context(|| format!("{}", path.display()))
}

/// Add `digest` to the record at `path` ([`SEEN`]): written whole (a new file
/// renamed over the old one), mode 0600, once. Called with the digest of the
/// request answers mode has built, before anything else can end it.
pub fn record_seen(path: &Path, digest: &str) -> Result<()> {
    ensure!(is_digest(digest), "not a digest");
    // A record that cannot be read is replaced (it was not ours).
    let mut all = read_seen(path).unwrap_or_default();
    if all.iter().any(|d| d == digest) {
        return Ok(());
    }
    all.push(digest.to_owned());
    let excess = all.len().saturating_sub(MAX_DIGESTS);
    all.drain(..excess);
    let partial = path.with_extension("partial");
    let _ = fs::remove_file(&partial);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&partial)
        .with_context(|| format!("writing {}", partial.display()))?;
    file.write_all(render(&all).as_bytes())?;
    file.sync_all()?;
    fs::rename(&partial, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// What a disk's `/boot` held, read before anything is written to it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Earlier {
    /// The digests of its stamp: none when the disk has no `ilex-boot`
    /// partition or no stamp on it (an installation that did not finish, or
    /// one made by another program).
    pub digests: Vec<String>,
    /// Why the stamp could not be read, when it could not: what is on the
    /// disk is unknown, not absent.
    pub unreadable: Option<String>,
}

/// The stamp of a disk, as the digests it holds: the partition labelled
/// `ilex-boot` with an ext4 file system, mounted read-only without journal
/// replay (`noload`: not one byte is written) in the mount namespace of the
/// caller, and unmounted before anything else is asked of the disk. `Ok` with
/// no digest is no such partition, or none on it. A file system that cannot be
/// mounted, or a stamp that is not made of digests, is an error: it may be
/// exactly the installation the caller wants to protect.
pub fn read_boot(run_dir: &Path, disk: &Disk) -> Result<Vec<String>> {
    let Some(boot) = disk
        .partitions
        .iter()
        .find(|p| p.label == target::BOOT_LABEL && p.filesystem == "ext4")
    else {
        return Ok(Vec::new());
    };
    let number = node_number(&boot.path).map_err(anyhow::Error::msg)?;
    let mut scratch = Scratch::new(run_dir, "stamp")?;
    let mountpoint = scratch.subdir("boot")?;
    let mount_point = mountpoint.to_string_lossy().into_owned();
    let mounted = exec::run(
        "mount",
        &[
            "-t",
            "ext4",
            "-o",
            "ro,noload,nodev,nosuid,noexec",
            &boot.path,
            &mount_point,
        ],
        Duration::from_secs(60),
    )?;
    ensure!(
        mounted.success(),
        "{} ({}) could not be mounted read-only to look for the stamp of an earlier installation ({}); erasing what might be an installation is not done blind",
        boot.path,
        target::BOOT_LABEL,
        exec::clean_line(
            mounted
                .stderr
                .lines()
                .last()
                .unwrap_or("no message")
                .as_bytes()
        )
    );
    let table = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    let same = mounted_number(&table, &mount_point) == Some(number);
    let read = if same {
        match fs::symlink_metadata(mountpoint.join(target::REQUEST_STAMP)) {
            Ok(_) => read_bounded(
                &mountpoint.join(target::REQUEST_STAMP),
                "the stamp",
                MAX_BYTES,
            )
            .map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow::anyhow!("looking for the stamp: {e}")),
        }
    } else {
        Err(anyhow::anyhow!(
            "what was mounted is not {} (the partition was replaced)",
            boot.path
        ))
    };
    let mut unmounted =
        exec::run("umount", &[&mount_point], Duration::from_secs(60)).is_ok_and(|o| o.success());
    if !unmounted {
        // Lazily, so that nothing of it stays in the way of the claim the
        // caller makes next; the read-only mount holds nothing open.
        unmounted = exec::run("umount", &["-l", &mount_point], Duration::from_secs(60))
            .is_ok_and(|o| o.success());
    }
    let read = read?;
    ensure!(
        unmounted,
        "the partition {} could not be unmounted after the stamp was read",
        boot.path
    );
    let Some(bytes) = read else {
        return Ok(Vec::new());
    };
    let text = std::str::from_utf8(&bytes).context("the stamp is not text");
    text.and_then(parse)
        .with_context(|| format!("the stamp on {}", boot.path))
}

/// [`read_boot`] for a caller that carries the digests on and does not refuse
/// over them (the helper, when an installation is about to erase the disk): an
/// unreadable stamp is reported, not fatal.
pub fn earlier(run_dir: &Path, disk: &Disk) -> Earlier {
    match read_boot(run_dir, disk) {
        Ok(digests) => Earlier {
            digests,
            unreadable: None,
        },
        Err(error) => Earlier {
            digests: Vec::new(),
            unreadable: Some(format!("{error:#}")),
        },
    }
}
