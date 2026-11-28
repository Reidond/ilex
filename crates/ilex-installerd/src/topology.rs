//! What the running system is using, down to the physical disks.
//!
//! One rule governs this module: **a disk is offered as a target only if every
//! current use of block storage has been positively traced and none reaches it.**
//! The uses are the ones the kernel lets this process see: every mount of every
//! mount namespace (the live medium's or not), every swap entry, every bound loop
//! device (mounted or not), every zram writeback device and every external file
//! system journal. Each is followed to whole disks through the kernel's own tree
//! (never the text of a mount's source, nor a path's prefix):
//!
//! * a device number (`/sys/dev/block/M:m`) to a kernel name; a device node is
//!   the device its `st_rdev` says, not the one its name suggests;
//! * a partition to its disk, a stacked device (device-mapper, MD) down through
//!   `slaves/`;
//! * a loop device to its backing file as the kernel records it (`lo_device`,
//!   `lo_inode`, from `losetup --json --list`): the device number names the
//!   device the file's file system is on. The file itself is looked for by its
//!   name and, since the kernel prints a name relative to its file system for
//!   the files dracut opened, below the mounts of that device: a path that
//!   `stat`s to exactly the recorded numbers is the file, and says whether it
//!   is a block device node. A file that no path reaches (deleted, hidden by a
//!   mount, its file system's mounts detached) might still be a block device
//!   node, which no number says; it is accepted only where a node could not
//!   have been opened (a `nodev` mount) or cannot exist (a read-only image file
//!   system), and otherwise the use is unresolved;
//! * a multi-device btrfs to every member (`/sys/fs/btrfs/<uuid>/devices/`).
//!
//! A mount is *not* block-backed only if its file system type is on an explicit
//! list of kernel-interface, memory, layered and network file systems; any
//! other type has to be traced, and a use that cannot be traced is reported as
//! `unresolved`, which makes every disk in use. A directory that cannot be read
//! is a failure to trace (only a directory that is not there lists nothing).
//! Nothing is guessed.
//!
//! This trace is the primary mechanism for what the kernel's claim on a disk
//! cannot show (a loop device attached to a block device node); the exclusive
//! open in [`crate::disks`] is the authority for what it can.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

/// Where sysfs and procfs are. Production uses `/sys` and `/proc`; the
/// self-check builds trees of the same shape.
#[derive(Clone, Debug)]
pub struct Roots {
    pub sys: PathBuf,
    pub proc: PathBuf,
}

