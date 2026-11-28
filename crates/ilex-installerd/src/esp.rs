//! The EFI system partition free-space mode shares with the other systems on
//! the disk, and the firmware's boot entries beside it.
//!
//! `bootupd` (which `bootc install` runs) copies ilex's loaders into the
//! partition: `EFI/fedora/*`, and `EFI/BOOT/BOOTX64.EFI` and `fbx64.efi`, the
//! fallback path, **replacing a file of the same name** (measured with
//! `bootupctl backend install` of `generic.gnome` on a FAT image that held a
//! Windows-like `EFI/BOOT/BOOTX64.EFI`: ilex's shim took its place). Windows'
//! own setup leaves a copy of its boot manager there. So the installation
//! reads every file of the partition before `bootc` (a [`Snapshot`]), puts
//! back what the install replaced under `EFI/BOOT`, and requires every other
//! file to be as it was: the other loaders stay byte for byte, and ilex is
//! reached through its own firmware boot entry (`bootupd --update-firmware`),
//! which is added beside the others and never replaces them.
//!
//! A failed installation undoes what it added here ([`Snapshot::undo`]) and puts
//! the firmware's variables back ([`Nvram::restore`]). An installation that is
//! interrupted (a power cut, a crashed helper) in the seconds `bootc` takes
//! between `bootupd` and its return is not undone: nothing is kept anywhere but
//! in the helper's memory.

use crate::exec;
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

/// More files than any real EFI system partition holds.
const MAX_FILES: usize = 20_000;
/// The most that is read to know what the partition held.
const MAX_BYTES: u64 = 2 << 30;
/// The fallback directory's files are kept whole in memory to be restored:
/// a few loaders of a few MiB each.
const MAX_KEPT_BYTES: u64 = 64 << 20;

/// `a/b/c` lower-cased: FAT does not tell `EFI/Boot` from `EFI/BOOT`.
fn key(rel: &str) -> String {
    rel.to_ascii_lowercase()
}

#[derive(Clone, Debug)]
struct Kept {
    /// The path as it was found (its own case).
    rel: String,
    size: u64,
    sha256: String,
    /// The bytes of a file the install may replace.
    bytes: Option<Vec<u8>>,
}

/// What a mounted EFI system partition held.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    files: BTreeMap<String, Kept>,
    dirs: BTreeSet<String>,
}

fn is_fallback_file(rel_key: &str) -> bool {
    rel_key
        .strip_prefix("efi/boot/")
        .is_some_and(|rest| !rest.contains('/'))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn walk(
    root: &Path,
    rel: &str,
    snapshot: &mut Snapshot,
    bytes: &mut u64,
    kept: &mut u64,
    keep_fallback: bool,
) -> Result<()> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            snapshot.dirs.insert(key(&child));
            walk(root, &child, snapshot, bytes, kept, keep_fallback)?;
        } else if meta.is_file() {
            ensure!(
                snapshot.files.len() < MAX_FILES,
                "the EFI system partition holds more than {MAX_FILES} files"
            );
            *bytes += meta.len();
            ensure!(
                *bytes <= MAX_BYTES,
                "the EFI system partition holds more than {} MiB of files",
                MAX_BYTES >> 20
            );
            let content = fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            let keep = keep_fallback && is_fallback_file(&key(&child));
            if keep {
                *kept += meta.len();
                ensure!(
                    *kept <= MAX_KEPT_BYTES,
                    "the fallback loaders of the EFI system partition are larger than {} MiB",
                    MAX_KEPT_BYTES >> 20
                );
            }
            snapshot.files.insert(
                key(&child),
                Kept {
                    rel: child,
                    size: meta.len(),
                    sha256: hex(&Sha256::digest(&content)),
                    bytes: keep.then_some(content),
                },
            );
        } else {
            bail!(
                "{} is neither a file nor a directory; the EFI system partition is left alone",
                entry.path().display()
            );
        }
    }
    Ok(())
}

