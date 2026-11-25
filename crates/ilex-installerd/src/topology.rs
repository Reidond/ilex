//! What the running system is using, down to the physical disks.
//!
//! One rule governs this module: **a disk is offered as a target only if every
//! current use of block storage has been positively traced and none reaches it.**
//! The uses are the three the kernel lets this process see: every mount (the
//! live medium's or not), every swap entry and every bound loop device (mounted
//! or not). Each is followed to whole disks through the kernel's own tree
//! (never the text of a mount's source, nor a path's prefix):
//!
//! * a device number (`/sys/dev/block/M:m`) or a device node to a kernel name;
//! * a partition to its disk, a stacked device (device-mapper, MD) down through
//!   `slaves/`;
//! * a loop device to its backing file as the kernel records it (`lo_device`,
//!   `lo_inode`, from `losetup --json --list`): the device number names the
//!   device the file's file system is on, and a backing file that is a block
//!   device node (a loop on `/dev/sdb1`) is followed to the device it is,
//!   once its `(dev, ino)` matches what the kernel recorded;
//! * a multi-device btrfs to every member (`/sys/fs/btrfs/<uuid>/devices/`).
//!
//! A mount is *not* block-backed only if its file system type is on an explicit
//! list of kernel-interface, memory, layered and network file systems; any
//! other type has to be traced, and a use that cannot be traced is reported as
//! `unresolved`, which makes every disk in use. Nothing is guessed.

use anyhow::{Context, Result};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

/// Where sysfs and the device nodes are. Production uses `/sys` and `/dev`;
/// the self-check builds a tree of the same shape.
#[derive(Clone, Debug)]
pub struct Roots {
    pub sys: PathBuf,
    pub dev: PathBuf,
}

impl Roots {
    pub fn real() -> Self {
        Self {
            sys: "/sys".into(),
            dev: "/dev".into(),
        }
    }
}

/// A device number: major and minor.
pub type DevNum = (u32, u32);

/// `st_dev` and `st_rdev` as glibc's `major()` and `minor()` split them.
pub fn split_dev(dev: u64) -> DevNum {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major as u32, minor as u32)
}

/// What `stat` says of a file: the device its file system is on, its inode,
/// and, for a block device node, the device it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    pub dev: DevNum,
    pub ino: u64,
    /// `Some` only for a block special file.
    pub block_device: Option<DevNum>,
}

/// `stat` of a path from this process's own mount namespace; `None` when it
/// cannot be read from here (not there, unreachable, unlinked, in another
/// namespace).
pub fn stat_file(path: &str) -> Option<FileId> {
    let meta = fs::metadata(path).ok()?;
    Some(FileId {
        dev: split_dev(meta.dev()),
        ino: meta.ino(),
        block_device: meta
            .file_type()
            .is_block_device()
            .then(|| split_dev(meta.rdev())),
    })
}

/// One line of `/proc/self/mountinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub major: u32,
    pub minor: u32,
    pub mount_point: String,
    pub fstype: String,
    pub source: String,
    /// The file system's own options (`logdev=/dev/sdb2`).
    pub options: String,
}

/// mountinfo writes a space, tab, newline and backslash in a path as `\040`,
/// `\011`, `\012` and `\134`.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let value =
                (bytes[i + 1] - b'0') * 64 + (bytes[i + 2] - b'0') * 8 + (bytes[i + 3] - b'0');
            out.push(value);
            i += 4;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            // `id parent major:minor root mount-point options [optional...] - fstype source super-options`
            let (head, tail) = line.split_once(" - ")?;
            let head: Vec<&str> = head.split(' ').collect();
            let tail: Vec<&str> = tail.split(' ').collect();
            let (major, minor) = head.get(2)?.split_once(':')?;
            Some(Mount {
                major: major.parse().ok()?,
                minor: minor.parse().ok()?,
                mount_point: unescape(head.get(4)?),
                fstype: (*tail.first()?).to_owned(),
                source: unescape(tail.get(1)?),
                options: tail.get(2).map_or_else(String::new, |o| (*o).to_owned()),
            })
        })
        .collect()
}

/// The files and devices `/proc/swaps` lists (its first column).
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|line| line.split_whitespace().next())
        .map(unescape)
        .collect()
}