impl Roots {
    pub fn real() -> Self {
        Self {
            sys: "/sys".into(),
            proc: "/proc".into(),
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
/// and, for a block special file, the device it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId {
    pub dev: DevNum,
    pub ino: u64,
    /// `Some` only for a block special file.
    pub block_device: Option<DevNum>,
}

/// `stat` of a path from this process's own mount namespace, links followed;
/// `None` when it cannot be read from here (not there, unreachable, unlinked,
/// in another namespace).
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

/// A path with every link and `..` resolved, as text.
pub fn canonical_path(path: &str) -> Option<String> {
    fs::canonicalize(path)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// One line of a `mountinfo` table.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Mount {
    pub major: u32,
    pub minor: u32,
    pub mount_point: String,
    /// This mount's own options (`rw,nosuid,nodev`): per mount, not per file
    /// system.
    pub vfs_options: String,
    pub fstype: String,
    pub source: String,
    /// The file system's own options (`logdev=/dev/sdb2`).
    pub options: String,
}

impl Mount {
    /// Device nodes on this mount cannot be opened as devices.
    pub fn nodev(&self) -> bool {
        self.vfs_options.split(',').any(|o| o == "nodev")
    }
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

fn parse_mount_line(line: &str) -> Result<Mount> {
    // `id parent major:minor root mount-point options [optional...] - fstype source super-options`
    let (head, tail) = line.split_once(" - ").context("no ` - ` separator")?;
    let head: Vec<&str> = head.split(' ').collect();
    let tail: Vec<&str> = tail.split(' ').collect();
    ensure!(
        head.len() >= 6 && tail.len() >= 2,
        "fewer fields than a mount has"
    );
    let (major, minor) = head[2].split_once(':').context("no major:minor")?;
    Ok(Mount {
        major: major.parse().context("the major number")?,
        minor: minor.parse().context("the minor number")?,
        mount_point: unescape(head[4]),
        vfs_options: head[5].to_owned(),
        fstype: tail[0].to_owned(),
        source: unescape(tail[1]),
        options: tail.get(2).map_or_else(String::new, |o| (*o).to_owned()),
    })
}

/// A mount table. A line that cannot be read fails the whole table: dropping
/// it would hide a mount, and every disk would look a little freer.
pub fn parse_mountinfo(text: &str) -> Result<Vec<Mount>> {
    let mounts = text
        .lines()
        .enumerate()
        .map(|(n, line)| {
            parse_mount_line(line).with_context(|| format!("mountinfo line {}: {line:?}", n + 1))
        })
        .collect::<Result<Vec<_>>>()?;
    ensure!(!mounts.is_empty(), "a mount table with no mount in it");
    Ok(mounts)
}

/// One row of `/proc/swaps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Swap {
    /// The file or device, unescaped.
    pub path: String,
    /// `partition` (a block device) or `file`.
    pub kind: String,
}

/// The areas `/proc/swaps` lists (its first two columns). A row without both is
/// an error, not a smaller list.
pub fn parse_swaps(text: &str) -> Result<Vec<Swap>> {
    text.lines()
        .enumerate()
        .skip(1)
        .map(|(n, line)| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next()) {
                (Some(path), Some(kind)) => Ok(Swap {
                    path: unescape(path),
                    kind: kind.to_owned(),
                }),
                _ => bail!("swaps line {}: {line:?} has no file and type", n + 1),
            }
        })
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

/// What the kernel says the system is using, read once: the mount tables, the
/// swap areas and the bound loop devices.
#[derive(Clone, Debug, Default)]
pub struct System {
    /// Every mount of every mount namespace, without repeats. A mount made
    /// under a private subtree of the host's namespace, or in a service's own,
    /// is not in this process's table: it is a use all the same.
    pub mounts: Vec<Mount>,
    /// This process's own table: the namespace paths are resolved in.
    pub local: Vec<Mount>,
    pub swaps: Vec<Swap>,
    pub loops: Vec<LoopRow>,
    /// Mount tables that exist but could not be read (a process's namespace):
    /// each is a use that was not seen.
    pub unreadable: Vec<String>,
}

/// What reading a process's files goes through: `/proc`'s own tree in
/// production, a tree with injected failures in the self-check (a task that is
/// exiting answers `ESRCH` or `EINVAL`, which no ordinary file does).
pub struct ProcFs<'a> {
    pub read_link: &'a dyn Fn(&Path) -> io::Result<PathBuf>,
    pub read: &'a dyn Fn(&Path) -> io::Result<Vec<u8>>,
}

fn real_read_link(path: &Path) -> io::Result<PathBuf> {
    fs::read_link(path)
}

fn real_read(path: &Path) -> io::Result<Vec<u8>> {
    fs::read(path)
}

const REAL: ProcFs<'static> = ProcFs {
    read_link: &real_read_link,
    read: &real_read,
};

fn read_table(proc_fs: &ProcFs, path: &Path) -> Result<Vec<Mount>> {
    let bytes = (proc_fs.read)(path).with_context(|| format!("reading {}", path.display()))?;
    parse_mountinfo(&String::from_utf8_lossy(&bytes))
        .with_context(|| format!("in {}", path.display()))
}

/// The operating system's error number inside an error from [`read_table`].
fn os_error(error: &anyhow::Error) -> Option<i32> {
    error
        .downcast_ref::<io::Error>()
        .and_then(io::Error::raw_os_error)
}

/// A process that is gone (or going): `/proc/<pid>` answers `ENOENT`, or
/// `ESRCH` for a task whose entry is already being torn down.
fn gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

/// The mount tables of this system: this process's, the host's (process 1's) and
/// those of every other mount namespace a process is in (found by
/// `/proc/<pid>/ns/mnt`, one table each). This process's and the host's must be
/// read or the probe fails; another namespace that cannot be read is reported,
/// and counts as a use that cannot be traced.
pub fn read_mounts(proc: &Path) -> Result<(Vec<Mount>, Vec<Mount>, Vec<String>)> {
    read_mounts_with(proc, &REAL)
}