impl Snapshot {
    /// Read the partition mounted at `root`: every file's size and digest, and
    /// the bytes of the files under `EFI/BOOT`.
    pub fn take(root: &Path) -> Result<Self> {
        let mut snapshot = Self::default();
        let (mut bytes, mut kept) = (0, 0);
        walk(root, "", &mut snapshot, &mut bytes, &mut kept, true)?;
        Ok(snapshot)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Does the partition hold `EFI/fedora` (any case)? The plan refuses such
    /// a partition; this is the check made again where the files are written.
    pub fn has_fedora(&self) -> bool {
        self.dirs.contains("efi/fedora") || self.files.contains_key("efi/fedora")
    }

    /// After `bootupd`: put back every file of `EFI/BOOT` it replaced or
    /// removed, and require that nothing else of what was there changed and
    /// that nothing was added outside `EFI/fedora` and `EFI/BOOT`. Returns the
    /// files that were put back.
    pub fn restore_and_verify(&self, root: &Path) -> Result<Vec<String>> {
        let mut restored = Vec::new();
        for (k, file) in &self.files {
            let Some(bytes) = &file.bytes else {
                continue;
            };
            let path = root.join(&file.rel);
            if fs::read(&path).ok().as_deref() != Some(bytes.as_slice()) {
                write_over(&path, bytes).with_context(|| format!("putting back {}", file.rel))?;
                restored.push(file.rel.clone());
            }
            let _ = k;
        }
        sync_fs(root)?;
        let now = Self::take(root)?;
        for (k, before) in &self.files {
            let after = now
                .files
                .get(k)
                .with_context(|| format!("{} is gone from the EFI system partition", before.rel))?;
            ensure!(
                after.sha256 == before.sha256 && after.size == before.size,
                "{} changed on the EFI system partition",
                before.rel
            );
        }
        for k in &self.dirs {
            ensure!(
                now.dirs.contains(k),
                "the directory {k} is gone from the EFI system partition"
            );
        }
        let added = now.added_since(self);
        ensure!(
            added.is_empty(),
            "the install added files the other systems did not have outside ilex's own places: {}",
            added.join(", ")
        );
        Ok(restored)
    }

    /// Paths that were not in `before` and are neither below `EFI/fedora` nor
    /// directly in `EFI/BOOT` (nor those two directories).
    fn added_since(&self, before: &Self) -> Vec<String> {
        let allowed = |k: &str| {
            k == "efi/fedora"
                || k.starts_with("efi/fedora/")
                || k == "efi/boot"
                || is_fallback_file(k)
        };
        self.files
            .iter()
            .map(|(k, f)| (k, &f.rel))
            .chain(self.dirs.iter().map(|k| (k, k)))
            .filter(|(k, _)| {
                !before.files.contains_key(*k) && !before.dirs.contains(*k) && !allowed(k)
            })
            .map(|(_, rel)| rel.clone())
            .collect()
    }

    /// After a failed installation: remove what it added below `EFI/fedora`
    /// and to `EFI/BOOT`, and put back the fallback files it replaced. Nothing
    /// else is touched; what cannot be undone is reported.
    pub fn undo(&self, root: &Path) -> Result<()> {
        let now = Self::take(root)?;
        let mut problems = Vec::new();
        for (k, file) in &now.files {
            if self.files.contains_key(k) {
                continue;
            }
            if k.starts_with("efi/fedora/") || is_fallback_file(k) {
                if let Err(e) = fs::remove_file(root.join(&file.rel)) {
                    problems.push(format!("removing {}: {e}", file.rel));
                }
            } else {
                problems.push(format!("{} is not ilex's to remove", file.rel));
            }
        }
        // Deepest directories first.
        let mut dirs: Vec<&String> = now
            .dirs
            .iter()
            .filter(|k| !self.dirs.contains(*k) && (k.starts_with("efi/fedora")))
            .collect();
        dirs.sort_by_key(|k| std::cmp::Reverse(k.matches('/').count()));
        for k in dirs {
            // The directory's own case: find it by walking its parent.
            if let Some(path) = find_dir(root, k)
                && let Err(e) = fs::remove_dir(&path)
            {
                problems.push(format!("removing {}: {e}", path.display()));
            }
        }
        for file in self.files.values() {
            let Some(bytes) = &file.bytes else {
                continue;
            };
            let path = root.join(&file.rel);
            if fs::read(&path).ok().as_deref() != Some(bytes.as_slice())
                && let Err(e) = write_over(&path, bytes)
            {
                problems.push(format!("putting back {}: {e}", file.rel));
            }
        }
        let synced = sync_fs(root);
        ensure!(problems.is_empty(), "{}", problems.join("; "));
        synced
    }
}

/// Everything written to the file system `path` is on is on the device, as a
/// power cut a second from now would find it: what was put back is only worth
/// anything if it is durable.
pub fn sync_fs(path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let handle = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: a valid descriptor that stays open for the call.
    let done = unsafe { libc::syncfs(handle.as_raw_fd()) };
    ensure!(
        done == 0,
        "syncfs of {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
    Ok(())
}

/// The directory whose lower-cased relative path is `k`, found by name.
fn find_dir(root: &Path, k: &str) -> Option<PathBuf> {
    let mut path = root.to_path_buf();
    for part in k.split('/') {
        let found = fs::read_dir(&path).ok()?.flatten().find(|e| {
            e.file_name().to_string_lossy().to_ascii_lowercase() == part
                && e.file_type().is_ok_and(|t| t.is_dir())
        })?;
        path = found.path();
    }
    Some(path)
}

/// Write `bytes` as a new file beside `path` and rename it over: a reset
/// leaves the old file or the new one, never half of either.
fn write_over(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("a path with no directory")?;
    let temporary = parent.join(".ilex-restore.tmp");
    let _ = fs::remove_file(&temporary);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, path)?;
    Ok(())
}

// --- the firmware's boot entries ----------------------------------------------

/// Where the kernel shows the firmware's variables.
pub const EFIVARS: &str = "/sys/firmware/efi/efivars";
/// The vendor GUID of the boot variables (`EFI_GLOBAL_VARIABLE`).
const GLOBAL_GUID: &str = "8be4df61-93ca-11d2-aa0d-00e098032b8c";
const MAX_VAR: u64 = 64 << 10;

/// `Boot0001`, `Boot000A`: the name of one boot entry, without its GUID.
fn is_entry_name(stem: &str) -> bool {
    stem.len() == 8
        && stem.starts_with("Boot")
        && stem[4..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
}

/// A GUID as the firmware stores it (the first three groups little-endian).
fn guid_bytes(text: &str) -> Option<[u8; 16]> {
    let digits: String = text.chars().filter(|c| *c != '-').collect();
    if digits.len() != 32 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut raw = [0u8; 16];
    for (i, byte) in raw.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).ok()?;
    }
    raw[0..4].reverse();
    raw[4..6].reverse();
    raw[6..8].reverse();
    Some(raw)
}