/// A bound loop device as `losetup --json --list --output
/// NAME,BACK-FILE,BACK-INO,BACK-MAJ:MIN` reports it: the kernel's own record of
/// the file (`lo_inode`) and the device its file system is on (`lo_device`,
/// which is `st_dev` of the file: a block device node backing a loop shows the
/// device of `/dev`, not the device it stands for). An empty column is `None`:
/// unknown, not "nothing".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopRow {
    /// `/dev/loop0`.
    pub name: String,
    pub file: Option<String>,
    pub inode: Option<u64>,
    pub device: Option<DevNum>,
}

/// The columns [`parse_losetup`] reads.
pub const LOSETUP_ARGS: [&str; 4] = [
    "--json",
    "--list",
    "--output",
    "NAME,BACK-FILE,BACK-INO,BACK-MAJ:MIN",
];

fn json_text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

/// The rows of `losetup`'s JSON. Anything that is not that document is an
/// error: an unreadable table must never look like one with no loop devices.
pub fn parse_losetup(json: &str) -> Result<Vec<LoopRow>> {
    let root: Value = serde_json::from_str(json).context("reading losetup")?;
    let rows = root["loopdevices"]
        .as_array()
        .context("losetup has no loopdevices")?;
    rows.iter()
        .map(|row| {
            Ok(LoopRow {
                name: json_text(&row["name"]).context("a loop device without a name")?,
                file: json_text(&row["back-file"]),
                inode: match &row["back-ino"] {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => s.trim().parse().ok(),
                    _ => None,
                },
                device: row["back-maj:min"]
                    .as_str()
                    .and_then(|s| s.split_once(':'))
                    .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?))),
            })
        })
        .collect()
}

/// What the kernel says the system is using, read once: the mount table, the
/// swap areas and the bound loop devices.
#[derive(Clone, Debug, Default)]
pub struct System {
    pub mounts: Vec<Mount>,
    pub swaps: Vec<String>,
    pub loops: Vec<LoopRow>,
}