/// [`read_mounts`] through `proc_fs`.
///
/// A namespace counts as read only once one of its processes' tables has been
/// read: a process that exits first must not hide the other processes of its
/// namespace. A task that is exiting answers `EINVAL` for `mountinfo` (it has
/// no namespace proxy left) and `ESRCH`, `ENOENT` or `EACCES` for its `ns/mnt`
/// link; that is a process that is gone, not a table that cannot be read.
pub fn read_mounts_with(
    proc: &Path,
    proc_fs: &ProcFs,
) -> Result<(Vec<Mount>, Vec<Mount>, Vec<String>)> {
    let local = read_table(proc_fs, &proc.join("self/mountinfo"))?;
    let mut all: BTreeSet<Mount> = local.iter().cloned().collect();
    all.extend(read_table(proc_fs, &proc.join("1/mountinfo"))?);
    let mut seen: BTreeSet<PathBuf> = ["self", "1"]
        .iter()
        .filter_map(|p| (proc_fs.read_link)(&proc.join(p).join("ns/mnt")).ok())
        .collect();
    // What stopped each namespace's table being read, until some process of it
    // is read.
    let mut failed: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut unreadable = Vec::new();
    let mut processes: Vec<(u64, String)> = Vec::new();
    for entry in fs::read_dir(proc).with_context(|| format!("listing {}", proc.display()))? {
        let entry = entry.with_context(|| format!("listing {}", proc.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        if let Ok(pid) = name.parse() {
            processes.push((pid, name));
        }
    }
    // Oldest process first: its namespace is the one it was started in.
    processes.sort_unstable();
    for (_, name) in processes {
        let base = proc.join(&name);
        let namespace = match (proc_fs.read_link)(&base.join("ns/mnt")) {
            Ok(link) => link,
            Err(e) if gone(&e) => continue,
            Err(e) => {
                unreadable.push(format!(
                    "the mount namespace of process {name} cannot be identified: {e}"
                ));
                continue;
            }
        };
        if seen.contains(&namespace) {
            continue;
        }
        match read_table(proc_fs, &base.join("mountinfo")) {
            Ok(mounts) => {
                seen.insert(namespace);
                all.extend(mounts);
            }
            Err(e) if os_error(&e).is_some_and(|n| gone(&io::Error::from_raw_os_error(n))) => {}
            Err(e)
                if os_error(&e) == Some(libc::EINVAL)
                    && matches!(
                        (proc_fs.read_link)(&base.join("ns/mnt")),
                        Err(ref again) if gone(again)
                            || again.raw_os_error() == Some(libc::EACCES)
                    ) => {}
            Err(e) => {
                failed.entry(namespace).or_insert_with(|| {
                    format!("the mounts of process {name} cannot be read: {e:#}")
                });
            }
        }
    }
    unreadable.extend(
        failed
            .into_iter()
            .filter(|(namespace, _)| !seen.contains(namespace))
            .map(|(_, why)| why),
    );
    Ok((all.into_iter().collect(), local, unreadable))
}

/// The swap areas, read or an error: a table that cannot be read must never
/// look like one with nothing in it. A system with no `/proc/swaps` at all (a
/// kernel built without swap) has no swap areas.
pub fn read_swaps(path: &Path) -> Result<Vec<Swap>> {
    match fs::read(path) {
        Ok(bytes) => parse_swaps(&String::from_utf8_lossy(&bytes))
            .with_context(|| format!("in {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// File systems with no block device behind them: in memory, a window onto
/// the kernel, layered over directories of other mounts (each of which is
/// traced on its own), or on another computer. **Everything else is traced to
/// a device or is unresolved**, including a desktop session's FUSE services
/// anywhere but where the session mounts them ([`is_session_service`]).
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

/// File systems of images: nobody makes a device node on one after it is
/// mounted, so a file on one is a regular file of the image.
const IMAGE_FILE_SYSTEMS: &[&str] = &["iso9660", "udf", "squashfs", "erofs"];

/// A desktop session's FUSE services are the only user-space file systems that
/// hold no block storage the installer ignores, and only where the session
/// mounts them: the document portal at `/run/user/<uid>/doc` and gvfs at
/// `/run/user/<uid>/gvfs`. The same type anywhere else is a file system of
/// someone's making, traced like any other (and unresolved: FUSE has no device).
pub fn is_session_service(mount: &Mount) -> bool {
    let leaf = match mount.fstype.as_str() {
        "fuse.portal" => "doc",
        "fuse.gvfsd-fuse" => "gvfs",
        _ => return false,
    };
    mount
        .mount_point
        .strip_prefix("/run/user/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(uid, name)| {
            !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()) && name == leaf
        })
}

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
    /// A path with its links resolved.
    canonical: &'a dyn Fn(&str) -> Option<String>,
    /// The file system signature found on a block device, by kernel name.
    device_type: &'a dyn Fn(&str) -> Option<String>,
}

impl<'a> Tracer<'a> {
    pub fn new(roots: &'a Roots, system: &'a System) -> Self {
        Self {
            roots,
            system,
            stat: &stat_file,
            canonical: &canonical_path,
            device_type: &|_| None,
        }
    }

    pub fn with_stat(mut self, stat: &'a dyn Fn(&str) -> Option<FileId>) -> Self {
        self.stat = stat;
        self
    }

    pub fn with_canonical(mut self, canonical: &'a dyn Fn(&str) -> Option<String>) -> Self {
        self.canonical = canonical;
        self
    }

    pub fn with_device_types(mut self, device_type: &'a dyn Fn(&str) -> Option<String>) -> Self {
        self.device_type = device_type;
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

    /// The kernel name of the device a node stands for (`/dev/mapper/x`,
    /// `/dev/disk/by-label/x`, `/dev/loop0`), from what `stat` says of the node
    /// (its `st_rdev`, through any link): never from the node's name, which
    /// anyone with `mknod` can set to anything.
    pub fn device_of_node(&self, path: &str) -> Option<String> {
        if !path.starts_with('/') {
            return None;
        }
        let number = (self.stat)(path)?.block_device?;
        self.device_of_number(number)
    }

    /// The device a mount's source names: a device node, or `UUID=`, `LABEL=`,
    /// `PARTUUID=`, `PARTLABEL=` through `/dev/disk/by-*`.
    fn device_of_source(&self, source: &str) -> Option<String> {
        if source.starts_with('/') {
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

    /// The names in a directory. One that is not there lists nothing; any other
    /// failure, of the directory or of one of its entries, is an error: a
    /// directory that cannot be read is not an empty one.
    fn entries(dir: &Path) -> io::Result<Vec<String>> {
        let read = match fs::read_dir(dir) {
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e),
        };
        let mut names = Vec::new();
        for entry in read {
            names.push(entry?.file_name().to_string_lossy().into_owned());
        }
        names.sort();
        Ok(names)
    }

    /// The devices stacked on `name` (`holders/`): an LVM volume, an MD array
    /// or a dm-crypt mapping built on it, open now. An error says why they could
    /// not be listed.
    pub fn holders(&self, name: &str) -> Result<Vec<String>, String> {
        let dir = self
            .class_path(name)
            .ok_or_else(|| format!("{name} is not in sysfs"))?;
        Self::entries(&dir.join("holders"))
            .map_err(|e| format!("what is stacked on {name} cannot be listed: {e}"))
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
        match Self::entries(&dir.join("slaves")) {
            Ok(slaves) => {
                for slave in slaves {
                    self.device(&slave, depth + 1, reach);
                }
            }
            Err(e) => reach
                .unresolved
                .push(format!("what {name} is stacked on cannot be listed: {e}")),
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

    /// Whether a file that cannot be told from its name could be a block device
    /// node (which no device number says). It cannot where a node cannot be
    /// opened as a device (every mount carrying the file system is `nodev`) or
    /// where a node is not made after the fact (an image file system: iso9660,
    /// udf, squashfs, erofs): found among the mounts, or, when the file system
    /// was detached and no mount carries it, by the signature on its device.
    fn cannot_hold_a_node(&self, device: DevNum, carrying: &[&Mount]) -> bool {
        if !carrying.is_empty() {
            return carrying
                .iter()
                .all(|m| m.nodev() || IMAGE_FILE_SYSTEMS.contains(&m.fstype.as_str()));
        }
        device.0 != 0
            && self
                .device_of_number(device)
                .and_then(|name| (self.device_type)(&name))
                .is_some_and(|t| IMAGE_FILE_SYSTEMS.contains(&t.as_str()))
    }

    /// The path at which the file the kernel recorded for a loop device can
    /// be reached from here, and what `stat` says of it. The kernel's
    /// `(device, inode)` identify the file; a path only gets to it, and counts
    /// only when `stat` answers with exactly those two numbers (an inode that
    /// something holds open is not reused, so the numbers are the file's, not a
    /// namesake's). The name the kernel prints is tried first. For the files
    /// dracut opened in the initramfs it is relative to the file system's root
    /// (`/LiveOS/squashfs.img` for the ISO's squashfs, `/boot/isos/ilex.iso`
    /// for an ISO file that iso-scan found), though the mounts that carry them
    /// (`/run/initramfs/live`, `/run/initramfs/isoscan`) are in every table;
    /// the same name below each visible mount of the same device is tried next,
    /// which finds the file through the host disk's own mounted file system. A
    /// deleted file has no path, and a file no path reaches is not located:
    /// whatever it is, the caller decides what it might be.
    fn locate(
        &self,
        file: &str,
        inode: u64,
        device: DevNum,
        carrying: &[&Mount],
    ) -> (String, Option<FileId>) {
        let matches =
            |path: &str| (self.stat)(path).filter(|id| id.dev == device && id.ino == inode);
        let name = file.strip_suffix(" (deleted)").unwrap_or(file);
        if let Some(id) = matches(name) {
            return (name.to_owned(), Some(id));
        }
        if name.len() == file.len() && name.starts_with('/') {
            for mount in carrying {
                if mount.mount_point == "/" {
                    continue;
                }
                let below = format!("{}{name}", mount.mount_point);
                if let Some(id) = matches(&below) {
                    return (below, Some(id));
                }
            }
        }
        (name.to_owned(), None)
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
    ///   is then found by its name, confirmed by `(dev, ino)` and a name with
    ///   no link in it, and the btrfs mount that contains it is traced;
    /// * a file that is a block device node is also the device it stands for
    ///   (its `st_rdev`), once `(dev, ino)` match what the kernel recorded.
    ///
    /// A file the name does not confirm (deleted, renamed, under a mount) may
    /// be a block device node the kernel's numbers do not show: that is
    /// unresolved unless [`Self::cannot_hold_a_node`].
    fn backing_file(
        &self,
        name: &str,
        file: &str,
        inode: u64,
        device: DevNum,
        depth: u32,
        reach: &mut Reach,
    ) {
        let what = format!("{name} is backed by {file}");
        let carrying: Vec<&Mount> = self
            .system
            .mounts
            .iter()
            .filter(|m| (m.major, m.minor) == device)
            .collect();
        let (path, confirmed) = self.locate(file, inode, device, &carrying);
        let node = confirmed.and_then(|id| id.block_device);
        let mut explained = false;
        if device.0 != 0 {
            self.number(device, &what, depth, reach);
            explained = true;
        } else {
            for mount in &carrying {
                if FILES_NOT_ON_A_DISK.contains(&mount.fstype.as_str()) {
                    explained = true;
                } else if mount.fstype == "devtmpfs" {
                    explained |= node.is_some();
                } else if !NO_BLOCK_STORAGE.contains(&mount.fstype.as_str())
                    && !is_session_service(mount)
                {
                    explained = true;
                    self.mount(mount, depth + 1, reach);
                }
            }
            if !explained && confirmed.is_some() {
                // The name is the file's (its device and inode match what the
                // kernel recorded), so it is reached through the mounts that
                // contain it, if the name is its own path (no link or `..` in
                // it, which would send the text of the name to other mounts);
                // if the one on top is btrfs, so is the file, and every btrfs
                // on the way is traced.
                if (self.canonical)(&path).as_deref() == Some(path.as_str()) {
                    let chain = self.mounts_containing(&path);
                    if chain.last().is_some_and(|m| m.fstype == "btrfs") {
                        explained = true;
                        for mount in chain.into_iter().filter(|m| m.fstype == "btrfs") {
                            self.mount(mount, depth + 1, reach);
                        }
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
        } else if confirmed.is_none() && !self.cannot_hold_a_node(device, &carrying) {
            reach.unresolved.push(format!(
                "{what}, which is not at that name any more (deleted, renamed or hidden by a mount) on device {}:{}, where a block device node could be",
                device.0, device.1
            ));
        }
    }

    /// The mounts of this process's namespace whose mount point contains
    /// `path`, shortest first (so the one on top is last). Used only for a file
    /// whose `(dev, ino)` were confirmed through a name with no link in it, so
    /// the name is known to be the file's own path.
    fn mounts_containing(&self, path: &str) -> Vec<&Mount> {
        let mut chain: Vec<&Mount> = self
            .system
            .local
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
    /// the file systems that list one of `devices` or have this UUID. `None`
    /// when no file system is found; an error when sysfs cannot be read.
    fn btrfs_members(
        &self,
        devices: &[String],
        uuid: Option<&str>,
    ) -> io::Result<Option<BTreeSet<String>>> {
        let base = self.roots.sys.join("fs/btrfs");
        let mut members = BTreeSet::new();
        let mut found = false;
        for fsid in Self::entries(&base)? {
            let listed = Self::entries(&base.join(&fsid).join("devices"))?;
            if listed.is_empty() {
                continue;
            }
            if uuid == Some(fsid.as_str()) || devices.iter().any(|d| listed.contains(d)) {
                found = true;
                members.extend(listed);
            }
        }
        Ok(found.then_some(members))
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
        if NO_BLOCK_STORAGE.contains(&mount.fstype.as_str()) || is_session_service(mount) {
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
                Ok(Some(members)) => {
                    traced = true;
                    devices.extend(members);
                }
                Ok(None) => {
                    reach.unresolved.push(format!(
                        "the btrfs file system {} (mounted at {at}) lists no devices in sysfs",
                        mount.source
                    ));
                    return;
                }
                Err(e) => {
                    reach.unresolved.push(format!(
                        "the devices of the btrfs file system {} (mounted at {at}) cannot be listed: {e}",
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

    /// The block devices whose kernel names are `<prefix><digits>`.
    fn numbered(&self, prefix: &str) -> io::Result<Vec<String>> {
        Ok(Self::entries(&self.roots.sys.join("class/block"))?
            .into_iter()
            .filter(|name| {
                name.strip_prefix(prefix)
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            })
            .collect())
    }

    /// Kernel names of the loop devices that are attached to a file: those
    /// losetup lists and those sysfs says are, whether or not either knows of
    /// the other. The second is what could not be read.
    fn attached_loops(&self) -> (BTreeSet<String>, Vec<String>) {
        let mut names: BTreeSet<String> = self
            .system
            .loops
            .iter()
            .filter_map(|r| r.name.strip_prefix("/dev/").map(str::to_owned))
            .collect();
        let mut unresolved = Vec::new();
        match self.numbered("loop") {
            Ok(loops) => {
                for name in loops {
                    match self.class_path(&name).map(|d| Self::bound_in_sysfs(&d)) {
                        Some(Ok(false)) | None => {}
                        Some(Ok(true)) => {
                            names.insert(name);
                        }
                        Some(Err(e)) => {
                            unresolved
                                .push(format!("the backing file of {name} cannot be read: {e}"));
                            names.insert(name);
                        }
                    }
                }
            }
            Err(e) => unresolved.push(format!("the loop devices cannot be listed: {e}")),
        }
        (names, unresolved)
    }

    /// Record what a use of storage reached, with the reason each name is in use.
    fn record(
        usage: &mut Usage,
        reached: &mut BTreeSet<String>,
        reach: Reach,
        reason: impl Fn(&str) -> String,
    ) {
        usage.unresolved.extend(reach.unresolved);
        for name in reach.names {
            usage
                .in_use
                .entry(name.clone())
                .or_default()
                .insert(reason(&name));
            reached.insert(name);
        }
    }

    /// A zram device that writes back to a block device holds it exclusively
    /// and shows no `holders/` or `slaves/` link: every zram device is read, not
    /// only those that are swap areas or mounted.
    fn zram_writeback(&self, usage: &mut Usage, reached: &mut BTreeSet<String>) {
        let devices = match self.numbered("zram") {
            Ok(devices) => devices,
            Err(e) => {
                usage
                    .unresolved
                    .push(format!("the zram devices cannot be listed: {e}"));
                return;
            }
        };
        for name in devices {
            let Some(dir) = self.class_path(&name) else {
                continue;
            };
            let target = match fs::read_to_string(dir.join("backing_dev")) {
                Ok(text) => text.trim().to_owned(),
                // A kernel without writeback has no such file.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    usage.unresolved.push(format!(
                        "the writeback device of {name} cannot be read: {e}"
                    ));
                    continue;
                }
            };
            if target.is_empty() || target == "none" {
                continue;
            }
            let mut reach = Reach::default();
            match self.device_of_node(&target) {
                Some(device) => self.device(&device, 0, &mut reach),
                None => reach.unresolved.push(format!(
                    "{name} writes back to {target}, which is not a block device the installer can find"
                )),
            }
            Self::record(usage, reached, reach, |_| {
                format!("/dev/{name} writes back to {target}")
            });
        }
    }

    /// The devices that hold the journals of ext4 file systems: `/proc/fs/jbd2/`
    /// has one entry per journal, named `<device>-<inode>` for a journal inside
    /// its file system and just `<device>` for one on a device of its own (`!`
    /// stands for `/` in a name, as in sysfs). An external journal never shows
    /// in the mount options.
    fn external_journals(&self, usage: &mut Usage, reached: &mut BTreeSet<String>) {
        let entries = match Self::entries(&self.roots.proc.join("fs/jbd2")) {
            Ok(entries) => entries,
            Err(e) => {
                usage.unresolved.push(format!(
                    "the journals in /proc/fs/jbd2 cannot be listed: {e}"
                ));
                return;
            }
        };
        for entry in entries {
            if self.class_path(&entry).is_some() {
                // The name of a device: its journal is a file system's on another.
                let mut reach = Reach::default();
                self.device(&entry, 0, &mut reach);
                Self::record(usage, reached, reach, |_| {
                    format!("/dev/{entry} holds the journal of a mounted file system")
                });
            } else if entry.rsplit_once('-').is_some_and(|(device, inode)| {
                !inode.is_empty()
                    && inode.bytes().all(|b| b.is_ascii_digit())
                    && self.class_path(device).is_some()
            }) {
                // A journal inside the file system on that device: traced
                // through its mount.
            } else {
                usage.unresolved.push(format!(
                    "/proc/fs/jbd2/{entry} is a journal the installer cannot place on a device"
                ));
            }
        }
    }

    /// Trace every use of block storage the system has.
    pub fn usage(&self) -> Usage {
        let mut usage = Usage::default();
        usage
            .unresolved
            .extend(self.system.unreadable.iter().cloned());
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
            // A swap file cannot be placed from what the kernel lists: it is
            // on some block file system, which one is not known, so no disk
            // can be shown not to hold it. A swap area on a device is the
            // device its node stands for.
            match swap.kind.as_str() {
                "partition" => match (self.stat)(&swap.path).and_then(|id| id.block_device) {
                    Some(number) => {
                        self.number(
                            number,
                            &format!("the swap area {}", swap.path),
                            0,
                            &mut reach,
                        );
                    }
                    None => reach.unresolved.push(format!(
                        "the swap area {} is not a block device the installer can find",
                        swap.path
                    )),
                },
                "file" => reach.unresolved.push(format!(
                    "the swap file {} is on a file system the installer cannot identify",
                    swap.path
                )),
                other => reach.unresolved.push(format!(
                    "the swap area {} is of a kind ({other}) the installer does not know",
                    swap.path
                )),
            }
            Self::record(&mut usage, &mut reached, reach, |_| {
                format!("{} is in use as swap", swap.path)
            });
        }
        self.zram_writeback(&mut usage, &mut reached);
        self.external_journals(&mut usage, &mut reached);
        // A loop device is a use of its file's disk whether or not it is
        // mounted: attached to a file, it holds the file open. Those the live
        // medium's mounts or another mount already reached are accounted for.
        let (loops, mut unresolved) = self.attached_loops();
        usage.unresolved.append(&mut unresolved);
        for name in loops {
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