/// The partition GUID of the shared EFI system partition, as the entries'
/// device paths hold it.
pub fn esp_guid(partuuid: &str) -> Result<[u8; 16]> {
    guid_bytes(partuuid).with_context(|| {
        format!("the shared EFI system partition's PARTUUID {partuuid:?} is not a GUID")
    })
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
}

/// Does `raw` hold `needle` as UTF-16LE text, ASCII case folded?
fn utf16_contains_ci(raw: &[u8], needle: &str) -> bool {
    let needle: Vec<u16> = needle.encode_utf16().map(ascii_lower).collect();
    (0..2).any(|start| {
        let units: Vec<u16> = le_units(&raw[start.min(raw.len())..])
            .into_iter()
            .map(ascii_lower)
            .collect();
        units.windows(needle.len()).any(|w| w == needle.as_slice())
    })
}

/// The little-endian 16-bit units of `raw` (a trailing odd byte is dropped).
fn le_units(raw: &[u8]) -> Vec<u16> {
    raw.as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect()
}

fn ascii_lower(unit: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
        unit + 32
    } else {
        unit
    }
}

/// The label of an entry: efivarfs' four bytes of the variable's attributes,
/// then the `EFI_LOAD_OPTION` (its own four bytes of attributes, the file path
/// list's length, the description as UTF-16).
fn entry_label(raw: &[u8]) -> String {
    let units: Vec<u16> = le_units(raw.get(10..).unwrap_or_default())
        .into_iter()
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

/// Is this boot entry one `bootupd` made for ilex's own files: it starts
/// `\EFI\fedora\...` on the shared EFI system partition (the partition GUID
/// is in its device path)? Another Fedora's entry has the same loader path and
/// another disk's GUID, and is never taken for ilex's.
fn is_ilex_entry(raw: &[u8], esp: &[u8; 16]) -> bool {
    contains_bytes(raw, esp) && utf16_contains_ci(raw, "\\efi\\fedora\\")
}

fn variable_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}-{GLOBAL_GUID}"))
}