/// The mount table and the swap areas, read or an error: a table that cannot
/// be read must never look like one with nothing in it, which would let every
/// disk pass as unused. A system with no `/proc/swaps` at all (a kernel built
/// without swap) has no swap areas.
pub fn read_tables(mountinfo: &Path, swaps: &Path) -> Result<(Vec<Mount>, Vec<String>)> {
    let mounts = fs::read(mountinfo).with_context(|| format!("reading {}", mountinfo.display()))?;
    let swaps = match fs::read(swaps) {
        Ok(bytes) => parse_swaps(&String::from_utf8_lossy(&bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => vec![],
        Err(e) => return Err(e).with_context(|| format!("reading {}", swaps.display())),
    };
    Ok((parse_mountinfo(&String::from_utf8_lossy(&mounts)), swaps))
}

/// File systems with no block device behind them: in memory, a window onto
/// the kernel, layered over directories of other mounts (each of which is
/// traced on its own), a desktop session's user-space services, or on another
/// computer. **Everything else is traced to a device or is unresolved.**
/// `devtmpfs` is here as a mount (mounting `/dev` uses no disk) but never as
/// the place of a file: it holds device nodes.
const NO_BLOCK_STORAGE: &[&str] = &[
    // memory
    "tmpfs",
    "ramfs",
    "rootfs",
    "hugetlbfs",
    // the kernel's interfaces
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "securityfs",
    "selinuxfs",
    "cgroup",
    "cgroup2",
    "debugfs",
    "tracefs",
    "configfs",
    "pstore",
    "bpf",
    "mqueue",
    "binfmt_misc",
    "fusectl",
    "autofs",
    "efivarfs",
    "nsfs",
    "rpc_pipefs",
    "nfsd",
    // layers of directories on other mounts
    "overlay",
    // user-space services of the desktop session
    "fuse.portal",
    "fuse.gvfsd-fuse",
    // another computer
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "9p",
    "virtiofs",
    "ceph",
    "afs",
    "vboxsf",
];

/// The subset where a file's bytes are in memory or on another computer, so a
/// loop device backed by one holds no local disk. Not `overlay` (its bytes are
/// in its layers), `devtmpfs` (it holds device nodes) or the kernel interfaces.
const FILES_NOT_ON_A_DISK: &[&str] = &[
    "tmpfs",
    "ramfs",
    "rootfs",
    "hugetlbfs",
    "nfs",
    "nfs4",
    "cifs",
    "smb3",
    "9p",
    "virtiofs",
    "ceph",
    "afs",
    "vboxsf",
];

/// A mount that belongs to the live system's own medium: dracut's
/// `/run/initramfs/live` and `/run/initramfs/isoscan` (the disk an ISO file is
/// found on), the root image's squashfs at `/run/rootfsbase`, any optical-style
/// file system, and a root made of dracut's `live-*` device-mapper devices.
pub fn is_live_mount(mount: &Mount) -> bool {
    let under = |dir: &str| {
        mount.mount_point == dir
            || mount
                .mount_point
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    under("/run/initramfs")
        || under("/run/rootfsbase")
        || ["iso9660", "udf"].contains(&mount.fstype.as_str())
        || (mount.mount_point == "/" && mount.source.starts_with("/dev/mapper/live-"))
}

/// Everything a use of block storage is made of.
#[derive(Debug, Default)]
pub struct Reach {
    /// Kernel names: the device, its disk, what is stacked on it, the loop
    /// devices' backing devices, a btrfs's other members, transitively.
    pub names: BTreeSet<String>,
    /// What could not be traced.
    pub unresolved: Vec<String>,
}

/// How far a chain of stacked devices is followed.
const DEPTH: u32 = 16;

pub struct Tracer<'a> {
    roots: &'a Roots,
    system: &'a System,
    /// `stat`; the self-check supplies its own.
    stat: &'a dyn Fn(&str) -> Option<FileId>,
}

impl<'a> Tracer<'a> {
    pub fn new(roots: &'a Roots, system: &'a System) -> Self {
        Self {
            roots,
            system,
            stat: &stat_file,
        }
    }

    pub fn with_stat(mut self, stat: &'a dyn Fn(&str) -> Option<FileId>) -> Self {
        self.stat = stat;
        self
    }

    /// `/sys/class/block/<name>` with its links followed: the device's own
    /// directory, below its parent disk's when it is a partition.
    fn class_path(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return None;
        }
        fs::canonicalize(self.roots.sys.join("class/block").join(name)).ok()
    }

    /// The kernel name of the block device with this number (sysfs).
    fn device_of_number(&self, (major, minor): DevNum) -> Option<String> {
        let link = self
            .roots
            .sys
            .join("dev/block")
            .join(format!("{major}:{minor}"));
        fs::canonicalize(link)
            .ok()
            .and_then(|p| Some(p.file_name()?.to_str()?.to_owned()))
    }

    /// The kernel name behind a device node (`/dev/mapper/x`,
    /// `/dev/disk/by-label/x`, `/dev/loop0`), following its links.
    pub fn device_of_node(&self, path: &str) -> Option<String> {
        let relative = path.strip_prefix("/dev/")?;
        if relative.split('/').any(|part| part == "..") {
            return None;
        }
        let resolved = fs::canonicalize(self.roots.dev.join(relative)).ok()?;
        let name = resolved.file_name()?.to_str()?.to_owned();
        self.class_path(&name).map(|_| name)
    }

    /// The device a mount's source names: a device node, or `UUID=`, `LABEL=`,
    /// `PARTUUID=`, `PARTLABEL=` through `/dev/disk/by-*`.
    fn device_of_source(&self, source: &str) -> Option<String> {
        if source.starts_with("/dev/") {
            return self.device_of_node(source);
        }
        let (key, value) = source.split_once('=')?;
        if value.is_empty() || value.contains('/') {
            return None;
        }
        let directory = match key {
            "UUID" => "by-uuid",
            "LABEL" => "by-label",
            "PARTUUID" => "by-partuuid",
            "PARTLABEL" => "by-partlabel",
            _ => return None,
        };
        self.device_of_node(&format!("/dev/disk/{directory}/{value}"))
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// The devices stacked on `name` (`holders/`): an LVM volume, an MD array
    /// or a dm-crypt mapping built on it, open now.
    pub fn holders(&self, name: &str) -> Vec<String> {
        self.class_path(name)
            .map(|dir| Self::entries(&dir.join("holders")))
            .unwrap_or_default()
    }

    /// Follow a device to everything it is made of: its disk, what it stacks
    /// on and, for a loop device, the file it is attached to and what that is
    /// on.
    pub fn device(&self, name: &str, depth: u32, reach: &mut Reach) {
        if !reach.names.insert(name.to_owned()) {
            return;
        }
        if depth > DEPTH {
            reach
                .unresolved
                .push(format!("{name} is stacked too deeply to follow"));
            return;
        }
        let Some(dir) = self.class_path(name) else {
            reach.unresolved.push(format!("{name} is not in sysfs"));
            return;
        };
        // A partition is its disk's; the disk carries the rest.
        if dir.join("partition").exists() {
            if let Some(parent) = dir
                .parent()
                .and_then(Path::file_name)
                .and_then(|n| n.to_str())
            {
                self.device(parent, depth + 1, reach);
            }
            return;
        }
        for slave in Self::entries(&dir.join("slaves")) {
            self.device(&slave, depth + 1, reach);
        }
        if dir.join("loop").is_dir() {
            self.loop_device(name, &dir, depth, reach);
        }
    }

    fn number(&self, number: DevNum, what: &str, depth: u32, reach: &mut Reach) {
        match self.device_of_number(number) {
            Some(name) => self.device(&name, depth + 1, reach),
            None => reach.unresolved.push(format!(
                "{what}, on device {}:{}, which sysfs does not list",
                number.0, number.1
            )),
        }
    }

    /// Whether the kernel says this loop device is attached to a file.
    fn bound_in_sysfs(dir: &Path) -> io::Result<bool> {
        match fs::read(dir.join("loop/backing_file")) {
            Ok(bytes) => Ok(!bytes.iter().all(u8::is_ascii_whitespace)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// A loop device is whatever its backing file is on, from the kernel's
    /// record of that file and not from the file's name.
    fn loop_device(&self, name: &str, dir: &Path, depth: u32, reach: &mut Reach) {
        let bound = match Self::bound_in_sysfs(dir) {
            Ok(bound) => bound,
            Err(e) => {
                reach
                    .unresolved
                    .push(format!("the backing file of {name} cannot be read: {e}"));
                return;
            }
        };
        let node = format!("/dev/{name}");
        let Some(row) = self.system.loops.iter().find(|r| r.name == node) else {
            if bound {
                reach.unresolved.push(format!(
                    "{name} is attached to a file, but losetup does not list it"
                ));
            }
            return;
        };
        let (Some(file), Some(inode), Some(device)) = (&row.file, row.inode, row.device) else {
            reach.unresolved.push(format!(
                "{name} is attached to a file the kernel does not identify (losetup shows no file name, inode or device)"
            ));
            return;
        };
        self.backing_file(name, file, inode, device, depth, reach);
    }

    /// Where a loop device's file is, by the kernel's `(device, inode)` and
    /// the file name only to confirm it:
    ///
    /// * a number with a major names the block device the file system is on;
    /// * an anonymous number is the mount that carries it: memory or a remote
    ///   file system holds no disk, a block-backed one is traced, `overlay`
    ///   and the kernel interfaces are not places a file can be traced from,
    ///   and `devtmpfs` holds device nodes, so it counts only for a file that
    ///   is verified to be one (below);
    /// * a btrfs subvolume's own number appears in no mount table: the file
    ///   is then found by its name, confirmed by `(dev, ino)`, and the btrfs
    ///   mount that contains it is traced;
    /// * a file that is a block device node is also the device it stands for
    ///   (its `st_rdev`), once `(dev, ino)` match what the kernel recorded.
    fn backing_file(
        &self,
        name: &str,
        file: &str,
        inode: u64,
        device: DevNum,
        depth: u32,
        reach: &mut Reach,
    ) {
        let path = file.strip_suffix(" (deleted)").unwrap_or(file);
        let confirmed = (self.stat)(path).filter(|id| id.dev == device && id.ino == inode);
        let node = confirmed.and_then(|id| id.block_device);
        let what = format!("{name} is backed by {file}");
        let mut explained = false;
        if device.0 != 0 {
            self.number(device, &what, depth, reach);
            explained = true;
        } else {
            for mount in self
                .system
                .mounts
                .iter()
                .filter(|m| (m.major, m.minor) == device)
            {
                if FILES_NOT_ON_A_DISK.contains(&mount.fstype.as_str()) {
                    explained = true;
                } else if mount.fstype == "devtmpfs" {
                    explained |= node.is_some();
                } else if !NO_BLOCK_STORAGE.contains(&mount.fstype.as_str()) {
                    explained = true;
                    self.mount(mount, depth + 1, reach);
                }
            }
            if !explained && confirmed.is_some() {
                // The name is the file's (its device and inode match what the
                // kernel recorded), so it is reached through the mounts that
                // contain it; if the one on top is btrfs, so is the file, and
                // every btrfs on the way is traced.
                let chain = self.mounts_containing(path);
                if chain.last().is_some_and(|m| m.fstype == "btrfs") {
                    explained = true;
                    for mount in chain.into_iter().filter(|m| m.fstype == "btrfs") {
                        self.mount(mount, depth + 1, reach);
                    }
                }
            }
        }
        if let Some(node) = node {
            self.number(node, &format!("{what} (a device node)"), depth, reach);
            explained = true;
        }
        if !explained {
            reach.unresolved.push(format!(
                "{what}, on device {}:{}, a file system the installer cannot trace to a disk",
                device.0, device.1
            ));
        }
    }

    /// The mounts whose mount point contains `path`, shortest first (so the one
    /// on top is last). Used only for a file whose `(dev, ino)` were confirmed,
    /// so the name is known to be the file's.
    fn mounts_containing(&self, path: &str) -> Vec<&Mount> {
        let mut chain: Vec<&Mount> = self
            .system
            .mounts
            .iter()
            .filter(|m| {
                m.mount_point == "/"
                    || path == m.mount_point
                    || path
                        .strip_prefix(m.mount_point.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .collect();
        chain.sort_by_key(|m| m.mount_point.len());
        chain
    }

    /// Every device of a btrfs file system, from `/sys/fs/btrfs/<uuid>/devices/`:
    /// the file systems that list one of `devices` or have this UUID.
    fn btrfs_members(&self, devices: &[String], uuid: Option<&str>) -> Option<BTreeSet<String>> {
        let base = self.roots.sys.join("fs/btrfs");
        let mut members = BTreeSet::new();
        let mut found = false;
        for fsid in Self::entries(&base) {
            let listed = Self::entries(&base.join(&fsid).join("devices"));
            if listed.is_empty() {
                continue;
            }
            if uuid == Some(fsid.as_str()) || devices.iter().any(|d| listed.contains(d)) {
                found = true;
                members.extend(listed);
            }
        }
        found.then_some(members)
    }

    /// The name this mount is shown by: its device, else its source.
    fn mount_label(&self, mount: &Mount) -> String {
        let named = (mount.major != 0)
            .then(|| self.device_of_number((mount.major, mount.minor)))
            .flatten()
            .or_else(|| self.device_of_source(&mount.source));
        named.map_or_else(|| mount.source.clone(), |n| format!("/dev/{n}"))
    }

    /// Trace a mount to the devices it is on, or say why it cannot be. A file
    /// system on the explicit list holds no block storage of its own; any
    /// other has to be found on a device by its number, its source, its btrfs
    /// members or the devices its options name.
    pub fn mount(&self, mount: &Mount, depth: u32, reach: &mut Reach) {
        if NO_BLOCK_STORAGE.contains(&mount.fstype.as_str()) {
            return;
        }
        let at = &mount.mount_point;
        let mut found: Vec<String> = Vec::new();
        if mount.major != 0 {
            match self.device_of_number((mount.major, mount.minor)) {
                Some(name) => found.push(name),
                None => {
                    reach.unresolved.push(format!(
                        "{} (mounted at {at}) is on device {}:{}, which sysfs does not list",
                        mount.source, mount.major, mount.minor
                    ));
                    return;
                }
            }
        }
        if let Some(name) = self.device_of_source(&mount.source)
            && !found.contains(&name)
        {
            found.push(name);
        }
        let mut traced = !found.is_empty();
        let mut devices: BTreeSet<String> = found.iter().cloned().collect();
        if mount.fstype == "btrfs" {
            let uuid = mount.source.strip_prefix("UUID=");
            match self.btrfs_members(&found, uuid) {
                Some(members) => {
                    traced = true;
                    devices.extend(members);
                }
                None => {
                    reach.unresolved.push(format!(
                        "the btrfs file system {} (mounted at {at}) lists no devices in sysfs",
                        mount.source
                    ));
                    return;
                }
            }
        }
        // Devices the file system uses besides its own: an external log or
        // journal, a realtime device.
        for option in mount.options.split(',') {
            let Some((key, value)) = option.split_once('=') else {
                continue;
            };
            if !["logdev", "rtdev", "journal_path", "journal_dev"].contains(&key) {
                continue;
            }
            match self.device_of_source(value) {
                Some(name) => {
                    devices.insert(name);
                }
                None => reach.unresolved.push(format!(
                    "{} (mounted at {at}) uses {key}={value}, a device the installer cannot find",
                    mount.source
                )),
            }
        }
        if !traced {
            reach.unresolved.push(format!(
                "{} (a {} file system mounted at {at}) is not on any block device the installer can find",
                mount.source, mount.fstype
            ));
            return;
        }
        for name in devices {
            self.device(&name, depth + 1, reach);
        }
    }

    /// Kernel names of the loop devices that are attached to a file: those
    /// losetup lists and those sysfs says are, whether or not either knows of
    /// the other.
    fn attached_loops(&self) -> BTreeSet<String> {
        let mut names: BTreeSet<String> = self
            .system
            .loops
            .iter()
            .filter_map(|r| r.name.strip_prefix("/dev/").map(str::to_owned))
            .collect();
        for name in Self::entries(&self.roots.sys.join("class/block")) {
            let is_loop = name
                .strip_prefix("loop")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if is_loop
                && self
                    .class_path(&name)
                    .is_some_and(|dir| Self::bound_in_sysfs(&dir).unwrap_or(true))
            {
                names.insert(name);
            }
        }
        names
    }

    /// Trace every use of block storage the system has.
    pub fn usage(&self) -> Usage {
        let mut usage = Usage::default();
        let mut reached = BTreeSet::new();
        for mount in &self.system.mounts {
            let mut reach = Reach::default();
            self.mount(mount, 0, &mut reach);
            usage.unresolved.append(&mut reach.unresolved);
            // A root made of loop devices is the live image's.
            let live = is_live_mount(mount)
                || (mount.mount_point == "/" && reach.names.iter().any(|n| n.starts_with("loop")));
            reached.extend(reach.names.iter().cloned());
            if live {
                usage.live.extend(reach.names);
            } else if !reach.names.is_empty() {
                let reason = format!(
                    "{} is mounted at {}",
                    self.mount_label(mount),
                    mount.mount_point
                );
                for name in reach.names {
                    usage.in_use.entry(name).or_default().insert(reason.clone());
                }
            }
        }
        for swap in &self.system.swaps {
            let mut reach = Reach::default();
            // A swap file cannot be placed from what the kernel lists (its
            // name only): it is on some block file system, which one is not
            // known, so no disk can be shown not to hold it.
            match swap
                .starts_with("/dev/")
                .then(|| self.device_of_node(swap))
                .flatten()
            {
                Some(name) => self.device(&name, 0, &mut reach),
                None if swap.starts_with("/dev/") => reach.unresolved.push(format!(
                    "the swap area {swap} is not a block device the installer can find"
                )),
                None => reach.unresolved.push(format!(
                    "the swap file {swap} is on a file system the installer cannot identify"
                )),
            }
            usage.unresolved.append(&mut reach.unresolved);
            reached.extend(reach.names.iter().cloned());
            for name in reach.names {
                usage
                    .in_use
                    .entry(name)
                    .or_default()
                    .insert(format!("{swap} is in use as swap"));
            }
        }
        // A loop device is a use of its file's disk whether or not it is
        // mounted: attached to a file, it holds the file open. Those the live
        // medium's mounts or another mount already reached are accounted for.
        for name in self.attached_loops() {
            if reached.contains(&name) || usage.live.contains(&name) {
                continue;
            }
            let mut reach = Reach::default();
            self.device(&name, 0, &mut reach);
            usage.unresolved.append(&mut reach.unresolved);
            let file = self
                .system
                .loops
                .iter()
                .find(|r| r.name == format!("/dev/{name}"))
                .and_then(|r| r.file.clone())
                .unwrap_or_else(|| "a file".into());
            for reached_name in reach.names {
                // The live medium is already not a target, and says so.
                if usage.live.contains(&reached_name) {
                    continue;
                }
                usage
                    .in_use
                    .entry(reached_name)
                    .or_default()
                    .insert(format!("/dev/{name} is attached to {file}"));
            }
        }
        usage
    }
}

/// How the running system uses the disks.
#[derive(Debug, Default)]
pub struct Usage {
    /// Kernel names (disks, partitions, stacked devices) of the live medium.
    pub live: BTreeSet<String>,
    /// Kernel names touched by anything else in use, with what uses them.
    pub in_use: BTreeMap<String, BTreeSet<String>>,
    /// What could not be traced: then no disk can be shown not to be used, and
    /// every disk is reported in use.
    pub unresolved: Vec<String>,
}
