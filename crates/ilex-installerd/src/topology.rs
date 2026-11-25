//! What the running system is made of, down to the physical disks: which disk
//! the live medium is on, and which disks the system is using for anything
//! else. The kernel is asked through sysfs, never the text of a mount's source
//! (`/dev/mapper/ventoy`, `/dev/disk/by-label/X` and `/dev/loop0` all name a
//! disk only indirectly): every mount is followed through its device number,
//! up from a partition to its disk, down through the devices stacked on it
//! (device-mapper, MD: `slaves/`) and through a loop device to the file it is
//! backed by and the device that file lives on.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
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

/// One line of `/proc/self/mountinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    pub major: u32,
    pub minor: u32,
    pub mount_point: String,
    pub fstype: String,
    pub source: String,
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

/// A mount that belongs to the live system's own medium: dracut's
/// `/run/initramfs/live` (and `/run/initramfs/isoscan`, the disk an ISO file
/// is found on), or any optical-style filesystem.
pub fn is_live_mount(mount: &Mount) -> bool {
    mount.mount_point == "/run/initramfs"
        || mount.mount_point.starts_with("/run/initramfs/")
        || ["iso9660", "udf"].contains(&mount.fstype.as_str())
}

/// Everything a block device is made of.
#[derive(Debug, Default)]
pub struct Footprint {
    /// Kernel names: the device, its disk, what is stacked on it, the loop
    /// devices' backing devices, transitively.
    pub names: BTreeSet<String>,
    /// What could not be followed.
    pub unresolved: Vec<String>,
}

/// How far a chain of stacked devices is followed.
const DEPTH: u32 = 16;

enum Holder {
    Device(String),
    /// A filesystem with no block device (tmpfs, virtiofs, overlay).
    NotBlock,
    /// No mount contains the path.
    Unknown,
}

pub struct Topology<'a> {
    roots: &'a Roots,
    mounts: &'a [Mount],
}

impl<'a> Topology<'a> {
    pub fn new(roots: &'a Roots, mounts: &'a [Mount]) -> Self {
        Self { roots, mounts }
    }

    /// `/sys/class/block/<name>` with its links followed: the device's own
    /// directory, below its parent disk's when it is a partition.
    fn class_path(&self, name: &str) -> Option<PathBuf> {
        if name.is_empty() || name.contains('/') || name == "." || name == ".." {
            return None;
        }
        fs::canonicalize(self.roots.sys.join("class/block").join(name)).ok()
    }

    /// The kernel name of the device a mount is on: from its device number,
    /// else from its source when that is a device node.
    pub fn device_of_mount(&self, mount: &Mount) -> Option<String> {
        if mount.major != 0 {
            let link = self
                .roots
                .sys
                .join("dev/block")
                .join(format!("{}:{}", mount.major, mount.minor));
            let name = fs::canonicalize(link)
                .ok()
                .and_then(|p| Some(p.file_name()?.to_str()?.to_owned()));
            if name.is_some() {
                return name;
            }
        }
        self.device_of_node(&mount.source)
    }

    /// The kernel name behind a device node (`/dev/mapper/x`,
    /// `/dev/disk/by-label/x`, `/dev/loop0`), following its links.
    pub fn device_of_node(&self, path: &str) -> Option<String> {
        let resolved = fs::canonicalize(self.roots.dev.join(path.strip_prefix("/dev/")?)).ok()?;
        let name = resolved.file_name()?.to_str()?.to_owned();
        self.class_path(&name).map(|_| name)
    }

    /// The device the file system holding `path` is on: the mount with the
    /// longest mount point that contains it.
    fn device_holding(&self, path: &str) -> Holder {
        let best = self
            .mounts
            .iter()
            .filter(|m| {
                m.mount_point == "/"
                    || path == m.mount_point
                    || path
                        .strip_prefix(m.mount_point.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|m| m.mount_point.len());
        match best {
            None => Holder::Unknown,
            Some(mount) => self
                .device_of_mount(mount)
                .map_or(Holder::NotBlock, Holder::Device),
        }
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

    /// Follow `name` to everything it is made of.
    pub fn footprint(&self, name: &str) -> Footprint {
        let mut footprint = Footprint::default();
        let mut queue = vec![(name.to_owned(), 0)];
        while let Some((name, depth)) = queue.pop() {
            if !footprint.names.insert(name.clone()) {
                continue;
            }
            if depth > DEPTH {
                footprint
                    .unresolved
                    .push(format!("{name} is stacked too deeply to follow"));
                continue;
            }
            let Some(dir) = self.class_path(&name) else {
                footprint.unresolved.push(format!("{name} is not in sysfs"));
                continue;
            };
            // A partition is its disk's; the disk carries the rest.
            if dir.join("partition").exists() {
                if let Some(parent) = dir
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|n| n.to_str())
                {
                    queue.push((parent.to_owned(), depth + 1));
                }
                continue;
            }
            for slave in Self::entries(&dir.join("slaves")) {
                queue.push((slave, depth + 1));
            }
            // A loop device is its backing file's file system's device.
            if let Ok(text) = fs::read_to_string(dir.join("loop/backing_file")) {
                let file = text.trim_end_matches('\n');
                let file = file.strip_suffix(" (deleted)").unwrap_or(file);
                match self.device_holding(file) {
                    Holder::Device(device) => queue.push((device, depth + 1)),
                    Holder::NotBlock => {}
                    Holder::Unknown => footprint.unresolved.push(format!(
                        "{name} is backed by {file}, which is on no mount the installer can see"
                    )),
                }
            }
        }
        footprint
    }
}

/// How the running system uses the disks.
#[derive(Debug, Default)]
pub struct Usage {
    /// Kernel names (disks, partitions, stacked devices) of the live medium.
    pub live: BTreeSet<String>,
    /// Kernel names touched by anything else in use, with what uses them.
    pub in_use: BTreeMap<String, BTreeSet<String>>,
    /// Why the live medium could not be fully located, if so.
    pub unresolved: Vec<String>,
}

/// Classify every mount and swap area: the live medium's, or other use.
pub fn usage(topology: &Topology, mounts: &[Mount], swaps: &[String]) -> Usage {
    let mut usage = Usage::default();
    for mount in mounts {
        let live = is_live_mount(mount);
        let Some(device) = topology.device_of_mount(mount) else {
            // Not block storage (tmpfs, virtiofs, overlay), unless a device
            // node was named and could not be found.
            if live && mount.source.starts_with("/dev/") {
                usage.unresolved.push(format!(
                    "{} (mounted at {}) is not a block device the installer can find",
                    mount.source, mount.mount_point
                ));
            }
            continue;
        };
        let footprint = topology.footprint(&device);
        if live {
            usage.live.extend(footprint.names);
            usage.unresolved.extend(footprint.unresolved);
        } else {
            for name in footprint.names {
                usage
                    .in_use
                    .entry(name)
                    .or_default()
                    .insert(format!("/dev/{device} is mounted at {}", mount.mount_point));
            }
        }
    }
    for swap in swaps {
        let device = if swap.starts_with("/dev/") {
            topology.device_of_node(swap)
        } else {
            match topology.device_holding(swap) {
                Holder::Device(device) => Some(device),
                Holder::NotBlock | Holder::Unknown => None,
            }
        };
        if let Some(device) = device {
            for name in topology.footprint(&device).names {
                usage
                    .in_use
                    .entry(name)
                    .or_default()
                    .insert(format!("{swap} is in use as swap"));
            }
        }
    }
    usage
}