/// A variable file may be immutable (efivarfs makes them so); that is cleared
/// before it is changed or removed.
fn clear_immutable(path: &Path) {
    let _ = exec::run(
        "chattr",
        &["-i", &path.to_string_lossy()],
        Duration::from_secs(10),
    );
}

fn write_variable(dir: &Path, stem: &str, raw: &[u8]) -> Result<()> {
    let path = variable_path(dir, stem);
    if path.exists() {
        // As libefivar replaces a variable: the old one goes, a new one is made.
        clear_immutable(&path);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    // The attributes and the data in one write, as efivarfs requires.
    file.write_all(raw)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn delete_variable(dir: &Path, stem: &str) -> Result<()> {
    let path = variable_path(dir, stem);
    if path.exists() {
        clear_immutable(&path);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

/// The firmware's boot variables as they are, each with its exact bytes
/// (the four bytes of attributes, then the data): every `Boot####` and
/// `BootOrder`. Saving them before `bootupd` and writing back what it changed
/// puts another system's entry back whatever it pointed at: nothing about the
/// entry needs to exist any more, and nothing but the bytes is relied on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Nvram {
    vars: BTreeMap<String, Vec<u8>>,
}

/// What [`Nvram::restore`] did.
#[derive(Clone, Debug, Default)]
pub struct Restored {
    /// One sentence each.
    pub lines: Vec<String>,
}

impl Nvram {
    /// The variables of the firmware now. Where there are none to read (a
    /// machine with no `efivarfs`) the answer is empty.
    pub fn read() -> Result<Self> {
        Self::read_from(Path::new(EFIVARS))
    }

    pub fn read_from(dir: &Path) -> Result<Self> {
        let mut vars = BTreeMap::new();
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("reading {}", dir.display())));
            }
        };
        let suffix = format!("-{GLOBAL_GUID}");
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(&suffix) else {
                continue;
            };
            if stem != "BootOrder" && !is_entry_name(stem) {
                continue;
            }
            let meta = fs::symlink_metadata(entry.path())?;
            ensure!(
                meta.is_file() && meta.len() <= MAX_VAR,
                "the firmware variable {stem} is not a plain file of a sensible size"
            );
            let raw = fs::read(entry.path())
                .with_context(|| format!("reading {}", entry.path().display()))?;
            vars.insert(stem.to_owned(), raw);
        }
        Ok(Self { vars })
    }

    pub fn vars(&self) -> &BTreeMap<String, Vec<u8>> {
        &self.vars
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// The entry numbers `BootOrder` lists, as variable names (`Boot0001`).
    pub fn order(&self) -> Vec<String> {
        self.vars
            .get("BootOrder")
            .map(|raw| {
                le_units(raw.get(4..).unwrap_or_default())
                    .into_iter()
                    .map(|n| format!("Boot{n:04X}"))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A line for the log about each entry: its number, label and size.
    pub fn describe(&self) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(name, _)| is_entry_name(name))
            .map(|(name, raw)| {
                format!(
                    "{name} {:?}, {} bytes, sha256 {}",
                    entry_label(raw),
                    raw.len(),
                    hex(&Sha256::digest(raw))
                )
            })
            .collect()
    }

    /// The entries that `bootupd` made for ilex: the entries of `self` that
    /// point at the shared EFI system partition's `EFI/fedora` and that are
    /// not byte for byte one of `saved`'s.
    pub fn ilex_entries(&self, saved: &Self, esp: &[u8; 16]) -> Vec<String> {
        self.vars
            .iter()
            .filter(|(name, raw)| {
                is_entry_name(name)
                    && is_ilex_entry(raw, esp)
                    && !saved.vars.values().any(|v| v == *raw)
            })
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Write back what `self` (the variables as they were) has and the
    /// firmware does not, or has changed, at the same name and with the same
    /// bytes; make sure none of ilex's entries sits where one of them was
    /// (`keep_ilex`: it moves to a number that is free; otherwise it is
    /// removed); and put `BootOrder` back as the saved bytes.
    pub fn restore(&self, dir: &Path, esp: &[u8; 16], keep_ilex: bool) -> Result<Restored> {
        let mut out = Restored::default();
        let now = Self::read_from(dir)?;
        let ilex = now.ilex_entries(self, esp);
        let mut taken: BTreeSet<String> =
            self.vars.keys().chain(now.vars.keys()).cloned().collect();
        let mut current = now.clone();
        for name in &ilex {
            if keep_ilex {
                if self.vars.contains_key(name) {
                    let free = (0..=0xFFFFu32)
                        .map(|n| format!("Boot{n:04X}"))
                        .find(|n| !taken.contains(n))
                        .context("the firmware has no free boot entry number")?;
                    write_variable(dir, &free, &now.vars[name])?;
                    out.lines.push(format!(
                        "ilex's firmware entry {name} sat where another entry was; it is {free} now"
                    ));
                    taken.insert(free.clone());
                    current.vars.insert(free, now.vars[name].clone());
                }
            } else {
                delete_variable(dir, name)?;
                current.vars.remove(name);
                out.lines
                    .push(format!("removed ilex's firmware entry {name}"));
            }
        }
        for (name, raw) in self.vars.iter().filter(|(n, _)| is_entry_name(n)) {
            if current.vars.get(name) == Some(raw) {
                continue;
            }
            let was = if current.vars.contains_key(name) {
                "changed"
            } else {
                "missing"
            };
            write_variable(dir, name, raw)?;
            current.vars.insert(name.clone(), raw.clone());
            out.lines.push(format!(
                "the firmware entry {name} ({:?}) was {was}; it is back, byte for byte",
                entry_label(raw)
            ));
        }
        match (self.vars.get("BootOrder"), current.vars.get("BootOrder")) {
            (Some(saved), Some(now)) if saved == now => {}
            (Some(saved), _) => {
                write_variable(dir, "BootOrder", saved)?;
                out.lines
                    .push("BootOrder is back as it was, byte for byte".to_owned());
            }
            (None, Some(_)) => {
                delete_variable(dir, "BootOrder")?;
                out.lines
                    .push("BootOrder did not exist before; it is removed again".to_owned());
            }
            (None, None) => {}
        }
        Ok(out)
    }

    /// After a restore: every saved variable is there with the saved bytes.
    pub fn verify_restored(&self, dir: &Path) -> Result<()> {
        let now = Self::read_from(dir)?;
        for (name, raw) in &self.vars {
            ensure!(
                now.vars.get(name) == Some(raw),
                "the firmware variable {name} is not as it was saved"
            );
        }
        Ok(())
    }

    /// The last stage of an installation: ilex's entries first, then the order
    /// the firmware had. Returns the order that was set.
    pub fn put_ilex_first(&self, dir: &Path, esp: &[u8; 16]) -> Result<Option<String>> {
        let now = Self::read_from(dir)?;
        let ilex = now.ilex_entries(self, esp);
        if ilex.is_empty() {
            return Ok(None);
        }
        let attributes = self
            .vars
            .get("BootOrder")
            .or_else(|| now.vars.get("BootOrder"))
            .map_or(vec![0x07, 0, 0, 0], |raw| raw[..4].to_vec());
        let mut list = ilex;
        for n in now.order() {
            if !list.contains(&n) {
                list.push(n);
            }
        }
        write_variable(dir, "BootOrder", &order_bytes(&attributes, &list))?;
        Ok(Some(list.join(",")))
    }
}

fn order_bytes(attributes: &[u8], list: &[String]) -> Vec<u8> {
    let mut raw = attributes.to_vec();
    for name in list {
        if let Some(n) = name
            .strip_prefix("Boot")
            .and_then(|h| u16::from_str_radix(h, 16).ok())
        {
            raw.extend_from_slice(&n.to_le_bytes());
        }
    }
    raw
}

/// One entry of the firmware's boot menu as `efibootmgr -v` lists it: for the
/// log, never for deciding anything.
pub fn boot_entry_lines() -> Vec<String> {
    exec::run("efibootmgr", &["--verbose"], Duration::from_secs(30))
        .ok()
        .filter(exec::Output::success)
        .map(|o| {
            o.stdout_text()
                .lines()
                .filter(|l| l.starts_with("Boot"))
                .map(|l| l.trim().to_owned())
                .collect()
        })
        .unwrap_or_default()
}
