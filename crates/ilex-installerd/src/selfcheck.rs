//! `ilex-installerd --self-check`: the helper's rules on fixtures, then this
//! system for real (its disks, its repart and the shipped definitions). Run in
//! the installer VM by the `installer::` tier.

use crate::{
    answers, apply, disks, exec, finish, helper, install, luks, policy, progress, registry, repart,
    scratch::Scratch,
    secret, stagelog, stamp, sys, target,
    topology::{self, FileId, LoopRow, Roots, System, Tracer},
};
use anyhow::{Context, Result, ensure};
use ilex_installer_model::{
    probe::{Extent, Partition},
    recovery::{TpmOutcome, TpmStatus},
};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    fs, io,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    time::Duration,
};

const LSBLK: &str = r#"{"blockdevices":[
 {"name":"/dev/vda","kname":"/dev/vda","type":"disk","size":42949672960,"model":"QEMU HARDDISK","serial":"fixture","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":"gpt","parttype":null,"partlabel":null,"fstype":null,"label":null,"pkname":null,
  "children":[
   {"name":"/dev/vda1","kname":"/dev/vda1","type":"part","size":536870912,"fstype":"vfat","label":"SYSTEM","pttype":"gpt","parttype":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B","partlabel":"EFI system partition","pkname":"/dev/vda"},
   {"name":"/dev/vda2","kname":"/dev/vda2","type":"part","size":1073741824,"fstype":"BitLocker","label":null,"pttype":"gpt","parttype":"ebd0a0a2-b9e5-4433-87c0-68b6b72699c7","partlabel":null,"pkname":"/dev/vda"}]},
 {"name":"/dev/nvme0n1","kname":"/dev/nvme0n1","type":"disk","size":"2000398934016","model":"Corsair MP400 ","serial":"S1","tran":"nvme","rm":"0","ro":"0","log-sec":"512","pttype":null,"children":null},
 {"name":"/dev/sdb","kname":"/dev/sdb","type":"disk","size":8000000000,"model":"USB Stick","serial":"U1","tran":"usb","rm":true,"ro":false,"log-sec":512,"pttype":"dos","children":[
   {"name":"/dev/sdb1","kname":"/dev/sdb1","type":"part","size":4000000000,"fstype":"iso9660","label":"ilex-installer","parttype":"0x83","partlabel":null,"pkname":"/dev/sdb"}]},
 {"name":"/dev/zram0","kname":"/dev/zram0","type":"disk","size":4294967296,"tran":null,"rm":false,"ro":false,"log-sec":4096},
 {"name":"/dev/sr0","kname":"/dev/sr0","type":"rom","size":1073741824,"tran":"sata","rm":true,"ro":false,"log-sec":2048},
 {"name":"/dev/sdc","kname":"/dev/sdc","type":"disk","size":16000000000,"tran":"usb","rm":true,"ro":true,"log-sec":512},
 {"name":"/dev/sdd","kname":"/dev/sdd","type":"disk","size":16000000000,"model":"LUN","tran":"iscsi","rm":false,"ro":false,"log-sec":512},
 {"name":"/dev/sde","kname":"/dev/sde","type":"disk","size":16000000000,"model":"SAN","tran":"fc","rm":false,"ro":false,"log-sec":512},
 {"name":"/dev/nvme1n1","kname":"/dev/nvme1n1","type":"disk","size":16000000000,"model":"Fabric","tran":"nvme","rm":false,"ro":false,"log-sec":512},
 {"name":"/dev/nbd0","kname":"/dev/nbd0","type":"disk","size":16000000000,"tran":null,"rm":false,"ro":false,"log-sec":512}]}"#;

const SFDISK: &str = r#"{"partitiontable":{"label":"gpt","id":"X","device":"/dev/vda","unit":"sectors","firstlba":2048,"lastlba":83886046,"sectorsize":512,"partitions":[]}}"#;

const REPART: &str = r#"[
 {"type":"esp","label":"ilex-esp","uuid":"e2857866-e04d-4eb3-915d-101cbf838697","partno":0,"file":"/x/10-esp.conf","node":"/dev/vdb1","offset":1048576,"old_size":0,"raw_size":536870912,"old_padding":0,"raw_padding":0,"activity":"create"},
 {"type":"root-x86-64","label":"ilex-root","uuid":"644b2311-535f-46d4-adab-8cff81f14455","partno":1,"file":"/x/30-root.conf","node":"/dev/vdb2","offset":537919488,"old_size":0,"raw_size":41337991168,"old_padding":0,"raw_padding":0,"activity":"create"}]"#;

fn hex(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

fn image_manifest() -> String {
    format!(
        r#"{{"schemaVersion":2,"config":{{"digest":"{}","size":100}},"layers":[{{"digest":"{}","size":1000}},{{"digest":"{}","size":2000}}],"annotations":{{"org.opencontainers.image.version":"45.1"}}}}"#,
        hex('a'),
        hex('b'),
        hex('c')
    )
}

fn index_manifest(children: &str) -> String {
    format!(r#"{{"schemaVersion":2,"manifests":[{children}]}}"#)
}

fn part(
    path: &str,
    start: u64,
    size: u64,
    filesystem: &str,
    label: &str,
    type_id: &str,
) -> Partition {
    Partition {
        number: 1,
        path: path.into(),
        start_bytes: start,
        size_bytes: size,
        type_id: type_id.into(),
        label: String::new(),
        filesystem: filesystem.into(),
        filesystem_label: label.into(),
    }
}

fn check_parsers() -> Result<()> {
    const MIB: u64 = 1024 * 1024;
    // lsblk: disks only, installable only, with their partitions.
    let raws = disks::parse_lsblk(
        LSBLK,
        &|name| match name {
            "vda1" => Some(2048),
            "vda2" => Some(2048 + 1_048_576),
            _ => None,
        },
        &|name| Some(if name == "nvme1n1" { "tcp" } else { "pcie" }.to_owned()),
    )?;
    // Zram, optical, read-only and network disks (iSCSI, Fibre Channel, NVMe
    // over fabrics, nbd) are not somewhere to install.
    let names: Vec<&str> = raws.iter().map(|r| r.kname.as_str()).collect();
    ensure!(names == ["vda", "nvme0n1", "sdb"], "lsblk disks: {names:?}");
    let vda = &raws[0].disk;
    ensure!(
        vda.bus == "virtio"
            && vda.partitions.len() == 2
            && vda.partitions[0].start_bytes == MIB
            && vda.partitions[0].type_id == "c12a7328-f81f-11d2-ba4b-00a0c93ec93b"
            && vda.partitions[1].number == 2,
        "vda: {vda:?}"
    );
    ensure!(
        raws[1].disk.bus == "nvme"
            && raws[1].disk.model == "Corsair MP400"
            && raws[1].disk.size_bytes == 2_000_398_934_016,
        "nvme: {:?}",
        raws[1].disk
    );
    ensure!(
        raws[2].disk.removable && raws[2].disk.bus == "usb",
        "usb: {:?}",
        raws[2].disk
    );

    // sfdisk and free space.
    ensure!(
        disks::parse_sfdisk(SFDISK) == Some((2048 * 512, 83_886_047 * 512)),
        "sfdisk"
    );
    let size = 40 * 1024 * MIB;
    let blank = disks::free_extents(size, None, &[]);
    ensure!(
        blank
            == [Extent {
                start_bytes: MIB,
                size_bytes: size - 2 * MIB
            }],
        "blank disk: {blank:?}"
    );
    let two = [
        part("/dev/x1", MIB, 512 * MIB, "vfat", "", ""),
        part("/dev/x2", 600 * MIB, 1024 * MIB, "ntfs", "", ""),
    ];
    let gaps = disks::free_extents(size, Some((2048 * 512, size - 16896)), &two);
    ensure!(
        gaps == [
            Extent {
                start_bytes: 513 * MIB,
                size_bytes: 87 * MIB
            },
            Extent {
                start_bytes: 1624 * MIB,
                size_bytes: size - 1624 * MIB - MIB
            },
        ],
        "gaps: {gaps:?}"
    );
    let full = [part("/dev/x1", MIB, size - 2 * MIB, "ext4", "", "")];
    ensure!(
        disks::free_extents(size, None, &full).is_empty(),
        "a full disk has free space"
    );

    // What else is on a disk.
    let seen = |fs: &str, label: &str, type_id: &str| {
        disks::classify(&part("/dev/x1", 0, 1, fs, label, type_id)).map(|s| s.kind)
    };
    ensure!(
        seen("ntfs", "Windows", "") == Some("windows".into()),
        "ntfs"
    );
    ensure!(
        seen("BitLocker", "", "") == Some("bitlocker".into()),
        "bitlocker"
    );
    ensure!(seen("btrfs", "", "") == Some("linux".into()), "btrfs");
    ensure!(
        seen("crypto_LUKS", "", "") == Some("encrypted".into()),
        "luks"
    );
    ensure!(
        seen("", "", "e3c9e316-0b5c-4db8-817d-f92df00215ae") == Some("windows".into()),
        "msr"
    );
    ensure!(
        seen("vfat", "", "").is_none()
            && seen("swap", "", "").is_none()
            && seen("", "", "").is_none(),
        "false positive"
    );

    // Stable names: eui, then wwn, then other ids, then the bus path.
    let link = |by: &str, name: &str| (by.to_owned(), name.to_owned());
    let id = disks::stable_ids(
        "nvme0n1",
        &[
            link("by-id", "nvme-Corsair_S1"),
            link("by-id", "nvme-eui.0025"),
            link("by-path", "pci-0000:01:00.0-nvme-1"),
        ],
    );
    ensure!(id == "nvme-eui.0025", "eui: {id}");
    ensure!(
        disks::stable_ids(
            "vdb",
            &[
                link("by-path", "pci-0000:00:05.0"),
                link("by-id", "virtio-blank")
            ]
        ) == "virtio-blank",
        "virtio id"
    );
    ensure!(
        disks::stable_ids("vdb", &[link("by-id", "virtio-blank-part1")]) == "vdb",
        "partition link used"
    );
    // A link that describes what is on the disk (an LVM physical volume's
    // UUID, a device-mapper or MD name) sorts ahead of `virtio-`, `usb-` and
    // `scsi-` but is no identity: it goes when the signature is wiped.
    ensure!(
        disks::stable_ids(
            "vdb",
            &[
                link("by-id", "lvm-pv-uuid-abcdef-1234"),
                link("by-id", "dm-name-vg-root"),
                link("by-id", "dm-uuid-LVM-abc"),
                link("by-id", "md-uuid-abcd:ef"),
                link("by-id", "md-name-host:0"),
                link("by-id", "virtio-ilex-disk-1"),
            ]
        ) == "virtio-ilex-disk-1",
        "a link that names the disk's contents was chosen over the hardware's"
    );
    ensure!(
        disks::stable_ids("vdb", &[link("by-id", "lvm-pv-uuid-abcdef-1234")]) == "vdb"
            && disks::stable_ids(
                "vdb",
                &[
                    link("by-id", "lvm-pv-uuid-abcdef-1234"),
                    link("by-path", "pci-0000:00:05.0")
                ]
            ) == "pci-0000:00:05.0",
        "a disk whose only by-id link names its contents was given that name"
    );
    for hardware in [
        "nvme-eui.0025",
        "nvme-Samsung_SSD_990_S1",
        "wwn-0x5000c500a1b2c3d4",
        "ata-Samsung_SSD_870_S1",
        "scsi-SATA_Disk_S1",
        "usb-SanDisk_Cruzer_0001-0:0",
        "mmc-SD16G_0x1234",
        "virtio-ilex-disk-1",
        "ccw-0.0.0100",
        "ieee1394-0x1234",
    ] {
        ensure!(
            disks::is_hardware_id(hardware),
            "{hardware} is the hardware's name of a disk"
        );
    }
    for contents in [
        "lvm-pv-uuid-abc",
        "dm-name-vg-root",
        "dm-uuid-LVM-abc",
        "md-uuid-abc",
        "md-name-h:0",
        "LVM-xyz",
        "raid-x",
        "",
    ] {
        ensure!(
            !disks::is_hardware_id(contents),
            "{contents:?} was taken for the hardware's name of a disk"
        );
    }

    // A signature on the whole disk (no partition table) is data too: a file
    // system, LUKS, an LVM physical volume, a swap area, and any other signature.
    let whole = disks::parse_lsblk(
        r#"{"blockdevices":[
 {"name":"/dev/vdb","kname":"/dev/vdb","type":"disk","size":42949672960,"serial":"b","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":null,"fstype":"LVM2_member","label":null,"children":null},
 {"name":"/dev/vdc","kname":"/dev/vdc","type":"disk","size":42949672960,"serial":"c","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":null,"fstype":"ext4","label":"data","children":null},
 {"name":"/dev/vdd","kname":"/dev/vdd","type":"disk","size":42949672960,"serial":"d","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":null,"fstype":"crypto_LUKS","label":null,"children":null},
 {"name":"/dev/vde","kname":"/dev/vde","type":"disk","size":42949672960,"serial":"e","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":null,"fstype":"zfs_member","label":"tank\u0007","children":null},
 {"name":"/dev/vdf","kname":"/dev/vdf","type":"disk","size":42949672960,"serial":"f","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":null,"fstype":null,"label":null,"children":null},
 {"name":"/dev/vdg","kname":"/dev/vdg","type":"disk","size":42949672960,"serial":"g","tran":"virtio","rm":false,"ro":false,"log-sec":512,"pttype":"gpt","fstype":null,"label":null,"children":[]}]}"#,
        &|_| None,
        &|_| None,
    )?;
    let described = |index: usize| -> Vec<(String, Option<String>, String)> {
        whole[index]
            .disk
            .other_systems
            .iter()
            .map(|s| (s.kind.clone(), s.partition.clone(), s.description.clone()))
            .collect()
    };
    ensure!(
        whole.len() == 6
            && described(0).len() == 1
            && described(0)[0].0 == "linux"
            && described(0)[0].1.is_none()
            && described(0)[0].2.contains("LVM2_member")
            && described(0)[0].2.contains("whole disk /dev/vdb"),
        "a whole-disk LVM physical volume is not a system: {:?}",
        described(0)
    );
    ensure!(
        described(1).len() == 1
            && described(1)[0].2.contains("ext4")
            && described(1)[0].2.contains("\"data\"")
            && described(2).len() == 1
            && described(2)[0].0 == "encrypted"
            && described(3).len() == 1
            && described(3)[0].0 == "filesystem"
            && described(3)[0].2.contains("zfs_member")
            && !described(3)[0].2.contains('\u{7}')
            && described(4).is_empty()
            && described(5).is_empty(),
        "whole-disk signatures: {:?} {:?} {:?} {:?}",
        described(1),
        described(2),
        described(3),
        described(4)
    );

    // repart's plan, the registry's manifests, meminfo.
    let layout = repart::Layout {
        dir: "/x".into(),
        formats: vec![
            ("10-esp.conf".into(), "vfat".into(), false),
            ("30-root.conf".into(), "btrfs".into(), true),
        ],
    };
    let planned = repart::parse(REPART, &layout)?;
    ensure!(
        planned.len() == 2
            && planned[1].number == 2
            && planned[1].filesystem == "btrfs"
            && planned[1].encrypted
            && !planned[0].encrypted,
        "repart plan: {planned:?}"
    );
    ensure!(
        repart::parse(&REPART.replace("create", "resize"), &layout).is_err(),
        "repart resize accepted"
    );
    check_registry()?;
    let memory = sys::parse_meminfo(
        "MemTotal:        8000000 kB\nMemFree: 1 kB\nMemAvailable:    6000000 kB\n",
    )?;
    ensure!(
        memory.total_bytes == 8_192_000_000 && memory.available_bytes == 6_144_000_000,
        "meminfo"
    );
    Ok(())
}

/// A throwaway directory for fixtures, removed (recursively: it is ours and
/// holds only what a check wrote) on drop.
struct TestDir(PathBuf);

impl TestDir {
    fn new(parent: &Path, name: &str) -> Result<Self> {
        let path = parent.join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, text)?;
    Ok(())
}

// --- registry ---------------------------------------------------------------

fn check_registry() -> Result<()> {
    let image = image_manifest();
    match registry::parse_manifest(image.as_bytes())? {
        registry::Parsed::Image(m) => ensure!(
            m.download_bytes == 3100
                && m.digest.starts_with("sha256:")
                && m.annotations_version.as_deref() == Some("45.1"),
            "image manifest"
        ),
        registry::Parsed::Index { .. } => anyhow::bail!("an image parsed as an index"),
    }
    // An index: the amd64 child, among entries that are not images for it.
    let index = index_manifest(&format!(
        r#"{{"digest":"{}","platform":{{"os":"linux","architecture":"arm64"}}}},{{"digest":"{}","platform":{{"os":"unknown","architecture":"unknown"}}}},{{"digest":"{}","platform":{{"os":"linux","architecture":"amd64"}}}}"#,
        hex('1'),
        hex('2'),
        hex('3')
    ));
    match registry::parse_manifest(index.as_bytes())? {
        registry::Parsed::Index { amd64, .. } => ensure!(amd64 == hex('3'), "index child"),
        registry::Parsed::Image(_) => anyhow::bail!("an index parsed as an image"),
    }
    // What a registry may send that must not be accepted: every case is a
    // manifest the installer would otherwise pin or pass on to skopeo.
    let sized = |a: &str, b: &str| {
        format!(
            r#"{{"schemaVersion":2,"config":{{"digest":"{}","size":{a}}},"layers":[{{"digest":"{}","size":{b}}},{{"digest":"{}","size":1}}]}}"#,
            hex('a'),
            hex('b'),
            hex('c')
        )
    };
    let max = u64::MAX.to_string();
    let bad: [(&str, String); 12] = [
        ("sizes that overflow", sized(&max, "5")),
        ("a layer size that overflows", sized("1", &max)),
        ("a negative size", sized("-1", "1")),
        ("a size that is not a number", sized("\"big\"", "1")),
        (
            "no layers",
            format!(
                r#"{{"schemaVersion":2,"config":{{"digest":"{}","size":1}}}}"#,
                hex('a')
            ),
        ),
        (
            "empty layers",
            format!(
                r#"{{"schemaVersion":2,"config":{{"digest":"{}","size":1}},"layers":[]}}"#,
                hex('a')
            ),
        ),
        (
            "no config",
            format!(
                r#"{{"schemaVersion":2,"layers":[{{"digest":"{}","size":1}}]}}"#,
                hex('a')
            ),
        ),
        (
            "an upper-case digest",
            image.replace(&hex('a'), &format!("sha256:{}", "A".repeat(64))),
        ),
        (
            "a layer without a digest",
            format!(
                r#"{{"schemaVersion":2,"config":{{"digest":"{}","size":1}},"layers":[{{"size":1}}]}}"#,
                hex('a')
            ),
        ),
        (
            "schema 1",
            image.replace("\"schemaVersion\":2", "\"schemaVersion\":1"),
        ),
        (
            "an index child that is an option",
            index_manifest(
                r#"{"digest":"--help","platform":{"os":"linux","architecture":"amd64"}}"#,
            ),
        ),
        (
            "an index without amd64",
            index_manifest(&format!(
                r#"{{"digest":"{}","platform":{{"os":"linux","architecture":"arm64"}}}}"#,
                hex('1')
            )),
        ),
    ];
    for (what, manifest) in &bad {
        ensure!(
            registry::parse_manifest(manifest.as_bytes()).is_err(),
            "a manifest with {what} was accepted"
        );
    }
    ensure!(
        registry::parse_manifest(b"not json").is_err(),
        "garbage accepted"
    );
    // Digests: the one shape.
    for good in [hex('0'), hex('f')] {
        ensure!(registry::is_digest(&good), "{good} refused");
    }
    for wrong in [
        "sha256:".to_owned(),
        format!("sha256:{}", "a".repeat(63)),
        format!("sha256:{}", "a".repeat(65)),
        format!("sha256:{}", "A".repeat(64)),
        format!("sha512:{}", "a".repeat(64)),
        format!("sha256:{}\n", "a".repeat(64)),
        format!("sha256:{}/../x", "a".repeat(64)),
        "--help".to_owned(),
        String::new(),
    ] {
        ensure!(!registry::is_digest(&wrong), "{wrong:?} accepted");
    }
    // Only an explicit absence is absence, whatever the case; the rest is the
    // registry not answering.
    for (stderr, code) in [
        (
            "Error: reading manifest x in ghcr.io/o/r: manifest unknown",
            ilex_installer_model::plan::ErrorCode::ImageNotFound,
        ),
        (
            "Error: MANIFEST UNKNOWN: manifest unknown",
            ilex_installer_model::plan::ErrorCode::ImageNotFound,
        ),
        (
            "requested name Unknown: Name Unknown",
            ilex_installer_model::plan::ErrorCode::ImageNotFound,
        ),
        (
            "Error: 404 not found",
            ilex_installer_model::plan::ErrorCode::Registry,
        ),
        (
            "Error: unauthorized: authentication required",
            ilex_installer_model::plan::ErrorCode::Registry,
        ),
        (
            "dial tcp: lookup ghcr.io: no such host",
            ilex_installer_model::plan::ErrorCode::Registry,
        ),
    ] {
        let error = registry::failure("ghcr.io/o/r:t", stderr);
        ensure!(
            error.code == code,
            "{stderr:?} classified as {:?}",
            error.code
        );
    }
    let error = registry::failure("ghcr.io/o/r:t", "oops \u{1b}[31mred\u{7} and \nbad");
    ensure!(
        !error.message.chars().any(char::is_control),
        "control characters shown: {:?}",
        error.message
    );
    Ok(())
}

// --- subprocess output, scratch directories ---------------------------------

fn check_exec_and_scratch(parent: &Path) -> Result<()> {
    let tool = |count: &str, stdout: usize| {
        exec::run_limited(
            "head",
            &["-c", count, "/dev/zero"],
            Duration::from_secs(20),
            exec::Limits {
                stdout,
                stderr: 1024,
            },
        )
    };
    ensure!(
        tool("1000", 1000)?.stdout.len() == 1000,
        "output of exactly the ceiling was refused"
    );
    ensure!(
        tool("1001", 1000).is_err(),
        "output over the ceiling was kept"
    );
    // A tool that never stops printing is stopped, not waited for.
    let started = std::time::Instant::now();
    ensure!(
        exec::run_limited(
            "yes",
            &[],
            Duration::from_secs(30),
            exec::Limits {
                stdout: 4096,
                stderr: 1024
            }
        )
        .is_err()
            && started.elapsed() < Duration::from_secs(10),
        "an endless tool was not stopped"
    );
    ensure!(
        exec::run_limited(
            "sh",
            &["-c", "head -c 3000 /dev/zero >&2"],
            Duration::from_secs(20),
            exec::Limits {
                stdout: 10,
                stderr: 1000
            }
        )
        .is_err(),
        "stderr over its ceiling was kept"
    );

    // Private scratch directories: fresh, ours, 0700, never a link.
    let dir = TestDir::new(parent, "scratch")?;
    let (first, second);
    {
        let mut one = Scratch::new(&dir.0, "scan")?;
        let two = Scratch::new(&dir.0, "scan")?;
        first = one.path().to_path_buf();
        second = two.path().to_path_buf();
        let meta = fs::symlink_metadata(&first)?;
        ensure!(
            meta.is_dir() && meta.permissions().mode() & 0o777 == 0o700,
            "scratch mode {:o}",
            meta.permissions().mode()
        );
        ensure!(first != second, "two scratch directories share a path");
        let mount_point = one.subdir("esp1")?;
        ensure!(
            fs::symlink_metadata(&mount_point)?.permissions().mode() & 0o777 == 0o700,
            "mount point mode"
        );
        ensure!(one.subdir("../x").is_err(), "a path was taken for a name");
    }
    ensure!(
        !first.exists() && !second.exists(),
        "scratch directories were left behind"
    );
    // A parent that is a link, or that others can write to, is refused.
    let real = dir.0.join("real");
    fs::create_dir(&real)?;
    symlink(&real, dir.0.join("link"))?;
    ensure!(
        Scratch::new(&dir.0.join("link"), "scan").is_err(),
        "a symlinked parent was accepted"
    );
    fs::set_permissions(&real, fs::Permissions::from_mode(0o777))?;
    ensure!(
        Scratch::new(&real, "scan").is_err(),
        "a world-writable parent was accepted"
    );
    ensure!(
        fs::read_dir(&real)?.next().is_none(),
        "something was made in a refused parent"
    );
    // The ESP child refuses what is not a device and a plain directory, and
    // says so instead of mounting.
    let refused = disks::scan_esp_child("/dev/null", &dir.0.join("nowhere"));
    ensure!(
        refused.error.is_some() && refused.loaders.is_empty(),
        "the ESP child tried a mount point that is not there"
    );
    let refused = disks::scan_esp_child("relative", &real);
    ensure!(
        refused.error.is_some(),
        "the ESP child took a relative device"
    );
    Ok(())
}

// --- NVMe transports ---------------------------------------------------------

/// NVMe namespaces in a sysfs tree of the kernel's shapes: a controller's own
/// namespace (no native multipath), and the subsystem's namespace (Fedora's
/// default), whose controllers are the subsystem's `nvme*` entries and, on newer
/// kernels, `multipath/*/device`. Only a namespace whose controllers are all on
/// PCIe is a local disk.
fn check_nvme(parent: &Path) -> Result<()> {
    let dir = TestDir::new(parent, "nvme")?;
    let sys = dir.0.join("sys");
    fs::create_dir_all(sys.join("class/block"))?;
    let make = |path: &str| -> Result<PathBuf> {
        let dir = sys.join("devices").join(path);
        fs::create_dir_all(&dir)?;
        Ok(dir)
    };
    let controller = |path: &str, transport: &str| -> Result<PathBuf> {
        let dir = make(path)?;
        write(&dir.join("transport"), transport)?;
        Ok(dir)
    };
    // The namespace's `device` link is its parent: the controller, or the subsystem.
    let namespace = |parent: &Path, name: &str| -> Result<PathBuf> {
        let dir = parent.join(name);
        fs::create_dir_all(&dir)?;
        symlink("..", dir.join("device"))?;
        symlink(&dir, sys.join("class/block").join(name))?;
        Ok(dir)
    };
    // A local drive with no multipath: `device` is its controller.
    let nvme0 = controller("pci0/nvme/nvme0", "pcie\n")?;
    namespace(&nvme0, "nvme0n1")?;
    // Native multipath, one PCIe controller, two, a fabric, a mixture.
    let subsystem = |n: u32| make(&format!("virtual/nvme-subsystem/nvme-subsys{n}"));
    let attach = |subsystem: &Path, name: &str, controller: &Path| -> Result<()> {
        symlink(controller, subsystem.join(name))?;
        Ok(())
    };
    let s1 = subsystem(1)?;
    attach(&s1, "nvme1", &controller("pci1/nvme/nvme1", "pcie\n")?)?;
    namespace(&s1, "nvme1n1")?;
    let s2 = subsystem(2)?;
    attach(&s2, "nvme2", &controller("pci2/nvme/nvme2", "pcie\n")?)?;
    attach(&s2, "nvme3", &controller("pci3/nvme/nvme3", "pcie\n")?)?;
    namespace(&s2, "nvme2n1")?;
    let s3 = subsystem(3)?;
    attach(&s3, "nvme4", &controller("net/nvme/nvme4", "tcp\n")?)?;
    namespace(&s3, "nvme3n1")?;
    let s4 = subsystem(4)?;
    attach(&s4, "nvme5", &controller("pci5/nvme/nvme5", "pcie\n")?)?;
    attach(&s4, "nvme6", &controller("net/nvme/nvme6", "rdma\n")?)?;
    namespace(&s4, "nvme4n1")?;
    // No controller found, or one whose transport reads empty: not provably local.
    let s5 = subsystem(5)?;
    namespace(&s5, "nvme5n1")?;
    let s6 = subsystem(6)?;
    attach(&s6, "nvme7", &controller("pci7/nvme/nvme7", "\n")?)?;
    namespace(&s6, "nvme6n1")?;
    // The paths a newer kernel lists under the namespace itself.
    let s7 = subsystem(7)?;
    let ns7 = namespace(&s7, "nvme7n1")?;
    controller(
        &ns7.strip_prefix(sys.join("devices"))?
            .join("multipath/nvme7c0n1/device")
            .to_string_lossy(),
        "fc\n",
    )?;
    let s8 = subsystem(8)?;
    let ns8 = namespace(&s8, "nvme8n1")?;
    controller(
        &ns8.strip_prefix(sys.join("devices"))?
            .join("multipath/nvme8c0n1/device")
            .to_string_lossy(),
        "pcie\n",
    )?;
    // One of two controllers whose transport cannot be read: not provably local.
    let s9 = subsystem(9)?;
    attach(&s9, "nvme9", &controller("pci9/nvme/nvme9", "pcie\n")?)?;
    attach(&s9, "nvme10", &make("pci10/nvme/nvme10")?)?;
    namespace(&s9, "nvme9n1")?;
    // Entries of the subsystem that are not controllers (`nvme-fabrics`, a
    // namespace's own directory) are not read, so they cannot make it remote.
    let s10 = subsystem(10)?;
    attach(&s10, "nvme11", &controller("pci11/nvme/nvme11", "pcie\n")?)?;
    make("virtual/nvme-subsystem/nvme-subsys10/nvme-fabrics")?;
    namespace(&s10, "nvme10n1")?;
    // A path under `multipath/` whose transport cannot be read, beside a PCIe
    // controller, and a subsystem whose own `transport` is not a file.
    let s11 = subsystem(11)?;
    attach(&s11, "nvme12", &controller("pci12/nvme/nvme12", "pcie\n")?)?;
    let ns11 = namespace(&s11, "nvme11n1")?;
    make(
        &ns11
            .strip_prefix(sys.join("devices"))?
            .join("multipath/nvme11c0n1/device")
            .to_string_lossy(),
    )?;
    let s12 = subsystem(12)?;
    attach(&s12, "nvme13", &controller("pci13/nvme/nvme13", "pcie\n")?)?;
    fs::create_dir(s12.join("transport"))?;
    namespace(&s12, "nvme12n1")?;
    // A directory that cannot be listed, as opposed to one that is not there:
    // `multipath` that is a file, and a namespace whose `device` is a file.
    let s13 = subsystem(13)?;
    attach(&s13, "nvme14", &controller("pci14/nvme/nvme14", "pcie\n")?)?;
    let ns13 = namespace(&s13, "nvme13n1")?;
    write(&ns13.join("multipath"), "")?;
    let file_device = make("virtual/nvme-subsystem/nvme-subsys14")?;
    write(&file_device.join("not-a-directory"), "")?;
    let ns14 = file_device.join("nvme14n1");
    fs::create_dir_all(&ns14)?;
    symlink("../not-a-directory", ns14.join("device"))?;
    symlink(&ns14, sys.join("class/block").join("nvme14n1"))?;
    let transports: Vec<Option<String>> = (0..15)
        .map(|n| disks::nvme_transport(&sys, &format!("nvme{n}n1")))
        .collect();
    let some = |t: &str| Some(t.to_owned());
    ensure!(
        transports
            == [
                some("pcie"),
                some("pcie"),
                some("pcie"),
                some("tcp"),
                some("rdma"),
                None,
                None,
                some("fc"),
                some("pcie"),
                None,
                some("pcie"),
                None,
                None,
                None,
                None,
            ],
        "NVMe transports: {transports:?}"
    );
    ensure!(
        disks::nvme_transport(&sys, "nvme99n1").is_none(),
        "a namespace sysfs does not list"
    );
    // Only the local ones survive lsblk's list, though lsblk says `nvme` of all.
    let lsblk = format!(
        r#"{{"blockdevices":[{}]}}"#,
        (0..15)
            .map(|n| format!(
                r#"{{"name":"/dev/nvme{n}n1","kname":"/dev/nvme{n}n1","type":"disk","size":16000000000,"tran":"nvme","rm":false,"ro":false,"log-sec":512}}"#
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let raws = disks::parse_lsblk(&lsblk, &|_| None, &|name| disks::nvme_transport(&sys, name))?;
    let local: Vec<&str> = raws.iter().map(|r| r.kname.as_str()).collect();
    ensure!(
        local == ["nvme0n1", "nvme1n1", "nvme2n1", "nvme8n1", "nvme10n1"],
        "local NVMe disks: {local:?}"
    );
    Ok(())
}

// --- the live medium and what is in use -------------------------------------

/// A sysfs and procfs tree of the shape the kernel makes, in a directory: block
/// devices below `sys/devices/virtual/block`, `class/block` and `dev/block`
/// links to them, `slaves/` and `holders/` links, loop devices' backing files,
/// zram's `backing_dev`, btrfs file systems' `devices/`, ext4's journals in
/// `proc/fs/jbd2/`; and the device nodes `stat` would show under `/dev` (nodes
/// cannot be made in a fixture: `stat` is a closure of the case).
struct Tree {
    root: PathBuf,
    /// `/dev/<path>` to the device it stands for.
    nodes: RefCell<BTreeMap<String, topology::DevNum>>,
}

impl Tree {
    fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("sys/class/block"))?;
        fs::create_dir_all(root.join("sys/dev/block"))?;
        fs::create_dir_all(root.join("sys/devices/virtual/block"))?;
        fs::create_dir_all(root.join("sys/fs/btrfs"))?;
        fs::create_dir_all(root.join("proc"))?;
        Ok(Self {
            root,
            nodes: RefCell::default(),
        })
    }

    fn roots(&self) -> Roots {
        Roots {
            sys: self.root.join("sys"),
            proc: self.root.join("proc"),
        }
    }

    /// The device directory below `devices/virtual/block`: `sdb` or `sdb/sdb1`.
    fn device(&self, name: &str, major: u32, minor: u32, parent: Option<&str>) -> Result<PathBuf> {
        let relative = parent.map_or(name.to_owned(), |p| format!("{p}/{name}"));
        let dir = self.root.join("sys/devices/virtual/block").join(&relative);
        fs::create_dir_all(&dir)?;
        let target = format!("../../devices/virtual/block/{relative}");
        symlink(&target, self.root.join("sys/class/block").join(name))?;
        symlink(
            &target,
            self.root
                .join("sys/dev/block")
                .join(format!("{major}:{minor}")),
        )?;
        write(&dir.join("dev"), &format!("{major}:{minor}\n"))?;
        self.nodes
            .borrow_mut()
            .insert(format!("/dev/{name}"), (major, minor));
        Ok(dir)
    }

    fn disk(&self, name: &str, major: u32, minor: u32) -> Result<()> {
        self.device(name, major, minor, None).map(|_| ())
    }

    fn partition(&self, disk: &str, name: &str, major: u32, minor: u32) -> Result<()> {
        let dir = self.device(name, major, minor, Some(disk))?;
        fs::write(dir.join("partition"), b"1\n")?;
        Ok(())
    }

    /// A device-mapper device built on `slaves` (`sdb2`, or `sdb/sdb2` for a
    /// partition's place in the tree).
    fn stacked(&self, name: &str, major: u32, minor: u32, slaves: &[&str]) -> Result<()> {
        let dir = self.device(name, major, minor, None)?;
        fs::create_dir_all(dir.join("slaves"))?;
        for slave in slaves {
            let leaf = slave.rsplit('/').next().unwrap_or(slave);
            symlink(format!("../../{slave}"), dir.join("slaves").join(leaf))?;
            // The device below also lists what is built on it.
            let below = self.root.join("sys/devices/virtual/block").join(slave);
            fs::create_dir_all(below.join("holders"))?;
            let depth = "../".repeat(slave.matches('/').count() + 2);
            symlink(format!("{depth}{name}"), below.join("holders").join(name))?;
        }
        Ok(())
    }

    /// A loop device. `backing` is what the kernel prints for its file
    /// (`loop/backing_file`): empty for a device attached to nothing.
    fn loop_device(&self, name: &str, major: u32, minor: u32, backing: &str) -> Result<()> {
        let dir = self.device(name, major, minor, None)?;
        write(
            &dir.join("loop/backing_file"),
            &if backing.is_empty() {
                String::new()
            } else {
                format!("{backing}\n")
            },
        )
    }

    /// A zram device; `backing` is what its `backing_dev` prints (`none`, or the
    /// writeback device), and `None` a kernel without writeback (no such file).
    fn zram(&self, name: &str, minor: u32, backing: Option<&str>) -> Result<()> {
        let dir = self.device(name, 252, minor, None)?;
        if let Some(backing) = backing {
            write(&dir.join("backing_dev"), &format!("{backing}\n"))?;
        }
        Ok(())
    }

    /// The journals ext4 lists in `/proc/fs/jbd2/`.
    fn journals(&self, entries: &[&str]) -> Result<()> {
        for entry in entries {
            fs::create_dir_all(self.root.join("proc/fs/jbd2").join(entry))?;
        }
        Ok(())
    }

    /// A btrfs file system and the devices it is made of.
    fn btrfs(&self, uuid: &str, members: &[&str]) -> Result<()> {
        for member in members {
            write(
                &self
                    .root
                    .join("sys/fs/btrfs")
                    .join(uuid)
                    .join("devices")
                    .join(member),
                "",
            )?;
        }
        Ok(())
    }

    /// A device node link under `/dev`: `target` names the device it leads to
    /// (its last component).
    fn node_link(&self, path: &str, target: &str) -> Result<()> {
        let device = target.rsplit('/').next().unwrap_or(target);
        let number = *self
            .nodes
            .borrow()
            .get(&format!("/dev/{device}"))
            .with_context(|| format!("no device {device} for the link {path}"))?;
        self.nodes
            .borrow_mut()
            .insert(format!("/dev/{path}"), number);
        Ok(())
    }

    /// What `stat` says of a device node of the tree.
    fn stat(&self, path: &str) -> Option<FileId> {
        let nodes = self.nodes.borrow();
        let number = nodes.get(path)?;
        Some(FileId {
            // devtmpfs, and a number of its own for the node's inode.
            dev: (0, 6),
            ino: 100_000 + u64::from(number.0) * 1000 + u64::from(number.1),
            block_device: Some(*number),
        })
    }
}

fn names(set: &BTreeSet<String>) -> Vec<&str> {
    set.iter().map(String::as_str).collect()
}

/// A bound loop device of a case: what sysfs says (the kernel's text), and the
/// row `losetup` prints for it, if it prints one.
struct Loop {
    name: &'static str,
    major: u32,
    minor: u32,
    sysfs: &'static str,
    row: Option<LoopRow>,
}

fn loop_row(
    name: &str,
    file: &str,
    inode: u64,
    device: Option<topology::DevNum>,
) -> Option<LoopRow> {
    Some(LoopRow {
        name: format!("/dev/{name}"),
        file: Some(file.to_owned()),
        inode: Some(inode),
        device,
    })
}

/// `stat`'s answer for a path of a case.
fn file_id(dev: topology::DevNum, ino: u64, block_device: Option<topology::DevNum>) -> FileId {
    FileId {
        dev,
        ino,
        block_device,
    }
}

// The mounts every running system has, as one has them: the root, `/dev` as
// devtmpfs, `/run` and `/tmp` as tmpfs (with the options systemd mounts them
// with), the kernel's interfaces and a desktop session's FUSE services.
const KERNEL_MOUNTS: &str = "\
11 1 0:22 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n\
12 1 0:23 / /sys rw,nosuid,nodev,noexec,relatime - sysfs sysfs rw\n\
13 1 0:6 / /dev rw,nosuid shared:2 - devtmpfs devtmpfs rw,size=4096k,nr_inodes=1048576,mode=755\n\
14 13 0:24 / /dev/pts rw,nosuid,noexec,relatime shared:3 - devpts devpts rw,gid=5,mode=620,ptmxmode=000\n\
15 13 0:25 / /dev/shm rw,nosuid,nodev shared:4 - tmpfs tmpfs rw\n\
16 1 0:31 / /run rw,nosuid,nodev shared:5 - tmpfs tmpfs rw,mode=755\n\
17 12 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime - cgroup2 cgroup2 rw\n\
18 12 0:27 / /sys/kernel/security rw,nosuid,nodev,noexec,relatime - securityfs securityfs rw\n\
19 12 0:29 / /sys/fs/fuse/connections rw,nosuid,nodev,noexec,relatime - fusectl fusectl rw\n\
20 1 0:32 / /run/user/1000/doc rw,nosuid,nodev,relatime - fuse.portal portal rw,user_id=1000\n\
21 1 0:33 / /run/user/1000/gvfs rw,nosuid,nodev,relatime - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000\n\
22 1 0:34 / /proc/sys/fs/binfmt_misc rw,relatime - autofs systemd-1 rw\n\
23 1 0:35 / /sys/kernel/debug rw,nosuid,nodev,noexec,relatime - debugfs debugfs rw\n\
24 13 0:36 / /dev/mqueue rw,nosuid,nodev,noexec,relatime - mqueue mqueue rw\n\
25 1 0:37 / /tmp rw,nosuid,nodev - tmpfs tmpfs rw\n";

/// What the ilex ISO boots into, read from the real ISO (written to a USB stick)
/// under QEMU with Secure Boot (`iso::next_helper_usb`, 2026-11-26): its squashfs
/// holds the root tree directly (`/proc` is a directory in it), so dracut's
/// dmsquash-live mounts an overlay root over the squashfs (`/run/rootfsbase`, on
/// `loop0`) with its upper directory in `/run` (tmpfs), and there is no
/// device-mapper device and no other loop device. The ISO's file system (on
/// `sda1`, the stick's first partition) is at `/run/initramfs/live`; `loop0` is
/// attached to `/LiveOS/squashfs.img` on it, a name the kernel prints relative
/// to that mount, so no name in this namespace confirms it.
const ROOT_OVERLAY: &str = "52 1 0:30 / / rw,relatime shared:1 - overlay LiveOS_rootfs rw,seclabel,lowerdir=/run/rootfsbase,upperdir=/run/overlayfs,workdir=/run/ovlwork,uuid=on\n";
const LIVE_MEDIUM: &str = "63 60 8:1 / /run/initramfs/live ro,relatime shared:17 - iso9660 /dev/sda1 ro,nojoliet,check=s,map=n,blocksize=2048,iocharset=utf8\n";
const LIVE_MEDIUM_OPTICAL: &str = "63 60 11:0 / /run/initramfs/live ro,relatime shared:17 - iso9660 /dev/sr0 ro,nojoliet,check=s,map=n,blocksize=2048,iocharset=utf8\n";
const ROOTFSBASE: &str = "64 60 7:0 / /run/rootfsbase ro,relatime shared:18 - squashfs /dev/loop0 ro,seclabel,errors=continue\n";
/// The name and inode the kernel gives the squashfs's file on the real ISO.
const SQUASHFS_FILE: &str = "/LiveOS/squashfs.img";
const SQUASHFS_INODE: u64 = 5126;
/// dracut's other layout, which the ilex ISO does **not** boot into: a squashfs
/// holding `LiveOS/rootfs.img` (a root `ext4` on `/dev/mapper/live-rw`, a
/// snapshot of `live-base`, which is the loop device `loop1` over `rootfs.img`
/// inside the squashfs) with its copy-on-write file `loop2` in the initramfs'
/// own memory, the file `/overlay (deleted)` on a device no mount carries once
/// the initramfs is gone. Kept so that the helper's answer to it is a test:
/// every disk refused.
const ROOT_LIVE_RW: &str = "1 0 253:0 / / rw,relatime - ext4 /dev/mapper/live-rw rw\n";
/// zram-generator's swap, as every live boot has it (writeback unset).
const ZRAM_SWAP: &str = "/dev/zram0 partition 8388604 0 100\n";

/// A change to a case's sysfs and procfs tree.
type TreeEdit<'a> = Box<dyn Fn(&Tree) -> Result<()> + 'a>;

/// What a case traces: the base layout plus its own mounts, swap entries,
/// loop devices and files.
struct Case<'a> {
    label: &'a str,
    /// The mounts before the case's own: the root, the kernel's, the medium's.
    base: String,
    mounts: &'a str,
    /// Swap rows after zram's.
    swaps: &'a str,
    /// Loop devices after the live image's own (`loop0`; with `live_rw` also
    /// `loop1` and `loop2`).
    loops: Vec<Loop>,
    /// The live image's loops' rows (`loop0`, then `loop1` and `loop2` with
    /// `live_rw`), replaced by these when given.
    live_rows: Option<Vec<Option<LoopRow>>>,
    /// What `stat` says of a path (device nodes of the tree are known besides).
    files: Vec<(&'static str, FileId)>,
    /// A path with its links resolved, when it is not its own.
    canonical: Vec<(&'static str, &'static str)>,
    /// The file system signature of a device (what lsblk would print).
    types: Vec<(&'static str, &'static str)>,
    /// Anything else the case adds to its sysfs and procfs.
    extra: Vec<TreeEdit<'a>>,
    /// dracut's other layout (a device-mapper snapshot of `rootfs.img`, with
    /// two more loop devices), not the overlay over the squashfs the ISO boots
    /// into.
    live_rw: bool,
}

impl<'a> Case<'a> {
    fn new(label: &'a str) -> Self {
        Self {
            label,
            base: format!("{ROOT_OVERLAY}{KERNEL_MOUNTS}{LIVE_MEDIUM}{ROOTFSBASE}"),
            mounts: "",
            swaps: "",
            loops: vec![],
            live_rows: None,
            files: vec![],
            canonical: vec![],
            // The live medium is an ISO; lsblk knows it by its signature.
            types: vec![("sda1", "iso9660"), ("loop0", "squashfs")],
            extra: vec![],
            live_rw: false,
        }
    }

    /// The ISO's layout with some of the live system's mounts gone (detached,
    /// as `umount -l` does), its loop device still attached.
    fn mounts_left(mut self, medium: bool, rootfsbase: bool) -> Self {
        self.base = format!(
            "{ROOT_OVERLAY}{KERNEL_MOUNTS}{}{}",
            if medium { LIVE_MEDIUM } else { "" },
            if rootfsbase { ROOTFSBASE } else { "" }
        );
        self
    }

    /// dracut's other layout, the one the ISO does not use.
    fn live_rw(mut self) -> Self {
        self.live_rw = true;
        self.base = format!("{ROOT_LIVE_RW}{KERNEL_MOUNTS}{LIVE_MEDIUM}{ROOTFSBASE}");
        self
    }

    fn mounts(mut self, mounts: &'a str) -> Self {
        self.mounts = mounts;
        self
    }

    /// Swap rows (`/dev/sde2 partition 1024 0 -2\n`), after zram's.
    fn swaps(mut self, swaps: &'a str) -> Self {
        self.swaps = swaps;
        self
    }

    fn loop_device(mut self, device: Loop) -> Self {
        self.loops.push(device);
        self
    }

    fn live_rows(mut self, rows: Vec<Option<LoopRow>>) -> Self {
        self.live_rows = Some(rows);
        self
    }

    fn file(mut self, path: &'static str, id: FileId) -> Self {
        self.files.push((path, id));
        self
    }

    fn canonical(mut self, path: &'static str, to: &'static str) -> Self {
        self.canonical.push((path, to));
        self
    }

    /// What lsblk knows of a device's signature, instead of the defaults.
    fn types(mut self, types: &[(&'static str, &'static str)]) -> Self {
        self.types = types.to_vec();
        self
    }

    fn device_type(mut self, name: &'static str, fstype: &'static str) -> Self {
        self.types.push((name, fstype));
        self
    }

    fn tree(mut self, extra: impl Fn(&Tree) -> Result<()> + 'a) -> Self {
        self.extra.push(Box::new(extra));
        self
    }

    /// Build the case's own sysfs and device tree and trace it.
    fn trace(&self, parent: &Path) -> Result<topology::Usage> {
        let dir = TestDir::new(parent, &format!("topology-{}", self.label))?;
        let tree = Tree::new(dir.0.join("tree"))?;
        // sda: the live USB stick; the ISO file system is on sda1 and carries
        // the squashfs (loop0). The other layout's rootfs.img (loop1) is
        // live-base and its copy-on-write file is loop2.
        tree.disk("sda", 8, 0)?;
        tree.partition("sda", "sda1", 8, 1)?;
        tree.partition("sda", "sda2", 8, 2)?;
        tree.loop_device("loop0", 7, 0, SQUASHFS_FILE)?;
        if self.live_rw {
            tree.loop_device("loop1", 7, 1, "/run/initramfs/squashfs/LiveOS/rootfs.img")?;
            tree.loop_device("loop2", 7, 2, "/overlay (deleted)")?;
            tree.stacked("dm-1", 253, 1, &["loop1"])?;
            tree.stacked("dm-0", 253, 0, &["dm-1", "loop2"])?;
            tree.node_link("mapper/live-base", "../dm-1")?;
            tree.node_link("mapper/live-rw", "../dm-0")?;
        }
        // zram-generator's device: swap in RAM, no writeback.
        tree.zram("zram0", 0, Some("none"))?;
        // sdb: Ventoy's stick; its second partition carries the dm device.
        tree.disk("sdb", 8, 16)?;
        tree.partition("sdb", "sdb1", 8, 17)?;
        tree.partition("sdb", "sdb2", 8, 18)?;
        tree.stacked("dm-4", 253, 4, &["sdb/sdb2"])?;
        tree.node_link("mapper/ventoy", "../dm-4")?;
        // sdc: the disk dracut's iso-scan found an ISO file on.
        tree.disk("sdc", 8, 32)?;
        tree.partition("sdc", "sdc1", 8, 33)?;
        // sdd: an optical-style medium, named by its label.
        tree.disk("sdd", 8, 48)?;
        tree.node_link("disk/by-label/ILEX", "../../sdd")?;
        // sde: in use: a mounted partition, swap, a stacked device.
        tree.disk("sde", 8, 64)?;
        for (n, minor) in [("sde1", 65), ("sde2", 66), ("sde3", 67)] {
            tree.partition("sde", n, 8, minor)?;
        }
        tree.stacked("dm-5", 253, 5, &["sde/sde3"])?;
        // sdf: nothing uses it.
        tree.disk("sdf", 8, 80)?;
        // sdg: a block device node that a loop device can be attached to.
        tree.disk("sdg", 8, 96)?;
        tree.partition("sdg", "sdg1", 8, 97)?;
        // sdh and sdi: the two devices of one btrfs; sdj: a single-device one,
        // known by its UUID and a label.
        tree.disk("sdh", 8, 112)?;
        tree.partition("sdh", "sdh1", 8, 113)?;
        tree.disk("sdi", 8, 128)?;
        tree.partition("sdi", "sdi1", 8, 129)?;
        tree.disk("sdj", 8, 144)?;
        tree.partition("sdj", "sdj1", 8, 145)?;
        tree.node_link("disk/by-label/DATA", "../../sdj1")?;
        // sdk: a disk only an unmounted loop device uses.
        tree.disk("sdk", 8, 160)?;
        tree.partition("sdk", "sdk1", 8, 161)?;
        // sdl: an xfs with its log on sdn; sdm: a FUSE block file system.
        tree.disk("sdl", 8, 176)?;
        tree.partition("sdl", "sdl1", 8, 177)?;
        tree.disk("sdm", 8, 192)?;
        tree.partition("sdm", "sdm1", 8, 193)?;
        tree.disk("sdn", 8, 208)?;
        tree.partition("sdn", "sdn1", 8, 209)?;
        tree.btrfs(UUID_POOL, &["sdh1", "sdi1"])?;
        tree.btrfs(UUID_DATA, &["sdj1"])?;
        for extra in &self.extra {
            extra(&tree)?;
        }
        let mut rows: Vec<Option<LoopRow>> = self.live_rows.clone().unwrap_or_else(|| {
            let mut rows = vec![loop_row(
                "loop0",
                SQUASHFS_FILE,
                SQUASHFS_INODE,
                Some((8, 1)),
            )];
            if self.live_rw {
                rows.push(loop_row(
                    "loop1",
                    "/run/initramfs/squashfs/LiveOS/rootfs.img",
                    200,
                    Some((7, 0)),
                ));
                // The initramfs' own file system is gone: no mount carries 0:23.
                rows.push(loop_row("loop2", "/overlay (deleted)", 300, Some((0, 23))));
            }
            rows
        });
        for device in &self.loops {
            tree.loop_device(device.name, device.major, device.minor, device.sysfs)?;
            rows.push(device.row.clone());
        }
        let mounts = topology::parse_mountinfo(&format!("{}{}", self.base, self.mounts))?;
        let system = System {
            local: mounts.clone(),
            mounts,
            swaps: topology::parse_swaps(&format!(
                "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n{ZRAM_SWAP}{}",
                self.swaps
            ))?,
            loops: rows.into_iter().flatten().collect(),
            unreadable: vec![],
        };
        let roots = tree.roots();
        let stat = |path: &str| {
            self.files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, id)| *id)
                .or_else(|| tree.stat(path))
        };
        let canonical = |path: &str| {
            Some(
                self.canonical
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map_or(path, |(_, to)| *to)
                    .to_owned(),
            )
        };
        let device_type = |name: &str| {
            self.types
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, t)| (*t).to_owned())
        };
        Ok(Tracer::new(&roots, &system)
            .with_stat(&stat)
            .with_canonical(&canonical)
            .with_device_types(&device_type)
            .usage())
    }
}

const UUID_POOL: &str = "0b4c1ab0-3d36-4f43-9a5e-6a8f7f6f1111";
const UUID_DATA: &str = "5d2e8f1c-7a3b-4c19-8e6d-2f9a0b3c2222";

fn loop_of(name: &'static str, minor: u32, sysfs: &'static str, row: Option<LoopRow>) -> Loop {
    Loop {
        name,
        major: 7,
        minor,
        sysfs,
        row,
    }
}

/// The disks and devices in use, without the swap on RAM every live boot has
/// (a use of `zram0` and of nothing on a disk).
fn in_use_names(usage: &topology::Usage) -> Vec<&str> {
    usage
        .in_use
        .keys()
        .map(String::as_str)
        .filter(|n| !n.starts_with("zram"))
        .collect()
}

/// A device node as `stat` shows it: a block special file of devtmpfs standing
/// for `number`.
fn node_of(ino: u64, number: topology::DevNum) -> FileId {
    file_id((0, 6), ino, Some(number))
}

/// What a loop device of `/live.iso` on the disk iso-scan found says about a
/// name that was reused: the case's mounts and signatures decide what the
/// unconfirmed file might be.
fn iso_by_name(
    parent: &Path,
    label: &str,
    mounts: &str,
    sdc1: Option<&'static str>,
    file: &'static str,
    id: Option<FileId>,
) -> Result<topology::Usage> {
    let mut case = Case::new(label).mounts(mounts).loop_device(loop_of(
        "loop4",
        4,
        file,
        loop_row("loop4", file, 9, Some((8, 33))),
    ));
    if let Some(sdc1) = sdc1 {
        case = case.device_type("sdc1", sdc1);
    }
    if let Some(id) = id {
        case = case.file(file, id);
    }
    case.trace(parent)
}

/// The live medium and the disks in use, found by following every use of block
/// storage through the kernel's tree and never through the text of a name or a
/// path's prefix. Each case runs beside the mounts a live system has (the root,
/// `/dev` as devtmpfs, `/run` and `/tmp` as tmpfs, the kernel's interfaces, a
/// desktop session's FUSE services, zram's swap, dracut's live layout): `/`
/// contains every path and the detached or colliding name of a file is a trap,
/// so a case that works only without them proves nothing.
fn check_topology(parent: &Path) -> Result<()> {
    // The layout of the real ISO alone: everything resolves, to the stick and
    // nothing else (the loop device's file is named `/LiveOS/squashfs.img`,
    // which no name here confirms: the stick's ISO file system carries it).
    let live = Case::new("live").trace(parent)?;
    ensure!(
        names(&live.live) == ["loop0", "sda", "sda1"]
            && in_use_names(&live).is_empty()
            && live.unresolved.is_empty(),
        "the dmsquash-live layout of the ISO: {live:?}"
    );
    // ... and the swap on RAM is a use of RAM.
    ensure!(
        live.in_use.keys().all(|n| n.starts_with("zram")),
        "zram's swap reaches a disk: {live:?}"
    );
    // The same ISO in an optical drive (`iso::next_helper_cdrom`): the medium is
    // `sr0`, which has no partition, and the file is on device 11:0.
    let optical = Case::new("optical")
        .mounts_left(false, true)
        .mounts(LIVE_MEDIUM_OPTICAL)
        .live_rows(vec![loop_row("loop0", SQUASHFS_FILE, 4102, Some((11, 0)))])
        .tree(|tree| tree.disk("sr0", 11, 0))
        .trace(parent)?;
    ensure!(
        names(&optical.live) == ["loop0", "sr0"]
            && in_use_names(&optical).is_empty()
            && optical.unresolved.is_empty(),
        "the ISO in an optical drive: {optical:?}"
    );
    // dracut's other layout (not the ISO's): the copy-on-write file of the
    // device-mapper snapshot is a deleted file of the initramfs' own file
    // system, which nothing carries any more. It could be a device node, so
    // it is not resolved: every disk refused.
    let live_rw = Case::new("live-rw").live_rw().trace(parent)?;
    ensure!(
        live_rw.unresolved.iter().any(|u| u.contains("loop2")),
        "the copy-on-write file of a device-mapper snapshot was traced: {live_rw:?}"
    );

    // --- A: the live medium's mount detached, its loop device still bound.
    // Anyone with `umount -l /run/initramfs/live` does it; the kernel prints
    // `/LiveOS/…` for the file whatever the mount, a name `/` does not have
    // here. The stick is found by the device number the kernel recorded, not
    // by any name; the file's name does not confirm it, so it is accepted
    // because the device is an ISO (lsblk's signature).
    let detached_medium = Case::new("lazy-live")
        .mounts_left(false, true)
        .trace(parent)?;
    ensure!(
        detached_medium.live.contains("sda")
            && detached_medium.live.contains("sda1")
            && detached_medium.unresolved.is_empty()
            && in_use_names(&detached_medium).is_empty(),
        "a lazily unmounted /run/initramfs/live: {detached_medium:?}"
    );
    // ... but not when nothing says what the detached file system was, or says
    // it was one that can hold a device node.
    for (what, types) in [
        ("unknown", &[("loop0", "squashfs")][..]),
        ("ext4", &[("sda1", "ext4"), ("loop0", "squashfs")][..]),
    ] {
        let case = Case::new("lazy-live-type")
            .mounts_left(false, true)
            .types(types)
            .trace(parent)?;
        ensure!(
            !case.unresolved.is_empty(),
            "a detached file system of {what} type was taken for an image: {case:?}"
        );
    }
    // ... and with neither the medium's mount nor the squashfs's: the loop
    // device is attached and nothing mounts it. The stick is in use (an
    // unmounted bound loop is a use of its file's disk), no longer the live
    // medium, and so refused either way.
    let bare_loops = Case::new("bare-loops")
        .mounts_left(false, false)
        .trace(parent)?;
    ensure!(
        in_use_names(&bare_loops).contains(&"sda")
            && in_use_names(&bare_loops).contains(&"sda1")
            && !in_use_names(&bare_loops).contains(&"sdb")
            && bare_loops.unresolved.is_empty(),
        "bound loop devices nothing mounts: {bare_loops:?}"
    );
    // The same chain with one loop's file unknown to losetup: every disk is in
    // use (the stick is not left looking free).
    let unknown = Case::new("lazy-unknown")
        .mounts_left(false, true)
        .live_rows(vec![Some(LoopRow {
            name: "/dev/loop0".into(),
            file: Some(SQUASHFS_FILE.into()),
            inode: Some(SQUASHFS_INODE),
            device: None,
        })])
        .trace(parent)?;
    ensure!(
        !unknown.unresolved.is_empty() && !unknown.live.contains("sda"),
        "an unknown device is taken for resolved: {unknown:?}"
    );
    // A bound loop that losetup does not list at all.
    let unlisted = Case::new("lazy-unlisted")
        .mounts_left(false, true)
        .live_rows(vec![None])
        .trace(parent)?;
    ensure!(
        unlisted.unresolved.iter().any(|u| u.contains("loop0")),
        "a loop sysfs binds and losetup omits: {unlisted:?}"
    );
    // The live medium's own mount, on a device nothing lists, is a failure to
    // trace like any other mount.
    let lost_mount = Case::new("live-unlisted")
        .mounts("49 16 8:250 / /run/initramfs/isoscan ro - ext4 none ro\n")
        .trace(parent)?;
    ensure!(
        !lost_mount.unresolved.is_empty(),
        "a device number sysfs does not list: {lost_mount:?}"
    );

    // --- B: a loop device attached to a block device node (`losetup /dev/sdg1`).
    // `lo_device` is `st_dev`, so the kernel reports the device of `/dev` (the
    // devtmpfs mount 0:6) and the inode of the node; the disk is `st_rdev`.
    let node = node_of(700, (8, 97));
    let on_node = Case::new("node")
        .loop_device(loop_of(
            "loop3",
            3,
            "/dev/sdg1",
            loop_row("loop3", "/dev/sdg1", 700, Some((0, 6))),
        ))
        .file("/dev/sdg1", node)
        .trace(parent)?;
    ensure!(
        names(&on_node.live) == ["loop0", "sda", "sda1"]
            && in_use_names(&on_node).contains(&"sdg")
            && in_use_names(&on_node).contains(&"sdg1")
            && on_node.unresolved.is_empty(),
        "a loop device on /dev/sdg1: {on_node:?}"
    );
    ensure!(
        on_node.in_use["sdg"]
            .iter()
            .any(|r| r == "/dev/loop3 is attached to /dev/sdg1"),
        "the reason: {:?}",
        on_node.in_use["sdg"]
    );
    // The node is not what the kernel recorded (replaced, renamed, deleted), or
    // is a plain file on devtmpfs: devtmpfs is never proof of "no disk".
    for (what, path, id) in [
        ("another inode", "/dev/sdg1", Some(node_of(701, (8, 97)))),
        (
            "a regular file",
            "/dev/sdg1",
            Some(file_id((0, 6), 700, None)),
        ),
        ("gone", "/dev/gone", None),
    ] {
        let mut case = Case::new("node-lost").loop_device(loop_of(
            "loop3",
            3,
            path,
            loop_row("loop3", path, 700, Some((0, 6))),
        ));
        if let Some(id) = id {
            case = case.file(path, id);
        }
        let lost = case.trace(parent)?;
        ensure!(
            !lost.unresolved.is_empty(),
            "a loop on {path} that is {what} was taken for resolved: {lost:?}"
        );
    }
    // A device node made on another file system (here sdc1's): both disks.
    let moved = Case::new("node-elsewhere")
        .loop_device(loop_of(
            "loop3",
            3,
            "/mnt/node",
            loop_row("loop3", "/mnt/node", 31, Some((8, 33))),
        ))
        .file("/mnt/node", file_id((8, 33), 31, Some((8, 97))))
        .trace(parent)?;
    ensure!(
        in_use_names(&moved).contains(&"sdc") && in_use_names(&moved).contains(&"sdg"),
        "a node on another file system: {moved:?}"
    );

    // --- C: a file system that is on a disk but whose mount table entry names
    // no device: by UUID or LABEL, a block file system with an anonymous
    // number, a type the installer does not know.
    let by_uuid = Case::new("btrfs-uuid")
        .mounts(&format!(
            "50 1 0:70 / /data rw - btrfs UUID={UUID_DATA} rw\n"
        ))
        .trace(parent)?;
    ensure!(
        in_use_names(&by_uuid) == ["sdj", "sdj1"] && by_uuid.unresolved.is_empty(),
        "btrfs mounted by UUID: {by_uuid:?}"
    );
    let by_label = Case::new("label")
        .mounts("50 1 0:71 / /data rw - ext4 LABEL=DATA rw\n")
        .trace(parent)?;
    ensure!(
        in_use_names(&by_label) == ["sdj", "sdj1"],
        "a source by LABEL: {by_label:?}"
    );
    let fuse_block = Case::new("fuseblk")
        .mounts("50 1 0:72 / /ntfs rw - fuseblk /dev/sdm1 rw\n")
        .trace(parent)?;
    ensure!(
        in_use_names(&fuse_block) == ["sdm", "sdm1"],
        "fuseblk on a device node: {fuse_block:?}"
    );
    for (what, line) in [
        (
            "a btrfs whose UUID sysfs does not list",
            "50 1 0:73 / /data rw - btrfs UUID=00000000-0000-0000-0000-000000000000 rw\n",
        ),
        ("zfs", "50 1 0:74 / /tank rw - zfs tank/data rw\n"),
        (
            "a FUSE file system of any other name",
            "50 1 0:75 / /fuse rw - fuse.fuse2fs fuse2fs rw\n",
        ),
        (
            "an unknown type",
            "50 1 0:76 / /x rw - frobnicatefs frob rw\n",
        ),
        (
            "a block file system with no number and no node",
            "50 1 0:77 / /y rw - ext4 none rw\n",
        ),
        (
            "an optical file system with no device",
            "50 16 0:60 / /run/media/x ro - udf none ro\n",
        ),
        (
            "a device node that is not there",
            "50 16 0:9 / /run/initramfs/live ro - iso9660 /dev/missing ro\n",
        ),
        (
            "a device number sysfs does not list",
            "50 16 8:250 / /mnt/x ro - ext4 none ro\n",
        ),
        // --- the session's FUSE services, but only where the session mounts them
        (
            "the document portal anywhere else",
            "50 1 0:81 / /mnt/doc rw,nosuid,nodev - fuse.portal portal rw,user_id=1000\n",
        ),
        (
            "the document portal under a name that is no uid",
            "50 1 0:82 / /run/user/abc/doc rw,nosuid,nodev - fuse.portal portal rw\n",
        ),
        (
            "the document portal below its directory",
            "50 1 0:83 / /run/user/1000/doc/x rw,nosuid,nodev - fuse.portal portal rw\n",
        ),
        (
            "gvfs at the portal's place",
            "50 1 0:84 / /run/user/1000/doc rw,nosuid,nodev - fuse.gvfsd-fuse gvfsd-fuse rw\n",
        ),
        (
            "gvfs in a user's home",
            "50 1 0:85 / /home/u/gvfs rw,nosuid,nodev - fuse.gvfsd-fuse gvfsd-fuse rw\n",
        ),
    ] {
        let case = Case::new("untraced").mounts(line).trace(parent)?;
        ensure!(
            !case.unresolved.is_empty(),
            "{what} is taken for no disk at all: {case:?}"
        );
    }
    // An external log or journal is a use of its device too.
    let log = Case::new("xfs-log")
        .mounts("50 1 8:177 / /xfs rw - xfs /dev/sdl1 rw,logdev=/dev/sdn1,noquota\n")
        .trace(parent)?;
    ensure!(
        in_use_names(&log).contains(&"sdl") && in_use_names(&log).contains(&"sdn"),
        "xfs with an external log: {log:?}"
    );
    let journal = Case::new("journal-dev")
        .mounts("50 1 8:177 / /ext rw - ext4 /dev/sdl1 rw,journal_dev=0xd001\n")
        .trace(parent)?;
    ensure!(
        !journal.unresolved.is_empty(),
        "an external journal given by number: {journal:?}"
    );

    // --- 7: a device node is the device its `st_rdev` says, not the one its
    // name suggests. A node named for sdf is sdn's, a swap area on a node named
    // for sdf is sde's, a mount source named for sdf is sdm's.
    let lying_log = Case::new("lying-logdev")
        .mounts("50 1 8:177 / /xfs rw - xfs /dev/sdl1 rw,logdev=/dev/sdf1\n")
        .file("/dev/sdf1", node_of(7, (8, 209)))
        .trace(parent)?;
    ensure!(
        in_use_names(&lying_log).contains(&"sdn")
            && !in_use_names(&lying_log).contains(&"sdf")
            && lying_log.unresolved.is_empty(),
        "a log device named for another: {lying_log:?}"
    );
    let lying_source = Case::new("lying-source")
        .mounts("50 1 0:72 / /ntfs rw - fuseblk /dev/sdf1 rw\n")
        .file("/dev/sdf1", node_of(8, (8, 193)))
        .trace(parent)?;
    ensure!(
        in_use_names(&lying_source) == ["sdm", "sdm1"],
        "a mount source named for another: {lying_source:?}"
    );
    let lying_swap = Case::new("lying-swap")
        .swaps("/dev/sdf partition 1024 0 -2\n")
        .file("/dev/sdf", node_of(9, (8, 66)))
        .trace(parent)?;
    ensure!(
        in_use_names(&lying_swap).contains(&"sde")
            && in_use_names(&lying_swap).contains(&"sde2")
            && !in_use_names(&lying_swap).contains(&"sdf")
            && lying_swap.unresolved.is_empty(),
        "a swap area on a node named for another: {lying_swap:?}"
    );
    ensure!(
        lying_swap.in_use["sde"]
            .iter()
            .any(|r| r == "/dev/sdf is in use as swap"),
        "its reason: {:?}",
        lying_swap.in_use["sde"]
    );
    // A swap area is "partition" or "file" in `/proc/swaps`' Type column: a
    // partition must be a block device node; a file is on an unknown file
    // system; any other kind is not known.
    for (what, swaps, id) in [
        (
            "a swap file",
            "/mnt/data/swap\\040file file 1024 0 -3\n",
            None,
        ),
        (
            "a swap device that is not there",
            "/dev/nowhere partition 1024 0 -2\n",
            None,
        ),
        (
            "a swap partition that is a regular file",
            "/mnt/swapfile partition 1024 0 -2\n",
            Some(("/mnt/swapfile", file_id((253, 0), 5, None))),
        ),
        (
            "a swap area of a kind nobody knows",
            "/dev/sde2 frobnicated 1024 0 -2\n",
            None,
        ),
    ] {
        let mut case = Case::new("swap").swaps(swaps);
        if let Some((path, id)) = id {
            case = case.file(path, id);
        }
        let case = case.trace(parent)?;
        ensure!(
            !case.unresolved.is_empty(),
            "{what} is taken for no disk at all: {case:?}"
        );
    }

    // --- D, 1: a loop device's file is told by the kernel's device and inode,
    // not by the name. A file the name no longer confirms (reused for another
    // file, deleted, renamed, under a mount) can still be a block device node
    // of any disk, which no device number says: the use is unresolved unless a
    // node could not have been opened there (a `nodev` mount) or cannot be
    // there (an image). The disk the file system is on is in use all the same.
    let iso = "/live.iso";
    let reused = iso_by_name(
        parent,
        "collide",
        "",
        None,
        iso,
        // `/live.iso` is, today, another file on the root's file system.
        Some(file_id((253, 0), 1234, None)),
    )?;
    ensure!(
        in_use_names(&reused).contains(&"sdc") && !reused.unresolved.is_empty(),
        "a colliding name on a disk file system: {reused:?}"
    );
    let deleted = iso_by_name(
        parent,
        "deleted",
        "",
        None,
        "/mnt/host/live.iso (deleted)",
        None,
    )?;
    ensure!(
        in_use_names(&deleted).contains(&"sdc") && !deleted.unresolved.is_empty(),
        "a deleted file on a file system that can hold a node: {deleted:?}"
    );
    for (what, mounts, type_of_sdc1) in [
        (
            "an ext4 mounted nodev",
            "51 1 8:33 / /mnt/host rw,nosuid,nodev,relatime - ext4 /dev/sdc1 rw\n",
            None,
        ),
        ("an ISO that is not mounted", "", Some("iso9660")),
        (
            "an ISO that is mounted",
            "51 1 8:33 / /mnt/iso ro,relatime - iso9660 /dev/sdc1 ro\n",
            None,
        ),
        (
            "a squashfs that is mounted",
            "51 1 8:33 / /mnt/sq ro,relatime - squashfs /dev/sdc1 ro\n",
            None,
        ),
    ] {
        for (state, file, id) in [
            ("renamed", iso, Some(file_id((253, 0), 1234, None))),
            ("deleted", "/mnt/host/live.iso (deleted)", None),
        ] {
            let case = iso_by_name(parent, "no-nodes", mounts, type_of_sdc1, file, id)?;
            // (a mounted ISO is the live medium, not a disk in use)
            ensure!(
                (in_use_names(&case).contains(&"sdc") || case.live.contains("sdc"))
                    && case.unresolved.is_empty(),
                "a {state} file on {what}: {case:?}"
            );
        }
    }
    let on_ext4 = iso_by_name(
        parent,
        "ext4",
        "51 1 8:33 / /mnt/host rw,relatime - ext4 /dev/sdc1 rw\n",
        None,
        "/mnt/host/live.iso (deleted)",
        None,
    )?;
    ensure!(
        !on_ext4.unresolved.is_empty(),
        "a deleted file on a mounted ext4: {on_ext4:?}"
    );
    // dracut's iso-scan: the kernel prints the loop device's file relative to
    // its file system's root (`/live.iso`), a name this system's `/` does not
    // have, and the disk is mounted where iso-scan mounts it. The file is
    // found below that mount by the numbers the kernel recorded, not by the
    // name; a file at that path that is not the recorded one, a path with
    // nothing at it, and a mount of another device are not.
    let iso_scan = |label: &str, mounts: &str, below: Option<FileId>| {
        let mut case = Case::new(label).mounts(mounts).loop_device(loop_of(
            "loop4",
            4,
            iso,
            loop_row("loop4", iso, 9, Some((8, 33))),
        ));
        if let Some(id) = below {
            case = case.file("/run/initramfs/isoscan/live.iso", id);
        }
        case.trace(parent)
    };
    let isoscan_mount =
        "64 16 8:33 / /run/initramfs/isoscan ro,relatime shared:17 - ext4 /dev/sdc1 ro\n";
    let found = iso_scan("iso-scan", isoscan_mount, Some(file_id((8, 33), 9, None)))?;
    // (the disk iso-scan mounted is the live medium, as dracut's own mounts say)
    ensure!(
        found.live.contains("sdc") && found.live.contains("sdc1") && found.unresolved.is_empty(),
        "a file found below the host disk's own mount: {found:?}"
    );
    // ... and it is a device node if that is what is there: the disk it
    // stands for is in use too.
    let node = iso_scan(
        "iso-scan-node",
        isoscan_mount,
        Some(file_id((8, 33), 9, Some((8, 208)))),
    )?;
    ensure!(
        node.live.contains("sdc")
            && in_use_names(&node).contains(&"sdn")
            && node.unresolved.is_empty(),
        "a device node found below the host disk's mount: {node:?}"
    );
    for (what, mounts, below) in [
        (
            "another inode at that path",
            isoscan_mount,
            Some(file_id((8, 33), 8, None)),
        ),
        (
            "another device at that path",
            isoscan_mount,
            Some(file_id((8, 49), 9, None)),
        ),
        ("nothing at that path", isoscan_mount, None),
        (
            "no mount of that device",
            "64 16 8:49 / /run/initramfs/isoscan ro,relatime - ext4 /dev/sdd1 ro\n",
            Some(file_id((8, 33), 9, None)),
        ),
    ] {
        let lost = iso_scan("iso-scan-lost", mounts, below)?;
        ensure!(
            !lost.unresolved.is_empty(),
            "a file with {what} was taken for resolved: {lost:?}"
        );
    }
    // The same file named by a path that is confirmed is not in doubt.
    let kept = iso_by_name(
        parent,
        "kept",
        "51 1 8:33 / /mnt/host rw,relatime - ext4 /dev/sdc1 rw\n",
        None,
        "/mnt/host/live.iso",
        Some(file_id((8, 33), 9, None)),
    )?;
    ensure!(
        in_use_names(&kept).contains(&"sdc") && kept.unresolved.is_empty(),
        "a file that is where the kernel says: {kept:?}"
    );
    // An anonymous number that no mount carries is a btrfs subvolume only if
    // the name is confirmed to be the file, by a name with no link in it; a
    // colliding name is not, and neither is a name whose text sends the lookup
    // to other mounts.
    let subvolume = |label: &str, id: Option<FileId>, canonical: Option<&'static str>| {
        let mut case = Case::new(label)
            .mounts("50 1 0:70 / /mnt rw,relatime - btrfs /dev/sdj1 rw\n")
            .loop_device(loop_of(
                "loop4",
                4,
                "/mnt/sub/live.iso",
                loop_row("loop4", "/mnt/sub/live.iso", 9, Some((0, 99))),
            ));
        if let Some(id) = id {
            case = case.file("/mnt/sub/live.iso", id);
        }
        if let Some(canonical) = canonical {
            case = case.canonical("/mnt/sub/live.iso", canonical);
        }
        case.trace(parent)
    };
    let confirmed = subvolume("subvolume", Some(file_id((0, 99), 9, None)), None)?;
    ensure!(
        in_use_names(&confirmed).contains(&"sdj") && confirmed.unresolved.is_empty(),
        "a btrfs subvolume's file: {confirmed:?}"
    );
    for (what, id, canonical) in [
        ("another inode", Some(file_id((0, 99), 8, None)), None),
        ("another device", Some(file_id((0, 98), 9, None)), None),
        ("gone", None, None),
        // The name leads through a link: `/mnt/sub/...` is not where the file
        // is, and the mounts that contain the text are not its mounts.
        (
            "reached through a link",
            Some(file_id((0, 99), 9, None)),
            Some("/srv/elsewhere/live.iso"),
        ),
    ] {
        let lost = subvolume("subvolume-lost", id, canonical)?;
        ensure!(
            !lost.unresolved.is_empty(),
            "a subvolume file that is {what} was taken for resolved: {lost:?}"
        );
    }
    // A file in RAM holds no disk; a file on a remote file system neither; one
    // on an overlay has its layers' mounts, not a place of its own. Where the
    // name confirms the file it is a regular file; where it does not, a block
    // device node may be there unless the mount is nodev.
    for (what, device, mounts, confirmed, resolved) in [
        ("tmpfs (nodev), unconfirmed", (0, 31), "", false, true),
        ("tmpfs (nodev), confirmed", (0, 31), "", true, true),
        (
            "tmpfs, unconfirmed",
            (0, 80),
            "50 1 0:80 / /mnt/t rw,relatime - tmpfs none rw\n",
            false,
            false,
        ),
        (
            "tmpfs, confirmed",
            (0, 80),
            "50 1 0:80 / /mnt/t rw,relatime - tmpfs none rw\n",
            true,
            true,
        ),
        (
            "nfs, unconfirmed",
            (0, 90),
            "50 1 0:90 / /nfs rw,relatime - nfs4 server:/share rw\n",
            false,
            false,
        ),
        (
            "nfs (nodev), unconfirmed",
            (0, 90),
            "50 1 0:90 / /nfs rw,nodev,relatime - nfs4 server:/share rw\n",
            false,
            true,
        ),
        (
            "nfs, confirmed",
            (0, 90),
            "50 1 0:90 / /nfs rw,relatime - nfs4 server:/share rw\n",
            true,
            true,
        ),
        ("overlay, confirmed", (0, 30), "", true, false),
    ] {
        let mut case = Case::new("ram").mounts(mounts).loop_device(loop_of(
            "loop4",
            4,
            "/x/live.iso",
            loop_row("loop4", "/x/live.iso", 9, Some(device)),
        ));
        if confirmed {
            case = case.file("/x/live.iso", file_id(device, 9, None));
        }
        let case = case.trace(parent)?;
        ensure!(
            case.unresolved.is_empty() == resolved
                && in_use_names(&case).iter().all(|n| n.starts_with("loop")),
            "an ISO file on {what}: {case:?}"
        );
    }
    // The attack the numbers cannot show: a node of a disk made on a tmpfs that
    // is not `nodev`, attached to a loop device and deleted
    // (`mknod /tmp/n b 8 97; losetup -f /tmp/n; rm /tmp/n`), leaves a file the
    // kernel records as an inode on tmpfs. It is not "a file in RAM".
    let hidden_node = Case::new("deleted-node")
        .mounts("50 1 0:80 / /mnt/t rw,relatime - tmpfs none rw\n")
        .loop_device(loop_of(
            "loop4",
            4,
            "/mnt/t/n (deleted)",
            loop_row("loop4", "/mnt/t/n (deleted)", 5, Some((0, 80))),
        ))
        .trace(parent)?;
    ensure!(
        !hidden_node.unresolved.is_empty(),
        "a deleted block node on a tmpfs was taken for a file in RAM: {hidden_node:?}"
    );

    // --- E: a loop device nothing mounts is a use of its file's disk.
    let unmounted = Case::new("unmounted")
        .loop_device(loop_of(
            "loop5",
            5,
            "/mnt/x/live.iso",
            loop_row("loop5", "/mnt/x/live.iso", 11, Some((8, 161))),
        ))
        .file("/mnt/x/live.iso", file_id((8, 161), 11, None))
        .trace(parent)?;
    ensure!(
        in_use_names(&unmounted) == ["loop5", "sdk", "sdk1"] && unmounted.unresolved.is_empty(),
        "an attached loop device nothing mounts: {unmounted:?}"
    );
    ensure!(
        unmounted.in_use["sdk"]
            .iter()
            .any(|r| r == "/dev/loop5 is attached to /mnt/x/live.iso"),
        "the reason: {:?}",
        unmounted.in_use["sdk"]
    );
    // Bound, but losetup shows nothing, or something unfinished.
    for (what, row) in [
        ("no row", None),
        (
            "no device",
            Some(LoopRow {
                name: "/dev/loop5".into(),
                file: Some("/mnt/x/live.iso".into()),
                inode: Some(11),
                device: None,
            }),
        ),
        (
            "no inode",
            Some(LoopRow {
                name: "/dev/loop5".into(),
                file: Some("/mnt/x/live.iso".into()),
                inode: None,
                device: Some((8, 161)),
            }),
        ),
        (
            "no file",
            Some(LoopRow {
                name: "/dev/loop5".into(),
                file: None,
                inode: Some(11),
                device: Some((8, 161)),
            }),
        ),
    ] {
        let case = Case::new("unfinished")
            .loop_device(loop_of("loop5", 5, "/mnt/x/live.iso", row))
            .trace(parent)?;
        ensure!(
            !case.unresolved.is_empty(),
            "an attached loop device with {what} is taken for resolved: {case:?}"
        );
    }
    // A loop device attached to nothing uses nothing.
    let free = Case::new("free-loop")
        .loop_device(loop_of("loop5", 5, "", None))
        .trace(parent)?;
    ensure!(
        in_use_names(&free).is_empty() && free.unresolved.is_empty(),
        "an unattached loop device: {free:?}"
    );
    // A loop on a loop on a disk, mounted: followed to the disk.
    let nested = Case::new("nested")
        .mounts("50 1 7:6 / /mnt/inner ro,relatime - squashfs /dev/loop6 ro\n")
        .loop_device(loop_of(
            "loop5",
            5,
            "/mnt/x/outer.img",
            loop_row("loop5", "/mnt/x/outer.img", 11, Some((8, 161))),
        ))
        .loop_device(loop_of(
            "loop6",
            6,
            "/outer/inner.img",
            loop_row("loop6", "/outer/inner.img", 12, Some((7, 5))),
        ))
        .file("/mnt/x/outer.img", file_id((8, 161), 11, None))
        .file("/outer/inner.img", file_id((7, 5), 12, None))
        .trace(parent)?;
    ensure!(
        in_use_names(&nested).contains(&"sdk") && nested.unresolved.is_empty(),
        "a loop on a loop: {nested:?}"
    );
    ensure!(
        nested.in_use["sdk"]
            .iter()
            .any(|r| r.ends_with("is mounted at /mnt/inner")),
        "its reason: {:?}",
        nested.in_use["sdk"]
    );

    // --- F: a btrfs of several devices is on all of them.
    let pool = Case::new("pool")
        .mounts("50 1 0:51 / /pool rw - btrfs /dev/sdh1 rw\n")
        .trace(parent)?;
    ensure!(
        in_use_names(&pool) == ["sdh", "sdh1", "sdi", "sdi1"] && pool.unresolved.is_empty(),
        "a two-device btrfs: {pool:?}"
    );
    ensure!(
        pool.in_use["sdi"]
            .iter()
            .any(|r| r == "/dev/sdh1 is mounted at /pool"),
        "its reason: {:?}",
        pool.in_use["sdi"]
    );
    let pool_by_uuid = Case::new("pool-uuid")
        .mounts(&format!(
            "50 1 0:51 / /pool rw - btrfs UUID={UUID_POOL} rw\n"
        ))
        .trace(parent)?;
    ensure!(
        in_use_names(&pool_by_uuid) == ["sdh", "sdh1", "sdi", "sdi1"],
        "a two-device btrfs by UUID: {pool_by_uuid:?}"
    );

    // --- 2: zram writes back to a block device it holds exclusively and that
    // shows no link in sysfs. Every zram device is read, whether or not it is a
    // swap area or mounted; none, empty and an absent file (a kernel without
    // writeback) are not a use.
    let writeback = Case::new("zram-writeback")
        .tree(|t| t.zram("zram1", 1, Some("/dev/sdk1")))
        .trace(parent)?;
    ensure!(
        in_use_names(&writeback) == ["sdk", "sdk1"] && writeback.unresolved.is_empty(),
        "zram writeback to sdk1: {writeback:?}"
    );
    ensure!(
        writeback.in_use["sdk"]
            .iter()
            .any(|r| r == "/dev/zram1 writes back to /dev/sdk1"),
        "its reason: {:?}",
        writeback.in_use["sdk"]
    );
    let no_writeback = Case::new("zram-none")
        .tree(|t| {
            t.zram("zram1", 1, Some("none"))?;
            t.zram("zram2", 2, Some(""))?;
            t.zram("zram3", 3, None)
        })
        .trace(parent)?;
    ensure!(
        in_use_names(&no_writeback).is_empty() && no_writeback.unresolved.is_empty(),
        "zram without writeback: {no_writeback:?}"
    );
    for (what, setup) in [
        (
            "a device that is not there",
            Box::new(|t: &Tree| t.zram("zram1", 1, Some("/dev/nowhere")))
                as Box<dyn Fn(&Tree) -> Result<()>>,
        ),
        (
            "a file that is not a block device",
            Box::new(|t: &Tree| t.zram("zram1", 1, Some("/dev/regular"))),
        ),
        (
            "a backing_dev that cannot be read",
            Box::new(|t: &Tree| {
                let dir = t.device("zram1", 252, 1, None)?;
                fs::create_dir(dir.join("backing_dev"))?;
                Ok(())
            }),
        ),
    ] {
        let case = Case::new("zram-unresolved")
            .file("/dev/regular", file_id((0, 6), 3, None))
            .tree(setup)
            .trace(parent)?;
        ensure!(
            !case.unresolved.is_empty(),
            "zram writing back to {what} was taken for none: {case:?}"
        );
    }

    // --- 4: an ext4 external journal never shows in the mount options. Its
    // device is in `/proc/fs/jbd2/` under its own name; a journal inside a file
    // system is `<device>-<inode>`. A name that is both a device and looks like
    // one with an inode (`dm-5`) is the device.
    let journals = Case::new("jbd2")
        .tree(|t| t.journals(&["sdl1-8", "dm-5-8", "sdn1"]))
        .trace(parent)?;
    ensure!(
        in_use_names(&journals) == ["sdn", "sdn1"] && journals.unresolved.is_empty(),
        "an external journal on sdn1: {journals:?}"
    );
    ensure!(
        journals.in_use["sdn"]
            .iter()
            .any(|r| r == "/dev/sdn1 holds the journal of a mounted file system"),
        "its reason: {:?}",
        journals.in_use["sdn"]
    );
    let stacked_journal = Case::new("jbd2-dm")
        .tree(|t| t.journals(&["dm-5"]))
        .trace(parent)?;
    ensure!(
        in_use_names(&stacked_journal) == ["dm-5", "sde", "sde3"]
            && stacked_journal.unresolved.is_empty(),
        "an external journal on a device-mapper device: {stacked_journal:?}"
    );
    let internal_only = Case::new("jbd2-internal")
        .tree(|t| t.journals(&["sdl1-8", "dm-5-8", "loop3-8"]))
        .loop_device(loop_of("loop3", 3, "", None))
        .trace(parent)?;
    ensure!(
        in_use_names(&internal_only).is_empty() && internal_only.unresolved.is_empty(),
        "journals inside their file systems: {internal_only:?}"
    );
    let unplaced = Case::new("jbd2-unplaced")
        .tree(|t| t.journals(&["mystery", "nodev-8"]))
        .trace(parent)?;
    ensure!(
        unplaced.unresolved.len() == 2,
        "journals that name no device: {unplaced:?}"
    );
    let unreadable_journals = Case::new("jbd2-file")
        .tree(|t| write(&t.root.join("proc/fs/jbd2"), ""))
        .trace(parent)?;
    ensure!(
        !unreadable_journals.unresolved.is_empty(),
        "a journal list that cannot be read: {unreadable_journals:?}"
    );

    // --- 3: a directory that is not there lists nothing (the cases above have
    // no `proc/fs/jbd2`, most devices no `slaves/`); any other failure to read
    // one is a failure to trace.
    for (what, setup) in [
        (
            "slaves",
            Box::new(|t: &Tree| {
                let dir = t.root.join("sys/devices/virtual/block/dm-4/slaves");
                fs::remove_dir_all(&dir)?;
                write(&dir, "")
            }) as Box<dyn Fn(&Tree) -> Result<()>>,
        ),
        (
            "a btrfs file system's devices",
            Box::new(|t: &Tree| {
                let dir = t.root.join("sys/fs/btrfs").join(UUID_POOL).join("devices");
                fs::remove_dir_all(&dir)?;
                write(&dir, "")
            }),
        ),
        (
            "the btrfs file systems",
            Box::new(|t: &Tree| {
                let dir = t.root.join("sys/fs/btrfs");
                fs::remove_dir_all(&dir)?;
                write(&dir, "")
            }),
        ),
    ] {
        let case = Case::new("unreadable-dir")
            .mounts(&format!(
                "50 16 253:4 / /run/initramfs/live ro,relatime - iso9660 /dev/mapper/ventoy ro\n\
                 51 1 0:51 / /pool rw - btrfs UUID={UUID_POOL} rw\n"
            ))
            .tree(setup)
            .trace(parent)?;
        ensure!(
            case.unresolved
                .iter()
                .any(|u| u.contains("cannot be listed")),
            "{what} that cannot be listed was taken for empty: {case:?}"
        );
    }
    {
        let dir = TestDir::new(parent, "topology-holders")?;
        let tree = Tree::new(dir.0.join("tree"))?;
        tree.disk("sde", 8, 64)?;
        tree.partition("sde", "sde1", 8, 65)?;
        tree.partition("sde", "sde3", 8, 67)?;
        tree.stacked("dm-5", 253, 5, &["sde/sde3"])?;
        let roots = tree.roots();
        let system = System::default();
        let tracer = Tracer::new(&roots, &system);
        ensure!(
            tracer.holders("sde3") == Ok(vec!["dm-5".to_owned()])
                && tracer.holders("sde1") == Ok(vec![]),
            "holders"
        );
        ensure!(
            tracer.holders("nosuch").is_err(),
            "the holders of a device sysfs does not list"
        );
        let unreadable = tree.root.join("sys/devices/virtual/block/sde/sde3/holders");
        fs::remove_dir_all(&unreadable)?;
        write(&unreadable, "")?;
        ensure!(
            tracer.holders("sde3").is_err(),
            "holders that cannot be listed were taken for none"
        );
        // The list of block devices itself.
        let class = tree.root.join("sys/class/block");
        fs::remove_dir_all(&class)?;
        write(&class, "")?;
        let usage = tracer.usage();
        ensure!(
            usage.unresolved.len() == 2
                && usage
                    .unresolved
                    .iter()
                    .all(|u| u.contains("cannot be listed")),
            "loop and zram devices that cannot be listed: {usage:?}"
        );
    }

    // --- the rest: what the earlier checks established stays established.
    // Ventoy: a device-mapper device on a partition, mounted where dracut mounts
    // the medium, named by /dev/mapper/ventoy; or with the kernel's number wrong.
    for line in [
        "50 16 253:4 / /run/initramfs/live ro,relatime shared:5 - iso9660 /dev/mapper/ventoy ro\n",
        "50 16 0:9 / /run/initramfs/live ro - iso9660 /dev/mapper/ventoy ro\n",
    ] {
        let ventoy = Case::new("ventoy").mounts(line).trace(parent)?;
        ensure!(
            ventoy.live.contains("dm-4")
                && ventoy.live.contains("sdb")
                && ventoy.live.contains("sdb2")
                && ventoy.unresolved.is_empty(),
            "ventoy: {ventoy:?}"
        );
    }
    // dracut's iso-scan: the ISO is a file on the host disk mounted aside, and
    // attached to a loop device that is mounted as the medium.
    let isoscan = Case::new("isoscan")
        .mounts(
            "50 16 7:3 / /run/initramfs/live ro - iso9660 /dev/loop3 ro\n\
             51 16 8:33 / /run/initramfs/isoscan rw - ext4 /dev/sdc1 rw\n",
        )
        .loop_device(loop_of(
            "loop3",
            3,
            "/run/initramfs/isoscan/live.iso",
            loop_row(
                "loop3",
                "/run/initramfs/isoscan/live.iso",
                31,
                Some((8, 33)),
            ),
        ))
        .file(
            "/run/initramfs/isoscan/live.iso",
            file_id((8, 33), 31, None),
        )
        .trace(parent)?;
    ensure!(
        isoscan.live.contains("sdc")
            && isoscan.live.contains("loop3")
            && in_use_names(&isoscan).is_empty()
            && isoscan.unresolved.is_empty(),
        "iso-scan: {isoscan:?}"
    );
    // ... with the host disk's mount unmounted lazily the file is not at that
    // name: an ext4 can hold a node, so the use is unresolved (the disk is still
    // the medium's).
    let isoscan_detached = Case::new("isoscan-detached")
        .mounts("50 16 7:3 / /run/initramfs/live ro - iso9660 /dev/loop3 ro\n")
        .loop_device(loop_of(
            "loop3",
            3,
            "/live.iso",
            loop_row("loop3", "/live.iso", 31, Some((8, 33))),
        ))
        .trace(parent)?;
    ensure!(
        isoscan_detached.live.contains("sdc") && !isoscan_detached.unresolved.is_empty(),
        "iso-scan with its host disk detached: {isoscan_detached:?}"
    );
    // A /dev/disk/by-label source (no usable device number), and a source that
    // is no path at all with the right number.
    let label = Case::new("by-label")
        .mounts("50 16 0:55 / /run/media/ilex ro - iso9660 /dev/disk/by-label/ILEX ro\n")
        .trace(parent)?;
    ensure!(
        label.live.contains("sdd") && !label.live.contains("sde"),
        "by-label: {label:?}"
    );
    let number = Case::new("by-number")
        .mounts("50 1 8:48 / /somewhere ro - udf none ro\n")
        .trace(parent)?;
    ensure!(number.live.contains("sdd"), "by number: {number:?}");
    // Anything else mounted is in use, not the medium; so is swap; and what is
    // stacked on a partition is found through the partition's holders.
    let busy = Case::new("busy")
        .mounts(
            "50 1 8:65 / /mnt/data rw - ext4 /dev/sde1 rw\n\
             51 16 0:38 / /run/initramfs rw - tmpfs tmpfs rw\n",
        )
        .swaps("/dev/sde2 partition 1024 0 -2\n")
        .trace(parent)?;
    ensure!(
        busy.unresolved.is_empty() && !busy.live.contains("sde"),
        "busy: {busy:?}"
    );
    let reasons: Vec<&str> = busy.in_use["sde"].iter().map(String::as_str).collect();
    ensure!(
        reasons
            == [
                "/dev/sde1 is mounted at /mnt/data",
                "/dev/sde2 is in use as swap",
            ],
        "in use: {reasons:?}"
    );
    ensure!(
        !busy.in_use.contains_key("sdf") && !busy.in_use.contains_key("sdb"),
        "an unrelated disk is in use: {busy:?}"
    );
    Ok(())
}

/// The parsers of what the kernel prints: a line they cannot read fails the
/// probe, because dropping it would hide a use.
fn check_tables(parent: &Path) -> Result<()> {
    let tables = TestDir::new(parent, "tables")?;
    write(
        &tables.0.join("mountinfo"),
        "1 0 0:30 / / rw - overlay overlay rw\n",
    )?;
    write(
        &tables.0.join("swaps"),
        "Filename\tType\tSize\tUsed\tPriority\n/dev/sde2 partition 1024 0 -2\n",
    )?;
    fs::create_dir(tables.0.join("swapdir"))?;
    let swaps = topology::read_swaps(&tables.0.join("swaps"))?;
    ensure!(
        swaps
            == [topology::Swap {
                path: "/dev/sde2".into(),
                kind: "partition".into()
            }],
        "swaps: {swaps:?}"
    );
    ensure!(
        topology::read_swaps(&tables.0.join("no-such-swaps"))?.is_empty(),
        "swaps of a kernel without swap"
    );
    ensure!(
        topology::read_swaps(&tables.0.join("swapdir")).is_err(),
        "an unreadable swap list was taken for an empty one"
    );
    // mountinfo's escapes, and the options of the mount itself.
    let escaped = topology::parse_mountinfo(
        "50 1 8:65 / /mnt/with\\040space rw,nosuid,nodev - ext4 /dev/sde1 rw\n",
    )?;
    ensure!(
        escaped.len() == 1
            && escaped[0].mount_point == "/mnt/with space"
            && escaped[0].nodev()
            && escaped[0].vfs_options == "rw,nosuid,nodev",
        "escapes: {escaped:?}"
    );
    ensure!(
        !topology::parse_mountinfo("50 1 8:65 / /x rw,nodevice - ext4 /dev/sde1 rw\n")?[0].nodev(),
        "an option that only starts like nodev"
    );
    // A line that cannot be read fails the table (it is never skipped), and so
    // does a table with nothing in it.
    let good = "50 1 8:65 / /mnt/data rw - ext4 /dev/sde1 rw\n";
    for (what, text) in [
        ("nothing", String::new()),
        ("text that is no table", "not a mount table\n".to_owned()),
        (
            "no separator",
            "50 1 8:65 / /mnt rw ext4 /dev/sde1 rw\n".to_owned(),
        ),
        (
            "too few fields",
            "50 1 8:65 / - ext4 /dev/sde1\n".to_owned(),
        ),
        (
            "a device number that is none",
            "50 1 8-65 / /mnt rw - ext4 /dev/sde1 rw\n".to_owned(),
        ),
        (
            "a major that is no number",
            "50 1 x:65 / /mnt rw - ext4 /dev/sde1 rw\n".to_owned(),
        ),
        ("no source", "50 1 8:65 / /mnt rw - ext4\n".to_owned()),
        (
            "a bad line after good ones",
            format!("{good}{good}garbage\n"),
        ),
        ("a blank line", format!("{good}\n{good}")),
    ] {
        ensure!(
            topology::parse_mountinfo(&text).is_err(),
            "{what} was taken for a mount table"
        );
    }
    ensure!(
        topology::parse_mountinfo(&format!("{good}{good}"))?.len() == 2,
        "two good lines"
    );
    write(&tables.0.join("bad-mountinfo"), &format!("{good}garbage\n"))?;
    ensure!(
        topology::read_swaps(&tables.0.join("bad-mountinfo")).is_err(),
        "a mount table read as a swap list"
    );
    for (what, text) in [
        (
            "a swap row with no type",
            "Filename Type Size\n/dev/sde2\n".to_owned(),
        ),
        (
            "a blank row",
            "Filename Type\n\n/dev/sde2 partition\n".to_owned(),
        ),
    ] {
        ensure!(
            topology::parse_swaps(&text).is_err(),
            "{what} was taken for a swap list"
        );
    }
    ensure!(
        topology::parse_swaps("Filename Type Size Used Priority\n")?.is_empty()
            && topology::parse_swaps("")?.is_empty(),
        "no swap areas"
    );
    // losetup's table: the kernel's numbers; an empty column is unknown; anything
    // that is not its document is an error, never "no loop devices".
    let rows = topology::parse_losetup(
        r#"{"loopdevices":[
 {"name":"/dev/loop1","back-file":"/dev/loop0","back-ino":66,"back-maj:min":"0:78"},
 {"name":"/dev/loop0","back-file":"/var/tmp/x.img (deleted)","back-ino":"1999","back-maj:min":"0:66"},
 {"name":"/dev/loop2","back-file":null,"back-ino":null,"back-maj:min":null},
 {"name":"/dev/loop3","back-file":"","back-ino":"","back-maj:min":"nonsense"}]}"#,
    )?;
    ensure!(
        rows.len() == 4
            && rows[0].file.as_deref() == Some("/dev/loop0")
            && rows[0].inode == Some(66)
            && rows[0].device == Some((0, 78))
            && rows[1].inode == Some(1999)
            && rows[1].device == Some((0, 66))
            && rows[2].file.is_none()
            && rows[2].inode.is_none()
            && rows[2].device.is_none()
            && rows[3].file.is_none()
            && rows[3].inode.is_none()
            && rows[3].device.is_none(),
        "losetup: {rows:?}"
    );
    ensure!(
        topology::parse_losetup(r#"{"loopdevices":[]}"#)?.is_empty()
            && [
                "",
                "not json",
                "{}",
                r#"{"loopdevices":{}}"#,
                r#"{"loopdevices":[{}]}"#
            ]
            .iter()
            .all(|text| topology::parse_losetup(text).is_err()),
        "an unreadable losetup was taken for no loop devices"
    );
    // blkid's own probe of a device (here image files: it reads regular files too).
    let image = tables.0.join("fs.img");
    fs::File::create(&image)?.set_len(16 << 20)?;
    ensure!(
        disks::probe_type(&image.to_string_lossy()).is_none(),
        "a blank image has a signature"
    );
    exec::run_ok(
        "mkfs.ext4",
        &["-q", "-F", &image.to_string_lossy()],
        Duration::from_secs(60),
    )?;
    ensure!(
        disks::probe_type(&image.to_string_lossy()).as_deref() == Some("ext4"),
        "blkid does not find the ext4 of an image"
    );
    ensure!(
        disks::probe_type("/no/such/device").is_none(),
        "a device that is not there has a signature"
    );
    // lsblk's signatures, by kernel name, of everything it lists.
    let types = disks::device_types(LSBLK)?;
    ensure!(
        types.get("vda1").map(String::as_str) == Some("vfat")
            && types.get("sdb1").map(String::as_str) == Some("iso9660")
            && !types.contains_key("vda")
            && disks::device_types("{}").is_err(),
        "device types: {types:?}"
    );
    Ok(())
}

/// The mount tables of every mount namespace: the host's (process 1's) and the
/// service namespaces' show mounts the helper's own (`PrivateMounts=yes`: it
/// sees what propagates to it, and nothing made under a private subtree) does
/// not.
fn check_namespaces(parent: &Path) -> Result<()> {
    let dir = TestDir::new(parent, "namespaces")?;
    let proc = dir.0.join("proc");
    let table = |extra: &str| format!("{ROOT_OVERLAY}{KERNEL_MOUNTS}{extra}");
    let process = |pid: &str, namespace: Option<&str>, mounts: Option<&str>| -> Result<()> {
        let base = proc.join(pid);
        fs::create_dir_all(base.join("ns"))?;
        if let Some(id) = namespace {
            symlink(format!("mnt:[{id}]"), base.join("ns/mnt"))?;
        }
        if let Some(text) = mounts {
            write(&base.join("mountinfo"), text)?;
        }
        Ok(())
    };
    let private = "60 1 8:97 / /mnt/private/data rw,relatime - ext4 /dev/sdg1 rw\n";
    // The helper's own namespace: what propagated to it.
    process("self", Some("4026532001"), Some(&table("")))?;
    // The host's: the mount under a private subtree is here only.
    process("1", Some("4026531841"), Some(&table(private)))?;
    // A service's, shared by two processes: one table of the two is read (which
    // one is up to the order /proc lists them in).
    process(
        "212",
        Some("4026532100"),
        Some(&table("70 1 8:177 / /srv/data rw - xfs /dev/sdl1 rw\n")),
    )?;
    process(
        "213",
        Some("4026532100"),
        Some(&table("71 1 8:177 / /srv/same rw - xfs /dev/sdl1 rw\n")),
    )?;
    // A process that is gone before its table is read: not a failure.
    process("400", Some("4026532400"), None)?;
    // One whose namespace cannot be told, one whose table cannot be read.
    process("300", None, None)?;
    write(&proc.join("300/ns/mnt"), "")?;
    process("500", Some("4026532500"), None)?;
    fs::create_dir(proc.join("500/mountinfo"))?;
    // Entries of /proc that are not processes.
    write(&proc.join("cpuinfo"), "")?;
    fs::create_dir_all(proc.join("thread-self"))?;
    let (all, local, unreadable) = topology::read_mounts(&proc)?;
    ensure!(
        all.iter().any(|m| m.mount_point == "/mnt/private/data")
            && all.iter().any(|m| m.mount_point == "/srv/data")
                != all.iter().any(|m| m.mount_point == "/srv/same")
            && !local.iter().any(|m| m.mount_point == "/mnt/private/data")
            && all.iter().filter(|m| m.mount_point == "/proc").count() == 1,
        "the tables of the namespaces: {all:?}"
    );
    ensure!(
        unreadable.len() == 2
            && unreadable.iter().any(|u| u.contains("process 300"))
            && unreadable.iter().any(|u| u.contains("process 500")),
        "tables that cannot be read: {unreadable:?}"
    );
    // The trace: the private mount is a use of its disk, and a table that could
    // not be read is a use that was not seen.
    let tree = Tree::new(dir.0.join("tree"))?;
    tree.disk("sdg", 8, 96)?;
    tree.partition("sdg", "sdg1", 8, 97)?;
    tree.disk("sdl", 8, 176)?;
    tree.partition("sdl", "sdl1", 8, 177)?;
    tree.disk("sda", 8, 0)?;
    tree.partition("sda", "sda1", 8, 1)?;
    tree.loop_device("loop0", 7, 0, "")?;
    let roots = tree.roots();
    let stat = |path: &str| tree.stat(path);
    let system = System {
        mounts: all,
        local,
        swaps: vec![],
        loops: vec![],
        unreadable,
    };
    let usage = Tracer::new(&roots, &system).with_stat(&stat).usage();
    ensure!(
        in_use_names(&usage).contains(&"sdg")
            && in_use_names(&usage).contains(&"sdl")
            && usage.unresolved.len() == 2,
        "the private mounts: {usage:?}"
    );
    ensure!(
        usage.in_use["sdg"]
            .iter()
            .any(|r| r == "/dev/sdg1 is mounted at /mnt/private/data"),
        "its reason: {:?}",
        usage.in_use["sdg"]
    );
    // The host's table and this process's must be read.
    for (what, remove) in [
        ("process 1's", "1/mountinfo"),
        ("this process's", "self/mountinfo"),
    ] {
        let kept = fs::read(proc.join(remove))?;
        fs::remove_file(proc.join(remove))?;
        ensure!(
            topology::read_mounts(&proc).is_err(),
            "{what} table is missing and the probe went on"
        );
        fs::write(proc.join(remove), kept)?;
    }
    write(&proc.join("1/mountinfo"), "garbage\n")?;
    ensure!(
        topology::read_mounts(&proc).is_err(),
        "the host's table could not be read and the probe went on"
    );
    Ok(())
}

/// Tasks that are exiting, as `/proc` shows them: `mountinfo` answers `EINVAL`
/// (the task has no namespace proxy left), `ns/mnt` answers `ENOENT`, `ESRCH` or
/// `EACCES`. Such a task is gone, whatever else shares its namespace is still
/// there to be read, and only a table that cannot be read for another reason is
/// a use that was not seen.
fn check_exiting_tasks(parent: &Path) -> Result<()> {
    let dir = TestDir::new(parent, "exiting")?;
    let proc = dir.0.join("proc");
    let table = |extra: &str| format!("{ROOT_OVERLAY}{KERNEL_MOUNTS}{extra}");
    let process = |pid: &str, namespace: &str, mounts: Option<&str>| -> Result<()> {
        let base = proc.join(pid);
        fs::create_dir_all(base.join("ns"))?;
        symlink(format!("mnt:[{namespace}]"), base.join("ns/mnt"))?;
        if let Some(text) = mounts {
            write(&base.join("mountinfo"), text)?;
        }
        Ok(())
    };
    process("self", "4026532001", Some(&table("")))?;
    process("1", "4026531841", Some(&table("")))?;
    // Two processes of one service's namespace: whichever /proc lists first
    // exits (its table is EINVAL and its link is gone), the other is read.
    let service = "50 1 8:177 / /srv/service rw - xfs /dev/sdl1 rw\n";
    process("101", "4026532600", Some(service))?;
    process("102", "4026532600", Some(service))?;
    // A namespace whose every process exits: nothing to read, nothing unseen.
    process("201", "4026532700", Some(service))?;
    // A task whose link answers ESRCH.
    process("301", "4026532800", Some(service))?;
    // EINVAL for a table but a link that still reads: not an exiting task.
    process("401", "4026532900", Some(service))?;
    // EACCES from the link after EINVAL.
    process("501", "4026533000", Some(service))?;
    let calls: RefCell<BTreeMap<String, u32>> = RefCell::new(BTreeMap::new());
    let key = |path: &Path| {
        path.strip_prefix(&proc)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    };
    let read = |path: &Path| -> io::Result<Vec<u8>> {
        let key = key(path);
        let einval = [
            "101/mountinfo",
            "201/mountinfo",
            "401/mountinfo",
            "501/mountinfo",
        ];
        if einval.contains(&key.as_str()) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        fs::read(path)
    };
    let read_link = |path: &Path| -> io::Result<PathBuf> {
        let key = key(path);
        let n = {
            let mut calls = calls.borrow_mut();
            let n = calls.entry(key.clone()).or_default();
            *n += 1;
            *n
        };
        // The first look finds the namespace; the look after EINVAL finds the
        // task gone (or, for 401, still there).
        match (key.as_str(), n) {
            ("101/ns/mnt" | "201/ns/mnt", 2) => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            ("301/ns/mnt", _) => Err(io::Error::from_raw_os_error(libc::ESRCH)),
            ("501/ns/mnt", 2) => Err(io::Error::from_raw_os_error(libc::EACCES)),
            _ => fs::read_link(path),
        }
    };
    let proc_fs = topology::ProcFs {
        read_link: &read_link,
        read: &read,
    };
    let (all, _, unreadable) = topology::read_mounts_with(&proc, &proc_fs)?;
    ensure!(
        all.iter().any(|m| m.mount_point == "/srv/service"),
        "the other process of a namespace was not read after the first one exited: {all:?}"
    );
    ensure!(
        unreadable.len() == 1 && unreadable[0].contains("process 401"),
        "an exiting task was reported as a table that cannot be read, or one that cannot be read was not: {unreadable:?}"
    );
    Ok(())
}

/// The kernel's claim on a disk, as the exclusive open of it answers: free,
/// refused (`EBUSY`, retried briefly so that a claim that is only passing does
/// not make a disk flicker) or not answered.
fn check_claims(parent: &Path) -> Result<()> {
    let calls = Cell::new(0u32);
    let busy = || io::Error::from_raw_os_error(libc::EBUSY);
    let mut passing = || {
        calls.set(calls.get() + 1);
        if calls.get() < 4 { Err(busy()) } else { Ok(()) }
    };
    ensure!(
        disks::claim_with(&mut passing, 10, Duration::ZERO) == disks::Claim::Free
            && calls.get() == 4,
        "a passing claim: {} tries",
        calls.get()
    );
    calls.set(0);
    let mut held = || {
        calls.set(calls.get() + 1);
        Err(busy())
    };
    ensure!(
        disks::claim_with(&mut held, 10, Duration::ZERO) == disks::Claim::Busy && calls.get() == 10,
        "a held disk: {} tries",
        calls.get()
    );
    calls.set(0);
    let mut refused = || {
        calls.set(calls.get() + 1);
        Err(io::Error::from_raw_os_error(libc::EACCES))
    };
    ensure!(
        matches!(
            disks::claim_with(&mut refused, 10, Duration::ZERO),
            disks::Claim::Unknown(_)
        ) && calls.get() == 1,
        "an open that fails for another reason is not retried: {} tries",
        calls.get()
    );
    ensure!(
        disks::claim_reason("/dev/vdb", &disks::Claim::Free).is_none()
            && disks::claim_reason("/dev/vdb", &disks::Claim::Busy)
                .is_some_and(|r| r.contains("/dev/vdb") && r.contains("held open exclusively"))
            && disks::claim_reason("/dev/vdb", &disks::Claim::Unknown("EIO".into()))
                .is_some_and(|r| r.contains("could not be opened") && r.contains("EIO")),
        "the reasons"
    );
    // The real thing, on something that is not a disk: not a block device.
    let tree = TestDir::new(parent, "claim")?;
    write(&tree.0.join("sys/class/block/fake/dev"), "7:99\n")?;
    write(&tree.0.join("node"), "")?;
    ensure!(
        matches!(
            disks::claim_disk(
                &tree.0.join("sys"),
                "fake",
                &tree.0.join("node").to_string_lossy()
            ),
            disks::Claim::Unknown(_)
        ),
        "a file was taken for the disk"
    );
    ensure!(
        matches!(
            disks::claim_disk(&tree.0.join("sys"), "missing", "/dev/null"),
            disks::Claim::Unknown(_)
        ),
        "a disk sysfs has no number for"
    );
    Ok(())
}

// --- Finish -----------------------------------------------------------------

/// A loop device over a file, detached when dropped.
struct LoopDevice(String);

impl LoopDevice {
    fn attach(image: &Path) -> Result<Self> {
        let out = exec::run_ok(
            "losetup",
            &["--find", "--show", &image.to_string_lossy()],
            Duration::from_secs(30),
        )?;
        Ok(Self(out.stdout_text().trim().to_owned()))
    }
}

impl Drop for LoopDevice {
    fn drop(&mut self) {
        let _ = exec::run("losetup", &["-d", &self.0], Duration::from_secs(30));
    }
}

/// `Finish(save-log)`: the tarball a log is, the names it may hold, and the
/// writer that puts it on a stick: a real file system on a loop device,
/// mounted in this process's namespace for the test, never overwriting a file.
fn check_finish(parent: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    // The archive: names, contents, padding, checksums.
    let mut tar = finish::Tar::default();
    tar.add(
        "ilex-installer-log/state.json",
        b"{\"stage\": \"image\"}",
        7,
    )?;
    tar.add("ilex-installer-log/empty", b"", 7)?;
    tar.add("ilex-installer-log/big", &[b'x'; 1500], 7)?;
    let bytes = tar.finish()?;
    ensure!(bytes.len() % 512 == 0, "a tar's size is not whole blocks");
    let members = finish::untar(&bytes)?;
    ensure!(
        members.len() == 3
            && members[0]
                == (
                    "ilex-installer-log/state.json".into(),
                    b"{\"stage\": \"image\"}".to_vec()
                )
            && members[1].1.is_empty()
            && members[2].1.len() == 1500,
        "a tar does not read back as it was written: {} members",
        members.len()
    );
    // GNU tar must agree (it is what the person unpacks it with).
    let dir = TestDir::new(parent, "finish")?;
    let archive = dir.0.join("log.tar");
    fs::write(&archive, &bytes)?;
    let listed = exec::run_ok(
        "tar",
        &["-tvf", &archive.to_string_lossy()],
        Duration::from_secs(30),
    )?
    .stdout_text();
    ensure!(
        listed.contains("ilex-installer-log/state.json") && listed.contains("1500"),
        "tar does not list the archive: {listed}"
    );
    for name in [
        "",
        "/etc/passwd",
        "../x",
        "a/../x",
        "a//b",
        "./a",
        "a\nb",
        "é",
        &"x".repeat(101),
    ] {
        ensure!(
            finish::Tar::default().add(name, b"x", 0).is_err(),
            "the member name {name:?} is accepted"
        );
    }
    ensure!(
        finish::member_name_ok("01-partition.log")
            && finish::member_name_ok("plan.json")
            && !finish::member_name_ok("")
            && !finish::member_name_ok(".hidden")
            && !finish::member_name_ok("a b")
            && !finish::member_name_ok("../x")
            && !finish::member_name_ok(&"x".repeat(65)),
        "the log member names"
    );
    ensure!(
        finish::log_name(Some(&"ab".repeat(16)), 1_700_000_000)
            == "ilex-installer-log-abababab-1700000000.tar"
            && finish::log_name(None, 5) == "ilex-installer-log-none-5.tar",
        "the log's file name"
    );

    // The writer refuses what it was not given by the helper, before it mounts.
    let mountpoint = dir.0.join("stick");
    fs::create_dir(&mountpoint)?;
    let refused = |device: &str, fstype: &str, name: &str, expect: &str| {
        finish::write_log_child(device, fstype, &mountpoint, &archive, name, expect).error
    };
    ensure!(
        refused("/etc/passwd", "vfat", "a.tar", "1:1").is_some()
            && refused("/dev/null", "vfat", "a.tar", "1:3").is_some()
            && refused("/dev/null", "swap", "a.tar", "1:3").is_some()
            && refused("/dev/null", "vfat", "../a.tar", "1:3").is_some()
            && refused("/dev/null", "vfat", "a.txt", "1:3").is_some()
            && refused("/dev/null", "vfat", "a.tar", "nonsense").is_some(),
        "the log writer takes a device, a type, a name or a number it should not"
    );

    // For real: ext4 and FAT images on loop devices.
    for (fs_name, mkfs) in [("ext4", "mkfs.ext4"), ("vfat", "mkfs.vfat")] {
        let image = dir.0.join(format!("stick-{fs_name}.img"));
        fs::File::create(&image)?.set_len(64 << 20)?;
        let args: &[&str] = if fs_name == "ext4" {
            &["-q", "-F"]
        } else {
            &["-F", "32"]
        };
        let mut all = args.to_vec();
        let path = image.to_string_lossy().into_owned();
        all.push(&path);
        exec::run_ok(mkfs, &all, Duration::from_secs(60))?;
        let device = LoopDevice::attach(&image)?;
        let meta = fs::metadata(&device.0)?;
        let number = topology::split_dev(meta.rdev());
        let expect = format!("{}:{}", number.0, number.1);
        let wrong = format!("{}:{}", number.0, number.1 + 1);
        ensure!(
            refused(&device.0, fs_name, "a.tar", &wrong)
                .is_some_and(|e| e.contains("not the partition that was checked")),
            "{fs_name}: a device number that is not the device's was accepted"
        );
        let first =
            finish::write_log_child(&device.0, fs_name, &mountpoint, &archive, "a.tar", &expect);
        ensure!(
            first.error.is_none() && first.file == "a.tar" && first.bytes == bytes.len() as u64,
            "{fs_name}: the first write: {first:?}"
        );
        let second =
            finish::write_log_child(&device.0, fs_name, &mountpoint, &archive, "a.tar", &expect);
        ensure!(
            second.error.is_none() && second.file == "a-1.tar",
            "{fs_name}: a file that exists was written over, or the name did not move on: {second:?}"
        );
        // What is on the stick is what was written.
        let look = dir.0.join(format!("look-{fs_name}"));
        fs::create_dir(&look)?;
        exec::run_ok(
            "mount",
            &[
                "-o",
                "ro,nodev,nosuid,noexec",
                &device.0,
                &look.to_string_lossy(),
            ],
            Duration::from_secs(30),
        )?;
        let on_stick = fs::read(look.join("a.tar"));
        let second_on_stick = fs::read(look.join("a-1.tar"));
        let mode = fs::metadata(look.join("a.tar"))
            .map(|m| m.mode() & 0o777)
            .ok();
        let _ = exec::run(
            "umount",
            &[&look.to_string_lossy()],
            Duration::from_secs(30),
        );
        ensure!(
            on_stick.as_ref().is_ok_and(|b| *b == bytes)
                && second_on_stick.as_ref().is_ok_and(|b| *b == bytes),
            "{fs_name}: the files on the stick are not the archive"
        );
        ensure!(
            fs_name != "ext4" || mode == Some(0o644),
            "{fs_name}: the log file's mode is {mode:?}"
        );
        ensure!(
            target::current_mounts()?
                .iter()
                .all(|m| m.mount_point != mountpoint.to_string_lossy()
                    && m.mount_point != look.to_string_lossy()),
            "{fs_name}: the stick is still mounted"
        );
    }
    Ok(())
}

// --- files ------------------------------------------------------------------

fn tzif(path: &Path) -> Result<()> {
    write(path, "TZif2\0\0\0")
}

fn check_files(parent: &Path) -> Result<()> {
    let scratch = TestDir::new(parent, "files")?;
    let root = &scratch.0;
    // An ESP's loaders.
    for dir in ["EFI/Microsoft/Boot", "EFI/BOOT", "EFI/ubuntu"] {
        fs::create_dir_all(root.join("esp").join(dir))?;
    }
    let loaders = disks::efi_loaders(&root.join("esp"), "/dev/x1");
    ensure!(
        loaders.len() == 2
            && loaders[0].description.starts_with("Windows Boot Manager")
            && loaders[1].description == "EFI loader directory EFI/ubuntu",
        "loaders: {loaders:?}"
    );
    // A link in place of EFI, or of a loader directory, counts for nothing.
    fs::create_dir_all(root.join("outside/EFI/evil"))?;
    fs::create_dir_all(root.join("linked"))?;
    symlink(root.join("outside/EFI"), root.join("linked/EFI"))?;
    fs::create_dir_all(root.join("linked2/EFI"))?;
    symlink(root.join("outside/EFI/evil"), root.join("linked2/EFI/evil"))?;
    ensure!(
        disks::efi_loader_names(&root.join("linked")).is_empty()
            && disks::efi_loader_names(&root.join("linked2")).is_empty(),
        "a link was followed into an ESP"
    );
    // Zone names: TZif files inside their tree, not the duplicates.
    let zones = root.join("zoneinfo");
    tzif(&zones.join("Europe/Berlin"))?;
    tzif(&zones.join("posix/Europe/Berlin"))?;
    tzif(&zones.join("right/Europe/Berlin"))?;
    tzif(&zones.join("America/New_York"))?;
    write(&zones.join("zone.tab"), "# not a zone\n")?;
    write(&zones.join("Europe/Notzone"), "TZi")?;
    write(&root.join("outside/file"), "TZif")?;
    symlink("../../outside/file", zones.join("Europe/Escape"))?;
    symlink("America/New_York", zones.join("posixrules"))?;
    symlink("../posix/Europe/Berlin", zones.join("Europe/ViaPosix"))?;
    let exists = |zone: &str| helper::time_zone_exists(&zones, zone);
    ensure!(
        exists("Europe/Berlin") && exists("America/New_York"),
        "real zones refused"
    );
    for (name, zone) in [
        ("a link out of the tree", "Europe/Escape"),
        ("a directory", "Europe"),
        ("a zone that is not there", "Nowhere/Land"),
        ("a text file", "zone.tab"),
        ("a file that is not TZif", "Europe/Notzone"),
        ("the posix tree", "posix/Europe/Berlin"),
        ("the right tree", "right/Europe/Berlin"),
        ("posixrules", "posixrules"),
        ("a link into the posix tree", "Europe/ViaPosix"),
    ] {
        ensure!(!exists(zone), "{name} ({zone}) was accepted as a zone");
    }
    fs::create_dir_all(root.join("keymaps/i386/qwertz"))?;
    fs::write(root.join("keymaps/i386/qwertz/de-latin1.map.gz"), b"")?;
    ensure!(
        helper::keymap_exists(&root.join("keymaps"), "de-latin1")
            && !helper::keymap_exists(&root.join("keymaps"), "de"),
        "keymap names"
    );
    // Account and group names the live image defines.
    write(
        &root.join("etc/passwd"),
        "root:x:0:0::/root:/bin/bash\nsshd:x:74:74::/:/sbin/nologin\n",
    )?;
    write(&root.join("etc/group"), "root:x:0:\ntss:x:59:\n")?;
    write(
        &root.join("sysusers/a.conf"),
        "# a comment\nu chrony - \"chrony\" /var/lib/chrony\ng ilex-homed -\nm chrony wheel\nr - 500-900\nu! locked - -\nu -skipped -\n",
    )?;
    write(&root.join("sysusers/ignored.txt"), "u hidden -\n")?;
    let defined = helper::system_names(
        &root.join("etc/passwd"),
        &root.join("etc/group"),
        &[root.join("sysusers"), root.join("not-there")],
    );
    for wanted in [
        "root",
        "sshd",
        "tss",
        "chrony",
        "ilex-homed",
        "wheel",
        "locked",
    ] {
        ensure!(defined.contains(wanted), "{wanted} is not a defined name");
    }
    ensure!(
        !defined.contains("hidden")
            && !defined.contains("-skipped")
            && !defined.contains("ilexmaint"),
        "names: {defined:?}"
    );
    Ok(())
}

/// `512M`, `1G`: repart's size suffixes.
fn parse_size(text: &str) -> Option<u64> {
    let (number, unit) = text.split_at(text.len().checked_sub(1)?);
    let unit = match unit {
        "M" => 1 << 20,
        "G" => 1 << 30,
        _ => return None,
    };
    Some(number.parse::<u64>().ok()? * unit)
}

/// The sizes the model plans with are the sizes the shipped files ask for, in
/// both layouts; the two directories are complete layouts of their own, with
/// the same ESP and `/boot` files, and one root partition each.
fn check_definition_sizes(definitions: &Path) -> Result<()> {
    use ilex_installer_model::plan::{BOOT_BYTES, ESP_BYTES, MIN_ROOT_BYTES};
    for (encrypt, directory) in [(false, "whole-disk"), (true, "whole-disk-encrypted")] {
        let dir = definitions.join(directory);
        let size = |file: &str, key: &str| -> Result<u64> {
            let text = fs::read_to_string(dir.join(file))?;
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
                .and_then(|v| parse_size(v.trim()))
                .with_context(|| format!("{directory}/{file} has no {key}"))
        };
        ensure!(
            size("10-esp.conf", "SizeMinBytes")? == ESP_BYTES
                && size("10-esp.conf", "SizeMaxBytes")? == ESP_BYTES,
            "{directory}: ESP size"
        );
        ensure!(
            size("20-boot.conf", "SizeMinBytes")? == BOOT_BYTES
                && size("20-boot.conf", "SizeMaxBytes")? == BOOT_BYTES,
            "{directory}: boot size"
        );
        ensure!(
            size("30-root.conf", "SizeMinBytes")? == MIN_ROOT_BYTES,
            "{directory}: root minimum"
        );
        let files: Vec<String> = fs::read_dir(&dir)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ensure!(
            files.len() == 3,
            "{directory} holds {files:?}, not exactly the three definitions of a layout"
        );
        // One root partition, encrypted when (and only when) asked.
        repart::read_layout(&dir, encrypt).with_context(|| directory.to_owned())?;
        ensure!(
            repart::read_layout(&dir, !encrypt).is_err(),
            "{directory} passes for the other kind of layout"
        );
    }
    for shared in ["10-esp.conf", "20-boot.conf"] {
        ensure!(
            fs::read(definitions.join("whole-disk").join(shared))?
                == fs::read(definitions.join("whole-disk-encrypted").join(shared))?,
            "{shared} differs between the two layouts"
        );
    }
    Ok(())
}

/// The shipped definitions and this system's `systemd-repart`, on a sparse file.
fn check_repart(config: &helper::Config, parent: &Path) -> Result<String> {
    check_definition_sizes(&config.definitions)?;
    let scratch = TestDir::new(parent, "repart")?;
    let image = scratch.0.join("disk.img");
    fs::File::create(&image)?.set_len(40 * 1024 * 1024 * 1024)?;
    let device = image.to_string_lossy().into_owned();
    let mut sizes = Vec::new();
    for encrypt in [false, true] {
        let layout = repart::layout(&config.definitions, encrypt)?;
        let plan = repart::dry_run(&layout, &device, "0b5d9a3e-6a4e-4a2c-9d0e-2f7c7b1c9f11")?;
        let kinds: Vec<&str> = plan.iter().map(|p| p.kind.as_str()).collect();
        ensure!(
            kinds == ["esp", "xbootldr", "root-x86-64"],
            "layout: {kinds:?}"
        );
        ensure!(
            plan.iter().all(|p| p.label.starts_with("ilex-")),
            "a partition label without the ilex- prefix"
        );
        ensure!(
            plan.iter().map(|p| p.encrypted).collect::<Vec<_>>() == [false, false, encrypt],
            "encryption flags with encrypt={encrypt}"
        );
        ensure!(
            plan[0].start_bytes == 1024 * 1024,
            "the first partition does not start at 1 MiB"
        );
        let end = plan.last().map_or(0, |p| p.start_bytes + p.size_bytes);
        ensure!(
            end <= 40 * 1024 * 1024 * 1024,
            "the layout is larger than the disk"
        );
        let again = repart::dry_run(&layout, &device, "0b5d9a3e-6a4e-4a2c-9d0e-2f7c7b1c9f11")?;
        ensure!(plan == again, "the same seed planned different partitions");
        sizes.push(plan.iter().map(|p| p.size_bytes).sum::<u64>());
    }
    ensure!(sizes[0] == sizes[1], "an encrypted layout differs in size");
    ensure!(
        fs::read_dir(&scratch.0)?
            .flatten()
            .all(|e| e.file_name() != "disk.img.part"),
        "repart left files"
    );
    ensure!(
        fs::metadata(&image)?.len() == 40 * 1024 * 1024 * 1024,
        "the dry run resized the file"
    );
    Ok("repart".into())
}

fn check_system(config: &helper::Config) -> Result<String> {
    let helper = helper::Helper::new(config.clone()).context("loading the catalogue")?;
    // An edition record that is absent means the Anaconda path's table; one
    // that is there and cannot be read (here a directory) stops the helper.
    let mut absent = config.clone();
    absent.edition_file = config.run_dir.join("no-such-edition.json");
    ensure!(
        helper::Helper::new(absent).is_ok(),
        "a table without an edition record was refused"
    );
    let mut unreadable = config.clone();
    unreadable.edition_file = config.run_dir.clone();
    ensure!(
        helper::Helper::new(unreadable).is_err(),
        "an edition record that cannot be read was ignored"
    );
    let probe = helper.probe()?;
    ensure!(
        probe.schema == 1 && probe.catalogue.default == helper.table.default,
        "probe header"
    );
    ensure!(
        probe.memory.total_bytes > 0 && probe.memory.available_bytes <= probe.memory.total_bytes,
        "memory"
    );
    // Every offer is this edition's, with the default's own shell list.
    let default = probe
        .catalogue
        .composition(&probe.catalogue.default)
        .context("the default is not offered")?;
    ensure!(
        default.shells.first() == Some(&probe.edition.id)
            && probe
                .catalogue
                .compositions
                .iter()
                .all(|c| c.shells == default.shells),
        "an offered composition is not this edition's"
    );
    ensure!(
        probe.firmware.uefi == config.efi_dir.is_dir(),
        "the firmware is not what the directory says"
    );
    let mut ids = BTreeSet::new();
    for disk in &probe.disks {
        ensure!(ids.insert(&disk.id), "two disks share the id {}", disk.id);
        let mut last = 0;
        for partition in &disk.partitions {
            ensure!(
                partition.start_bytes >= last,
                "{} overlaps its predecessor",
                partition.path
            );
            ensure!(
                partition.start_bytes + partition.size_bytes <= disk.size_bytes,
                "{} runs past its disk",
                partition.path
            );
            last = partition.start_bytes + partition.size_bytes;
        }
        for extent in &disk.free_extents {
            ensure!(
                extent.start_bytes % (1 << 20) == 0
                    && extent.start_bytes + extent.size_bytes <= disk.size_bytes,
                "free extent {extent:?} of {}",
                disk.id
            );
            ensure!(
                disk.partitions
                    .iter()
                    .all(|p| p.start_bytes + p.size_bytes <= extent.start_bytes
                        || p.start_bytes >= extent.start_bytes + extent.size_bytes),
                "free extent {extent:?} overlaps a partition of {}",
                disk.id
            );
        }
    }
    Ok(format!(
        "{} disk(s), edition {}",
        probe.disks.len(),
        probe.edition.id
    ))
}

pub fn run(config: &helper::Config) -> Result<String> {
    let model = ilex_installer_model::selfcheck::run().context("model")?;
    check_parsers().context("parsers")?;
    // A private directory of its own: a self-check run beside the daemon (or
    // another self-check) shares no path with it.
    let parent = Scratch::new(&config.run_dir, "selfcheck")?;
    check_exec_and_scratch(parent.path()).context("subprocess output and scratch directories")?;
    check_nvme(parent.path()).context("NVMe transports")?;
    check_tables(parent.path()).context("the tables of the running system")?;
    check_topology(parent.path()).context("the live medium and disks in use")?;
    check_namespaces(parent.path()).context("the mount namespaces")?;
    check_exiting_tasks(parent.path()).context("tasks that are exiting")?;
    check_claims(parent.path()).context("the exclusive open of a disk")?;
    check_secrets().context("secrets")?;
    check_luks().context("the encrypted volume")?;
    check_recovery_holder().context("the recovery key's hand-over")?;
    check_jobs(parent.path()).context("the tools an installation runs")?;
    check_state(parent.path()).context("the installation's state")?;
    check_target(parent.path()).context("the target system's files")?;
    check_apply_rules().context("the checks before the first write")?;
    check_answers(parent.path()).context("answers mode")?;
    check_finish(parent.path()).context("Finish and the log on a stick")?;
    check_files(parent.path()).context("files")?;
    let repart = check_repart(config, parent.path()).context("repart")?;
    let system = check_system(config).context("system")?;
    Ok(format!(
        "{model}; parsers, registry, subprocess limits, scratch, topology, installation, files, {repart}; {system}"
    ))
}

// --- what Apply adds ----------------------------------------------------------

/// Secrets arrive as sealed memfds and nothing else.
fn check_secrets() -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let reads = |fd: OwnedFd| secret::Secret::from_memfd(fd, 4096);
    let sealed = secret::sealed_memfd(b"correct horse")?;
    let kept = reads(sealed)?;
    ensure!(
        kept.expose() == b"correct horse",
        "a sealed memfd was misread"
    );
    ensure!(
        secret::admin_password(&kept)? == "correct horse",
        "the password was not accepted"
    );
    // An unsealed memfd could change after it was read; a memfd that can still
    // grow could be made to look different through the same descriptor.
    let raw = unsafe {
        libc::memfd_create(
            c"unsealed".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(raw >= 0, "memfd_create");
    let unsealed = unsafe { OwnedFd::from_raw_fd(raw) };
    ensure!(
        unsafe { libc::write(unsealed.as_raw_fd(), b"x".as_ptr().cast(), 1) } == 1,
        "writing the memfd"
    );
    ensure!(reads(unsealed).is_err(), "an unsealed memfd was accepted");
    // Not a memfd at all: a pipe, an ordinary file.
    let (reader, writer) = std::io::pipe()?;
    drop(writer);
    ensure!(reads(OwnedFd::from(reader)).is_err(), "a pipe was accepted");
    let file = fs::File::open("/etc/os-release")?;
    ensure!(
        reads(OwnedFd::from(file)).is_err(),
        "a regular file was accepted"
    );
    ensure!(
        reads(secret::sealed_memfd(b"")?).is_err(),
        "an empty secret was accepted"
    );
    ensure!(
        secret::Secret::from_memfd(secret::sealed_memfd(&[b'x'; 33])?, 32).is_err(),
        "an oversized secret was accepted"
    );
    // Each secret has pages of its own: dropping one unlocks nothing of
    // another's, and a secret's length is not read after it was wiped.
    let first = reads(secret::sealed_memfd(b"first secret")?)?;
    let second = reads(secret::sealed_memfd(b"second secret")?)?;
    let page = |s: &secret::Secret| s.address() / 4096;
    ensure!(
        page(&first) != page(&second)
            && first.address() % 4096 == 0
            && second.address() % 4096 == 0
            && first.mapped_len() >= 4096,
        "two secrets share a page"
    );
    let address = second.address();
    let locked = second.is_locked();
    drop(first);
    ensure!(
        second.expose() == b"second secret",
        "dropping a secret changed another"
    );
    if locked {
        // /proc/self/smaps: the mapping that holds the second secret is still
        // locked after the first was unlocked.
        let smaps = fs::read_to_string("/proc/self/smaps")?;
        let mut inside = false;
        let mut kilobytes = None;
        for line in smaps.lines() {
            if let Some((range, _)) = line.split_once(' ')
                && let Some((from, to)) = range.split_once('-')
                && let (Ok(from), Ok(to)) = (
                    usize::from_str_radix(from, 16),
                    usize::from_str_radix(to, 16),
                )
            {
                inside = from <= address && address < to;
            } else if inside && let Some(rest) = line.strip_prefix("Locked:") {
                kilobytes = rest
                    .trim()
                    .trim_end_matches("kB")
                    .trim()
                    .parse::<u64>()
                    .ok();
            }
        }
        ensure!(
            kilobytes.is_some_and(|k| k >= 4),
            "a live secret's page was unlocked by dropping another: {kilobytes:?}"
        );
    }
    drop(second);
    // The password goes to chpasswd as one line.
    for bad in [&b"a\nb"[..], b"a\rb", b"a\0b", b"\xff\xfe"] {
        let secret = reads(secret::sealed_memfd(bad)?)?;
        ensure!(
            secret::admin_password(&secret).is_err(),
            "a password with a line break, NUL or invalid UTF-8 was accepted"
        );
    }
    // A password may hold what a person types: spaces, colons, quotes.
    let secret = reads(secret::sealed_memfd(b" a:b 'c' \"d\" ")?)?;
    ensure!(
        secret::admin_password(&secret)? == " a:b 'c' \"d\" ",
        "a password with punctuation was changed"
    );
    Ok(())
}

/// What an encrypted installation needs besides the tools: the volume's header
/// as the installed system accepts it, the line of `/etc/crypttab`, the kernel
/// arguments, the passphrase as a file the tools read through a descriptor,
/// and the rules for what a passphrase may hold.
fn check_luks() -> Result<()> {
    let uuid = "0b5d9a3e-6a4e-4a2c-9d0e-2f7c7b1c9f11";
    ensure!(
        luks::is_uuid(uuid)
            && !luks::is_uuid(&uuid.to_ascii_uppercase())
            && !luks::is_uuid(&uuid[1..])
            && !luks::is_uuid("0b5d9a3e6a4e4a2c9d0e2f7c7b1c9f11----"),
        "the UUID rule"
    );
    ensure!(
        luks::mapping_name(uuid) == format!("luks-{uuid}")
            && luks::mapping_node(uuid) == format!("/dev/mapper/luks-{uuid}")
            && luks::crypttab_line(uuid) == format!("luks-{uuid} UUID={uuid} none discard\n"),
        "the volume's names and its crypttab line"
    );
    ensure!(
        luks::kernel_args(uuid, "de-nodeadkeys")
            == [
                format!("rd.luks.uuid={uuid}"),
                "vconsole.keymap=de-nodeadkeys".to_owned()
            ],
        "the kernel arguments"
    );
    ensure!(
        luks::parse_uuid(&["".into(), format!("  {}\n", uuid.to_ascii_uppercase())])? == uuid
            && luks::parse_uuid(&["not a uuid".into()]).is_err()
            && luks::parse_uuid(&[]).is_err(),
        "luksUUID's output"
    );

    // The header of what repart makes (cryptsetup 2.8's own JSON, shortened).
    let header = |edit: &dyn Fn(&mut serde_json::Value)| -> Result<luks::Header> {
        let mut json = serde_json::json!({
            "keyslots": {"0": {"type": "luks2", "key_size": 64,
                "af": {"type": "luks1", "stripes": 4000, "hash": "sha256"},
                "area": {"type": "raw", "offset": "32768", "size": "258048", "encryption": "aes-xts-plain64", "key_size": 64},
                "kdf": {"type": "argon2id", "time": 4, "memory": 1048576, "cpus": 4, "salt": "x"}}},
            "tokens": {},
            "segments": {"0": {"type": "crypt", "offset": "16777216", "size": "dynamic", "iv_tweak": "0",
                "encryption": "aes-xts-plain64", "sector_size": 4096}},
            "digests": {"0": {"type": "pbkdf2", "keyslots": ["0"], "segments": ["0"]}},
            "config": {"json_size": "12288", "keyslots_size": "16744448"},
        });
        edit(&mut json);
        luks::check_header(&json.to_string())
    };
    ensure!(
        header(&|_| {})?
            == luks::Header {
                cipher: "aes-xts-plain64".into(),
                key_bits: 512,
                kdf: "argon2id".into(),
                sector_size: 4096
            },
        "a header repart makes was not accepted"
    );
    type Edit = Box<dyn Fn(&mut serde_json::Value)>;
    let refused: [(&str, Edit); 8] = [
        (
            "a second keyslot",
            Box::new(|j| j["keyslots"]["1"] = j["keyslots"]["0"].clone()),
        ),
        (
            "no keyslot",
            Box::new(|j| j["keyslots"] = serde_json::json!({})),
        ),
        (
            "pbkdf2",
            Box::new(|j| j["keyslots"]["0"]["kdf"]["type"] = "pbkdf2".into()),
        ),
        (
            "another cipher",
            Box::new(|j| j["segments"]["0"]["encryption"] = "aes-cbc-essiv:sha256".into()),
        ),
        (
            "a 256-bit payload key",
            Box::new(|j| j["keyslots"]["0"]["key_size"] = 32.into()),
        ),
        (
            "a token",
            Box::new(|j| j["tokens"]["0"] = serde_json::json!({"type": "systemd-tpm2"})),
        ),
        (
            "a fixed-size segment",
            Box::new(|j| j["segments"]["0"]["size"] = "1048576".into()),
        ),
        (
            "two segments",
            Box::new(|j| j["segments"]["1"] = j["segments"]["0"].clone()),
        ),
    ];
    for (what, edit) in &refused {
        ensure!(
            header(edit.as_ref()).is_err(),
            "a header with {what} was accepted"
        );
    }
    ensure!(
        luks::check_header("not json").is_err() && luks::check_header("{}").is_err(),
        "a document that is no header was accepted"
    );

    // The passphrase as a file: the child reads exactly the bytes, by the path
    // that names its own descriptor, and the file is zeros once it is dropped.
    let passphrase = secret::Secret::from_memfd(
        secret::sealed_memfd("Zürich y-z 'q' \"w\"".as_bytes())?,
        4096,
    )?;
    let key = luks::KeyFile::new(&passphrase)?;
    let path = key.path();
    ensure!(
        path == format!("/proc/self/fd/{}", key.raw()),
        "the key file's path"
    );
    let run = |inherit: &[std::os::fd::RawFd], script: &str| -> Result<(exec::Exit, Vec<String>)> {
        let mut lines = Vec::new();
        let exit = exec::run_job(
            &exec::Job {
                program: "sh",
                args: &["-c", script],
                env: &[],
                stdin: None,
                inherit,
                timeout: Duration::from_secs(20),
            },
            &mut |line| {
                lines.push(line.to_owned());
                exec::Flow::Continue
            },
            &mut || {},
        )?;
        Ok((exit, lines))
    };
    let read = format!("cat {path}; echo; wc -c < {path}");
    let (exit, lines) = run(&[key.raw()], &read)?;
    ensure!(
        exit.success()
            && lines
                == [
                    "Zürich y-z 'q' \"w\"".to_owned(),
                    format!("{}", "Zürich y-z 'q' \"w\"".len())
                ],
        "a child did not read the passphrase through its descriptor: {lines:?}"
    );
    // Not given to the child, the descriptor is not there (it is close-on-exec).
    let (exit, lines) = run(&[], &format!("cat {path}"))?;
    ensure!(
        !exit.success() && !lines.iter().any(|l| l.contains("Zürich")),
        "a child that was not given the key file read it: {lines:?}"
    );
    // Mode 0600 (repart warns about a key file others can read).
    ensure!(
        std::fs::metadata(&path)?.permissions().mode() & 0o777 == 0o600,
        "the key file is readable by others"
    );
    // Dropped, it is wiped: a duplicate of the descriptor reads nothing, and
    // the zeros are not the passphrase.
    // SAFETY: `dup` of a descriptor this function owns.
    let dup = unsafe { libc::dup(key.raw()) };
    ensure!(dup >= 0, "dup");
    // SAFETY: `dup` is a descriptor this function just made and owns.
    let dup = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(dup) };
    drop(key);
    ensure!(
        std::fs::File::from(dup).metadata()?.len() == 0,
        "the key file still holds bytes after it was dropped"
    );

    // What a passphrase may hold: it is typed at a prompt, so a line break (the
    // end of what is typed), a tab or a NUL cannot be in it.
    for (text, ok) in [
        ("correct horse battery", true),
        ("Zürich ß 日本語 x", true),
        (" leading and trailing ", true),
        ("", false),
        ("a\nb", false),
        ("a\rb", false),
        ("a\tb", false),
        ("a\0b", false),
        ("\u{7f}", false),
        (&"x".repeat(512), true),
        (&"x".repeat(513), false),
    ] {
        ensure!(
            ilex_installer_model::plan::passphrase_problem(text).is_none() == ok,
            "the passphrase rule for {text:?}"
        );
        let secret = secret::Secret::from_memfd(secret::sealed_memfd(text.as_bytes())?, 4096);
        if text.is_empty() {
            ensure!(secret.is_err(), "an empty secret was taken");
        } else {
            ensure!(
                secret::disk_passphrase(&secret?).is_ok() == ok,
                "the helper's passphrase rule for {text:?}"
            );
        }
    }
    ensure!(
        secret::disk_passphrase(&secret::Secret::from_memfd(
            secret::sealed_memfd(b"\xff\xfe")?,
            4096
        )?)
        .is_err(),
        "a passphrase that is not UTF-8 was accepted"
    );
    Ok(())
}

/// The runner of long jobs: merged output in order, a secret on standard
/// input, a deadline that ends the whole process group, an early stop.
fn check_jobs(scratch: &Path) -> Result<()> {
    // Everything this writes is in a directory of its own, removed with it: the
    // helper's scratch directory must be empty when the check ends.
    let work = TestDir::new(scratch, "jobs")?;
    let parent = work.0.as_path();
    let run = |script: &str,
               stdin: Option<&[u8]>,
               timeout: Duration|
     -> Result<(exec::Exit, Vec<String>, u32)> {
        let mut lines = Vec::new();
        let mut ticks = 0;
        let exit = exec::run_job(
            &exec::Job {
                program: "sh",
                args: &["-c", script],
                env: &[("ILEX_CHECK", "set")],
                stdin,
                inherit: &[],
                timeout,
            },
            &mut |line| {
                lines.push(line.to_owned());
                exec::Flow::Continue
            },
            &mut || ticks += 1,
        )?;
        Ok((exit, lines, ticks))
    };
    let (exit, lines, _) = run(
        "echo out; echo err >&2; printf 'bar\\r50%%\\rdone\\n'; read secret; echo \"got $secret\"; echo $ILEX_CHECK; echo ${HOME:-nohome}; exit 3",
        Some(b"hunter2\n"),
        Duration::from_secs(20),
    )?;
    ensure!(
        exit.code == Some(3) && !exit.stopped,
        "the exit status: {exit:?}"
    );
    ensure!(
        lines
            == [
                "out",
                "err",
                "bar",
                "50%",
                "done",
                "got hunter2",
                "set",
                "nohome"
            ],
        "the lines of a job: {lines:?}"
    );
    // One tick a second while it runs.
    let (_, _, ticks) = run("sleep 2.5", None, Duration::from_secs(20))?;
    ensure!((2..=3).contains(&ticks), "{ticks} ticks in 2.5 seconds");
    // A deadline ends the job and what it started: a sleeping grandchild, which
    // holds the output pipe, does not outlive it.
    let marker = parent.join("grandchild");
    let started = std::time::Instant::now();
    let error = run(
        &format!("sleep 30 & echo $! > {}; wait", marker.display()),
        None,
        Duration::from_secs(2),
    )
    .err()
    .context("a job past its deadline was not stopped")?;
    ensure!(
        started.elapsed() < Duration::from_secs(10)
            && format!("{error:#}").contains("did not finish"),
        "the deadline: {error:#}"
    );
    let pid: i32 = fs::read_to_string(&marker)?.trim().parse()?;
    std::thread::sleep(Duration::from_millis(200));
    ensure!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the job's grandchild outlived the job"
    );
    // A sink that has what it wanted stops the job.
    let mut seen = 0;
    let exit = exec::run_job(
        &exec::Job {
            program: "sh",
            args: &["-c", "echo first; sleep 30; echo never"],
            env: &[],
            stdin: None,
            inherit: &[],
            timeout: Duration::from_secs(60),
        },
        &mut |line| {
            seen += 1;
            if line == "first" {
                exec::Flow::Stop
            } else {
                exec::Flow::Continue
            }
        },
        &mut || {},
    )?;
    ensure!(
        exit.stopped && !exit.success() && seen == 1,
        "stopping a job: {exit:?}"
    );
    ensure!(
        exec::clean_line(b"a\x1b[31mb\tc\x07").as_str() == "a[31mbc",
        "control characters of a line"
    );
    ensure!(
        install::json_array(&[
            "Applying".into(),
            "[".into(),
            "  {}".into(),
            "]".into(),
            "done".into()
        ])? == "[\n  {}\n]"
            && install::json_array(&["no list".into()]).is_err()
            && install::json_array(&["[".into()]).is_err(),
        "the partition list of systemd-repart's output"
    );
    // The policy engine's words, as skopeo prints them.
    for (lines, success, expected) in [
        (
            &["Getting image source signatures", "Copying blob sha256:aa"][..],
            false,
            "accepted",
        ),
        (
            &[
                "Error: copying system image from manifest list: Source image rejected: A signature was required, but no signature exists",
            ][..],
            false,
            "rejected",
        ),
        (
            &[
                "Error: Source image rejected: cryptographic signature verification failed: invalid signature",
            ][..],
            false,
            "rejected",
        ),
        (
            &[
                "Error: ... Running image docker://quay.io/fedora/fedora-bootc:45 is rejected by policy.",
            ][..],
            false,
            "rejected",
        ),
        (
            &[
                "Error: initializing source docker://ghcr.io/x: pinging container registry ghcr.io: dial tcp: lookup ghcr.io: no such host",
            ][..],
            false,
            "unreachable",
        ),
        (&[][..], false, "unreachable"),
        // Progress without the line that says the policy engine has let the
        // image through is no acceptance, nor is any progress after an error.
        (&["Copying blob sha256:aa"][..], false, "unreachable"),
        (&["Copying blob sha256:aa"][..], true, "unreachable"),
        (&[][..], true, "unreachable"),
        (
            &[
                "Getting image source signatures",
                "Error: initializing source docker://ghcr.io/x: reading manifest: connection reset",
                "Copying blob sha256:aa",
            ][..],
            false,
            "unreachable",
        ),
        (
            &[
                "Getting image source signatures",
                "time=\"2026-11-26T10:00:00Z\" level=fatal msg=\"copying: i/o timeout\"",
                "Copying blob sha256:aa",
            ][..],
            true,
            "unreachable",
        ),
        (
            &[
                "Error: something broke early",
                "Getting image source signatures",
                "Copying blob sha256:aa",
            ][..],
            false,
            "unreachable",
        ),
        // The whole image copied: the policy allowed it.
        (
            &[
                "Getting image source signatures",
                "Copying blob sha256:aa",
                "Writing manifest to image destination",
            ][..],
            true,
            "accepted",
        ),
        // A rejection after an earlier error line is still a rejection.
        (
            &[
                "Error: retrying: i/o timeout",
                "Error: Source image rejected: A signature was required, but no signature exists",
            ][..],
            false,
            "rejected",
        ),
    ] {
        let mut judge = policy::Judge::new();
        lines.iter().for_each(|l| judge.line(l));
        let verdict = match judge.finish(success) {
            policy::Verdict::Accepted => "accepted",
            policy::Verdict::Rejected(_) => "rejected",
            policy::Verdict::Unreachable(_) => "unreachable",
        };
        ensure!(
            verdict == expected,
            "{lines:?} was judged {verdict}, not {expected}"
        );
    }
    // Logs: one directory per plan, root's alone.
    let logs = stagelog::Logs::new(parent, &"ab".repeat(16))?;
    let mut log = logs.stage(ilex_installer_model::state::Stage::Image)?;
    log.command("bootc", &["install", "to-filesystem"]);
    let mode = |p: &Path| fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
    ensure!(
        mode(logs.dir())? == 0o700
            && mode(&logs.dir().join("04-image.log"))? == 0o600
            && fs::read_to_string(logs.dir().join("04-image.log"))?
                .contains("$ bootc install to-filesystem"),
        "the stage log's modes or content"
    );
    ensure!(
        stagelog::Logs::new(parent, "../../etc").is_err()
            && stagelog::Logs::new(parent, "short").is_err(),
        "a plan id that is not 32 hex digits made a log directory"
    );
    Ok(())
}

fn test_summary() -> ilex_installer_model::state::Summary {
    ilex_installer_model::state::Summary {
        edition: "ilex Glabra".into(),
        composition: "generic.gnome".into(),
        version: Some("45.20261127".into()),
        digest: format!("sha256:{}", "c".repeat(64)),
        disk: "virtio-target".into(),
        disk_label: "QEMU HARDDISK target".into(),
        encrypted: false,
    }
}

/// The state record: written before a stage begins, recovered after a crash,
/// never resumed.
fn check_state(parent: &Path) -> Result<()> {
    use ilex_installer_model::state::{Outcome, Stage, StageStatus, State};
    let dir = TestDir::new(parent, "state")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    let tracker = progress::Tracker::new(&dir.0);
    tracker.set_sender(sender);
    ensure!(
        tracker.state().stage == Stage::Idle && !tracker.active(),
        "a fresh tracker"
    );
    // An `Apply` that is checking says so in `State()`, in the file too, until
    // it refuses (the guard is dropped) or begins (`begin` clears it with the
    // flag that says the installation is active); a helper that died in that
    // window leaves a file that its successor reads as not started.
    let plan = "c".repeat(32);
    {
        let refused = tracker.starting(&plan);
        let during = tracker.state();
        let in_file: State = serde_json::from_slice(&fs::read(dir.0.join("state.json"))?)?;
        ensure!(
            during.starting.as_deref() == Some(plan.as_str())
                && during.busy()
                && !during.active
                && in_file.starting == during.starting,
            "an Apply under way is not in State: {during:?}"
        );
        let successor = progress::Tracker::new(&dir.0);
        ensure!(
            successor.state().starting.is_none() && !successor.state().busy(),
            "a successor read a starting Apply that was never answered"
        );
        // `new` rewrote the file as it reads it; this helper's record is its own.
        tracker.set_plan(&plan);
        // The refusal is recorded with the plan's id before the guard says
        // nothing is being checked any more.
        let expired = ilex_installer_model::plan::PlanError::new(
            ilex_installer_model::plan::ErrorCode::PlanExpired,
            Some("plan"),
            "the plan is 16 minutes old and is valid for 15 minutes: plan again",
        );
        tracker.refused(&plan, &expired);
        let both = tracker.state();
        ensure!(
            both.busy()
                && both
                    .refusal
                    .as_ref()
                    .is_some_and(|r| r.plan == plan && r.error == expired),
            "a refusal is not recorded with its plan while the guard is held: {both:?}"
        );
        drop(refused);
        ensure!(
            tracker.state().starting.is_none() && !tracker.state().busy(),
            "a refused Apply left its starting mark: {:?}",
            tracker.state()
        );
        let in_file: State = serde_json::from_slice(&fs::read(dir.0.join("state.json"))?)?;
        ensure!(
            in_file.refusal == tracker.state().refusal && in_file.refusal.is_some(),
            "the refusal is not in the state file: {in_file:?}"
        );
        // A successor keeps it (it says which plan was refused and why, and is
        // no claim that anything is running).
        ensure!(
            progress::Tracker::new(&dir.0).state().refusal == tracker.state().refusal,
            "a successor lost the refusal"
        );
    }
    {
        // The next `Apply` that takes the lock clears the earlier refusal, and
        // `begin` leaves none.
        let _checking = tracker.starting(&plan);
        ensure!(
            tracker.state().refusal.is_none(),
            "a new Apply kept the earlier refusal: {:?}",
            tracker.state()
        );
        tracker.begin(&plan, &Stage::UNENCRYPTED, test_summary())?;
        ensure!(
            tracker.state().starting.is_none()
                && tracker.state().active
                && tracker.state().refusal.is_none(),
            "begin did not replace the starting mark with the active flag: {:?}",
            tracker.state()
        );
        // An installation that began is not a refusal, whatever `Apply` then
        // returns.
        tracker.refused(
            &plan,
            &ilex_installer_model::plan::PlanError::new(
                ilex_installer_model::plan::ErrorCode::Internal,
                None,
                "the installation thread did not start",
            ),
        );
        ensure!(
            tracker.state().refusal.is_none(),
            "an installation that began was recorded as refused"
        );
    }
    ensure!(
        tracker.state().active && tracker.state().starting.is_none(),
        "the guard changed an installation that began"
    );
    tracker.start(Stage::Partition, "Writing the partition table")?;
    tracker.disk_changed()?;
    tracker.line("Formatting /dev/vda1 as vfat");
    let on_disk: State = serde_json::from_slice(&fs::read(dir.0.join("state.json"))?)?;
    ensure!(
        on_disk.active
            && on_disk.stage == Stage::Partition
            && on_disk.stages.len() == 6
            && on_disk.stages[0].status == StageStatus::Running
            && on_disk.stages[1].status == StageStatus::Pending
            && on_disk.disk_changed,
        "the record written before the stage: {on_disk:?}"
    );
    ensure!(
        mode_of(&dir.0.join("state.json"))? == 0o600,
        "the state file is not root's alone"
    );
    // A helper that dies here leaves exactly that file. The next one reads it.
    let successor = progress::Tracker::new(&dir.0);
    let recovered = successor.state();
    ensure!(
        !recovered.active
            && recovered.disk_changed
            && recovered.stages[0].status == StageStatus::Failed
            && matches!(&recovered.outcome, Some(Outcome::Failed { stage: Stage::Partition, reason })
                if reason.contains("partition") && reason.contains("disk was already changed")),
        "a crashed installation was not reported as failed: {recovered:?}"
    );
    // Nothing resumes: the new helper has no installation to continue.
    ensure!(!successor.active(), "a recovered installation is active");
    // Progress goes up within a stage and never down, and is not sent more
    // than twice a second.
    std::thread::sleep(Duration::from_millis(600));
    tracker.progress(0.6, Some("more than half"));
    tracker.progress(0.9, Some("dropped: too soon after the last"));
    ensure!(
        (tracker.state().progress - 0.6).abs() < 1e-9,
        "progress sent too often: {}",
        tracker.state().progress
    );
    std::thread::sleep(Duration::from_millis(600));
    tracker.progress(0.2, Some("less than before"));
    ensure!(
        (tracker.state().progress - 0.6).abs() < 1e-9
            && tracker.state().message == "less than before",
        "progress went backwards: {:?}",
        tracker.state()
    );
    tracker.done(Stage::Partition);
    tracker.fail(Stage::Image, "bootc exited 1");
    let events: Vec<progress::Event> = receiver.try_iter().collect();
    ensure!(
        events.iter().any(|e| matches!(e, progress::Event::Progress { stage: Stage::Partition, fraction, .. } if *fraction == 0.0))
            && matches!(events.last(), Some(progress::Event::Finished(Outcome::Failed { stage: Stage::Image, .. }))),
        "the events: {events:?}"
    );
    // A stage whose record cannot be written does not begin.
    let blocked = dir.0.join("not-a-directory");
    write(&blocked, "")?;
    let stuck = progress::Tracker::new(&blocked);
    ensure!(
        stuck
            .begin(&"d".repeat(32), &Stage::UNENCRYPTED, test_summary())
            .is_err()
            && !stuck.active(),
        "an installation began without its record"
    );
    // An old state file of an installation that ended is kept as it was.
    let done = progress::recover(
        State {
            outcome: Some(Outcome::Success),
            ..State::idle(1)
        },
        2,
    );
    ensure!(
        done.outcome == Some(Outcome::Success) && done.updated == 1,
        "a finished state was changed"
    );
    // `State` documents without the fields later changes add still parse.
    let old: State = serde_json::from_str(
        r#"{"schema":1,"stage":"idle","plan":null,"progress":0.0,"message":"","outcome":null,"updated":1}"#,
    )?;
    ensure!(
        old.stages.is_empty() && !old.active && old.starting.is_none() && !old.busy(),
        "an old state document"
    );
    Ok(())
}

fn mode_of(path: &Path) -> Result<u32> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

/// What is found below the target and what is written into it.
fn check_target(parent: &Path) -> Result<()> {
    let tree = TestDir::new(parent, "target")?;
    let root = &tree.0;
    let checksum = "ab".repeat(32);
    let deploy = root.join("ostree/deploy/default/deploy");
    fs::create_dir_all(deploy.join(format!("{checksum}.0")))?;
    fs::create_dir_all(root.join("ostree/deploy/default/var"))?;
    ensure!(
        target::find_deployment(root).is_err(),
        "a deployment without an origin was found"
    );
    write(
        &deploy.join(format!("{checksum}.0.origin")),
        "[origin]\ncontainer-image-reference=ostree-image-signed:docker://ghcr.io/reidond/ilex:generic.gnome\n",
    )?;
    let found = target::find_deployment(root)?;
    ensure!(
        found.path == deploy.join(format!("{checksum}.0"))
            && found.checksum == checksum
            && found.var == root.join("ostree/deploy/default/var")
            && target::origin_reference(&found.origin)
                == Some("ostree-image-signed:docker://ghcr.io/reidond/ilex:generic.gnome"),
        "the deployment: {found:?}"
    );
    ensure!(
        target::expected_origin("ghcr.io/reidond/ilex:generic.gnome")
            == "ostree-image-signed:docker://ghcr.io/reidond/ilex:generic.gnome",
        "the origin an enforcing install writes"
    );
    // Two deployments, a stray stateroot or a link in place of a directory is
    // not what a fresh install makes.
    fs::create_dir_all(deploy.join(format!("{}.0", "cd".repeat(32))))?;
    ensure!(
        target::find_deployment(root).is_err(),
        "two deployments were accepted"
    );
    fs::remove_dir(deploy.join(format!("{}.0", "cd".repeat(32))))?;
    fs::create_dir_all(root.join("ostree/deploy/other"))?;
    ensure!(
        target::find_deployment(root).is_err(),
        "two stateroots were accepted"
    );
    fs::remove_dir(root.join("ostree/deploy/other"))?;
    fs::create_dir_all(root.join("elsewhere"))?;
    fs::remove_dir(deploy.join(format!("{checksum}.0")))?;
    symlink(root.join("elsewhere"), deploy.join(format!("{checksum}.0")))?;
    ensure!(
        target::find_deployment(root).is_err(),
        "a linked deployment was accepted"
    );
    fs::remove_file(deploy.join(format!("{checksum}.0")))?;

    // An encrypted installation's record says what phase 0's own completion
    // says of a disk that unlocks with its passphrase alone.
    let mut encrypted = fixture_plan()?;
    encrypted.request.encrypt = true;
    encrypted.encryption.enabled = true;
    let record: serde_json::Value = serde_json::from_str(&target::setup_record(&encrypted, None))?;
    ensure!(
        record["security"]["tpm"]
            == serde_json::json!({"requested": "auto", "capability": "unknown", "reason": "not-attempted", "status": "passphrase-only"})
            && record["composition"] == "generic.gnome",
        "an encrypted installation's setup.json: {record}"
    );
    encrypted.request.tpm = Some(ilex_installer_model::plan::TpmMode::Off);
    ensure!(
        serde_json::from_str::<serde_json::Value>(&target::setup_record(&encrypted, None))?["security"]
            ["tpm"]["requested"]
            == "off",
        "the TPM mode the person turned off is not recorded"
    );
    ensure!(
        install::stages(&encrypted).len() == 7 && install::stages(&fixture_plan()?).len() == 6,
        "the stages of an encrypted and of an unencrypted installation"
    );
    // With a recovery key: its stage after the account, and the chip's when it
    // will be enrolled; what the installation did is what setup.json then says.
    encrypted.request.tpm = Some(ilex_installer_model::plan::TpmMode::Auto);
    encrypted.encryption.recovery_key = true;
    ensure!(
        install::stages(&encrypted).len() == 8
            && install::stages(&encrypted)[6] == ilex_installer_model::state::Stage::Recovery,
        "the stages of an encrypted installation with a recovery key"
    );
    encrypted.encryption.tpm.will_enroll = true;
    ensure!(
        install::stages(&encrypted).len() == 9
            && install::stages(&encrypted)[7] == ilex_installer_model::state::Stage::Tpm,
        "the stages of an installation that enrols the security chip"
    );
    for (outcome, want) in [
        (
            TpmOutcome {
                status: TpmStatus::EnrolledUnverified,
                reason: "pcr7-sha256".into(),
            },
            serde_json::json!({"requested": "auto", "capability": "eligible", "reason": "pcr7-sha256", "status": "enrolled-unverified"}),
        ),
        (
            TpmOutcome {
                status: TpmStatus::PassphraseOnly,
                reason: "secure-boot-off".into(),
            },
            serde_json::json!({"requested": "auto", "capability": "eligible", "reason": "secure-boot-off", "status": "passphrase-only"}),
        ),
    ] {
        let record: serde_json::Value = serde_json::from_str(&target::setup_record(
            &encrypted,
            Some(("eligible", &outcome)),
        ))?;
        ensure!(
            record["security"]["tpm"] == want,
            "setup.json for {outcome:?}: {record}"
        );
    }
    // The reserved record of the Anaconda path, key for key.
    let plan = fixture_plan()?;
    let record: serde_json::Value = serde_json::from_str(&target::setup_record(&plan, None))?;
    ensure!(
        record
            == serde_json::json!({
                "schema": 1,
                "composition": "generic.gnome",
                "image": "ghcr.io/reidond/ilex:generic.gnome",
                "profile": {"id": null, "schema": null},
                "security": {"capabilities": {}, "tpm": {"requested": "unspecified", "capability": "unknown", "status": "not-configured"}},
            })
            && target::setup_record(&plan, None).ends_with("}\n"),
        "setup.json: {record}"
    );
    // No line for `/`.
    let (kept, dropped) = target::strip_root_fstab(
        "# comment /\nUUID=1 / btrfs subvol=root 0 0\nUUID=2 /boot ext4 defaults 0 2\n/dev/x\t/\text4\tdefaults\t0 0\n#UUID=3 / x\n",
    );
    ensure!(
        dropped == 2 && kept == "# comment /\nUUID=2 /boot ext4 defaults 0 2\n#UUID=3 / x\n",
        "the fstab: {kept:?} ({dropped})"
    );
    ensure!(target::strip_root_fstab("").0.is_empty(), "an empty fstab");
    // The default target: the administrator's link first, else the vendor's.
    let dep = root.join("dep");
    fs::create_dir_all(dep.join("usr/lib/systemd/system"))?;
    symlink(
        "graphical.target",
        dep.join("usr/lib/systemd/system/default.target"),
    )?;
    ensure!(
        target::default_target(&dep)? == "graphical.target",
        "the vendor default target"
    );
    fs::create_dir_all(dep.join("etc/systemd/system"))?;
    symlink(
        "/usr/lib/systemd/system/multi-user.target",
        dep.join("etc/systemd/system/default.target"),
    )?;
    ensure!(
        target::default_target(&dep)? == "multi-user.target",
        "the administrator's default target"
    );
    // What is mounted of the target: below it, and anywhere its partitions are
    // mounted (bootc leaves /run/bootc/storage), deepest first.
    let mounts = topology::parse_mountinfo(concat!(
        "10 1 0:1 / / rw - ext4 /dev/vda1 rw\n",
        "20 10 0:20 / /run/ilex-installer/target rw - btrfs /dev/vdb3 rw\n",
        "21 20 0:21 / /run/ilex-installer/target/boot rw - ext4 /dev/vdb2 rw\n",
        "22 21 0:22 / /run/ilex-installer/target/boot/efi rw - vfat /dev/vdb1 rw\n",
        "23 10 0:20 /ostree/bootc/storage /run/bootc/storage rw - btrfs /dev/vdb3 rw\n",
        "24 20 0:20 /ostree/deploy/default/var /run/ilex-installer/target/ostree/deploy/default/deploy/x.0/var rw - btrfs /dev/vdb3 rw\n",
        "25 10 0:30 / /run/ilex-installer/target-other rw - tmpfs tmpfs rw\n",
    ))?;
    let nodes: BTreeSet<String> = ["/dev/vdb1", "/dev/vdb2", "/dev/vdb3"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let left = target::leftover_mounts(&mounts, Path::new("/run/ilex-installer/target"), &nodes);
    let at = |point: &str| left.iter().position(|p| p == point);
    ensure!(
        left.len() == 5
            && at("/run/bootc/storage").is_some()
            && at("/run/ilex-installer/target/ostree/deploy/default/deploy/x.0/var")
                < at("/run/ilex-installer/target/boot/efi")
            && at("/run/ilex-installer/target/boot/efi") < at("/run/ilex-installer/target/boot")
            && at("/run/ilex-installer/target/boot") < at("/run/ilex-installer/target"),
        "mounts to remove, children before their parents: {left:?}"
    );
    ensure!(
        target::leftover_mounts(
            &mounts[..1],
            Path::new("/run/ilex-installer/target"),
            &nodes
        )
        .is_empty(),
        "the root file system was to be unmounted"
    );
    // The new partitions are found by UUID and must be what sysfs says: here
    // there is no such link, so none are found.
    let by_partuuid = root.join("by-partuuid");
    fs::create_dir_all(&by_partuuid)?;
    ensure!(
        target::wait_for_partitions(
            &plan,
            &root.join("sys"),
            &by_partuuid,
            Duration::from_millis(300)
        )
        .is_err(),
        "partitions were found where there are none"
    );
    ensure!(
        target::used_bytes(root).is_some_and(|used| used > 0),
        "the bytes in use of a file system"
    );
    // The target's compiled policy: the highest version, files only.
    let dep = root.join("selinux-dep");
    write(&dep.join("etc/selinux/targeted/policy/policy.33"), "x")?;
    write(&dep.join("etc/selinux/targeted/policy/policy.35"), "x")?;
    write(&dep.join("etc/selinux/targeted/policy/policy.bin"), "x")?;
    fs::create_dir_all(dep.join("etc/selinux/targeted/policy/policy.99"))?;
    ensure!(
        target::selinux_policy_file(&dep)? == dep.join("etc/selinux/targeted/policy/policy.35"),
        "the target's policy file"
    );
    ensure!(
        target::selinux_policy_file(&root.join("not-there")).is_err(),
        "a policy was found where there is none"
    );
    // The kernel's partitions of a disk: the directories with a `partition`
    // file, and only those of the disk asked for.
    let sys = root.join("sys");
    write(&sys.join("block/vda/vda1/partition"), "1\n")?;
    write(&sys.join("block/vda/vda2/partition"), "2\n")?;
    write(&sys.join("block/vda/size"), "83886080\n")?;
    fs::create_dir_all(sys.join("block/vda/queue"))?;
    write(&sys.join("block/vdb/size"), "83886080\n")?;
    ensure!(
        target::kernel_partitions(&sys, "vda") == ["vda1", "vda2"]
            && target::kernel_partitions(&sys, "vdb").is_empty()
            && target::kernel_partitions(&sys, "vdz").is_empty(),
        "the kernel's partitions: {:?}",
        target::kernel_partitions(&sys, "vda")
    );
    ensure!(
        target::wait_for_no_partitions(&sys, "vda", Duration::from_millis(300)).is_err()
            && target::wait_for_no_partitions(&sys, "vdb", Duration::from_millis(300)).is_ok(),
        "waiting for the kernel to forget its partitions"
    );
    Ok(())
}

fn fixture_plan() -> Result<ilex_installer_model::plan::Plan> {
    use ilex_installer_model::plan::{Plan, PlanRequest};
    let plan: Plan = serde_json::from_value(serde_json::json!({
        "schema": 1, "id": "e".repeat(32), "created": 1, "valid_seconds": 900,
        "seed": "0b5d9a3e-6a4e-4a2c-9d0e-2f7c7b1c9f11", "edition": "gnome",
        "request": serde_json::from_str::<PlanRequest>(
            r#"{"composition":"generic.gnome","disk":"virtio-blank","mode":"whole-disk","encrypt":false,"administrator":"ilexmaint"}"#,
        )?,
        "composition": {"tag": "generic.gnome", "machine": "generic", "shells": ["gnome"], "reason": null},
        "image": {"target": "ghcr.io/reidond/ilex:generic.gnome", "source": format!("ghcr.io/reidond/ilex@sha256:{}", "a".repeat(64)),
                  "digest": format!("sha256:{}", "a".repeat(64)), "download_bytes": 1, "version": null, "revision": null},
        "disk": {"id": "virtio-blank", "path": "/dev/vda", "model": "", "serial": "blank", "size_bytes": 42949672960u64},
        "partitions": [{"number": 1, "path": "/dev/vda1", "kind": "esp", "label": "ilex-esp",
                        "uuid": "e2857866-e04d-4eb3-915d-101cbf838697", "start_bytes": 1048576,
                        "size_bytes": 536870912, "filesystem": "vfat", "encrypted": false}],
        "erases": {"disk": "virtio-blank", "partitions": [], "systems": []},
        "encryption": {"enabled": false, "tpm": {"requested": "off", "will_enroll": false, "reason": "x"}},
        "warnings": [],
    }))?;
    Ok(plan)
}

/// The comparison of the disk a plan was made for with the one found at Apply.
fn check_apply_rules() -> Result<()> {
    use ilex_installer_model::probe::{Disk, Partition};
    let partition = Partition {
        number: 1,
        path: "/dev/vda1".into(),
        start_bytes: 1 << 20,
        size_bytes: 1 << 30,
        type_id: "0fc63daf-8483-4772-8e79-3d69d8477de4".into(),
        label: "data".into(),
        filesystem: "ext4".into(),
        filesystem_label: "keep".into(),
    };
    let then = Disk {
        id: "virtio-blank".into(),
        path: "/dev/vda".into(),
        model: "QEMU HARDDISK".into(),
        serial: "blank".into(),
        size_bytes: 40 << 30,
        sector_size: 512,
        bus: "virtio".into(),
        removable: false,
        live_medium: false,
        in_use: vec![],
        notes: vec![],
        partition_table: Some("gpt".into()),
        partitions: vec![partition.clone()],
        free_extents: vec![],
        other_systems: vec![],
    };
    ensure!(
        apply::disk_difference(&then, &then.clone()).is_none(),
        "a disk differs from itself"
    );
    // What a disk the helper holds cannot show (an ESP's loaders) and what
    // changes by itself (notes) is not a difference.
    let mut same = then.clone();
    same.notes.push("an ESP that could not be read".into());
    same.other_systems
        .push(ilex_installer_model::probe::OtherSystem {
            kind: "efi-loader".into(),
            partition: None,
            description: "x".into(),
        });
    ensure!(
        apply::disk_difference(&then, &same).is_none(),
        "the loaders of an ESP are compared"
    );
    type Change = Box<dyn Fn(&mut Disk)>;
    let cases: [(&str, Change); 9] = [
        ("id", Box::new(|d| d.id = "virtio-other".into())),
        ("kernel name", Box::new(|d| d.path = "/dev/vdb".into())),
        ("model", Box::new(|d| d.model = "Other".into())),
        ("serial", Box::new(|d| d.serial = "other".into())),
        ("size", Box::new(|d| d.size_bytes += 1)),
        ("bus", Box::new(|d| d.removable = true)),
        ("partition table", Box::new(|d| d.partition_table = None)),
        (
            "partitions",
            Box::new(|d| d.partitions[0].size_bytes += 1 << 20),
        ),
        ("partitions", Box::new(|d| d.partitions.clear())),
    ];
    for (what, change) in cases {
        let mut now = then.clone();
        change(&mut now);
        ensure!(
            apply::disk_difference(&then, &now).is_some_and(|d| d.contains(what)),
            "a change of the disk's {what} was not noticed"
        );
    }
    Ok(())
}

/// Answers mode: finding the volume, the file's rules and the checks that come
/// before the plan.
/// A file system mounted for the length of a check, unmounted when it ends
/// however it ends (lazily when the ordinary unmount fails).
struct Mounted(PathBuf);

impl Mounted {
    fn ext4(image: &Path, at: &Path, options: &str) -> Result<Self> {
        fs::create_dir_all(at)?;
        let (image, at_text) = (image.to_string_lossy(), at.to_string_lossy());
        let mounted = exec::run(
            "mount",
            &["-t", "ext4", "-o", options, &image, &at_text],
            Duration::from_secs(60),
        )?;
        ensure!(
            mounted.success(),
            "mounting {image} at {at_text} ({options}): {}",
            mounted.stderr.trim()
        );
        Ok(Self(at.to_owned()))
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let at = self.0.to_string_lossy().into_owned();
        let done = exec::run("umount", &[&at], Duration::from_secs(60)).is_ok_and(|o| o.success());
        if !done {
            let _ = exec::run("umount", &["-l", &at], Duration::from_secs(60));
        }
    }
}

/// `Journal start:` of an ext4 image (`dumpe2fs -h`): 0 is an empty journal,
/// anything else is a journal holding transactions a replay would apply.
fn journal_start(image: &Path) -> Result<u64> {
    let out = exec::run_ok(
        "dumpe2fs",
        &["-h", &image.to_string_lossy()],
        Duration::from_secs(60),
    )?
    .stdout_text();
    out.lines()
        .find_map(|line| line.strip_prefix("Journal start:"))
        .and_then(|number| number.trim().parse().ok())
        .context("dumpe2fs prints no `Journal start:` (is there a journal?)")
}

/// The stamp written to a real ext4 `/boot` the way the installer writes it:
/// the provisional stamp, the checkpoint of its journal, and the finished stamp
/// over it with its own checkpoint. After each checkpoint the journal is empty
/// and a copy of the file system mounted `ro,noload` (as answers mode reads a
/// disk: nothing is replayed) finds the stamp. A directory that is not on ext4
/// has no checkpoint to make, and that is an error rather than a silent skip.
/// (A copy of a backing file cannot show what a device's write cache loses at a
/// power cut: the trial that does is described in the pull request.)
fn check_checkpoint(root: &Path, digests: &[String], own: &str) -> Result<()> {
    let image = root.join("boot.img");
    let copy = root.join("boot-copy.img");
    let at = root.join("boot-ext4");
    let seen_at = root.join("boot-ext4-copy");
    fs::File::create(&image)?.set_len(32 << 20)?;
    exec::run_ok(
        "mkfs.ext4",
        &[
            "-q",
            "-F",
            "-L",
            target::BOOT_LABEL,
            &image.to_string_lossy(),
        ],
        Duration::from_secs(60),
    )?;
    // Read a copy of the image the way answers mode reads the disk.
    let read_copy = |want: &[String]| -> Result<()> {
        fs::copy(&image, &copy)?;
        let mounted = Mounted::ext4(&copy, &seen_at, "loop,ro,noload,nodev,nosuid,noexec")?;
        let held = fs::read_to_string(seen_at.join(target::REQUEST_STAMP))
            .context("the stamp is not on a read-only, no-replay mount of the copy")
            .and_then(|text| stamp::parse(&text));
        drop(mounted);
        fs::remove_file(&copy)?;
        ensure!(
            held? == want,
            "a no-replay mount of the copy holds another stamp"
        );
        Ok(())
    };
    let stamp_file = at.join(target::REQUEST_STAMP);
    {
        let _mounted = Mounted::ext4(&image, &at, "loop,nodev,nosuid,noexec")?;
        // A directory that is not on ext4 cannot be checkpointed: an error.
        ensure!(
            stamp::checkpoint(Path::new("/proc")).is_err(),
            "a directory of procfs was checkpointed"
        );
        ensure!(
            stamp::write_provisional(&stamp_file, digests)?,
            "no provisional stamp was written"
        );
        stamp::checkpoint(&at)?;
        ensure!(
            journal_start(&image)? == 0,
            "the journal of /boot still holds transactions after its checkpoint"
        );
        read_copy(digests)?;
        let finished = stamp::merge(digests, &[], own);
        stamp::write_finished(&stamp_file, &finished)?;
        stamp::checkpoint(&at)?;
        ensure!(
            journal_start(&image)? == 0,
            "the journal of /boot still holds transactions after the finished stamp's checkpoint"
        );
        read_copy(&finished)?;
        // A file that is not a directory of the file system is not a mount point
        // to checkpoint: nothing is made durable on its behalf.
        ensure!(
            stamp::checkpoint(&stamp_file).is_err(),
            "a file was checkpointed as a directory"
        );
    }
    fs::remove_file(&image)?;
    Ok(())
}

fn check_answers(parent: &Path) -> Result<()> {
    use ilex_installer_model::probe::{Disk, OtherSystem, Partition};
    let tree = TestDir::new(parent, "answers")?;
    let root = &tree.0;

    // Which volumes carry an answers label, from lsblk's listing.
    let listing = |rows: &str| format!(r#"{{"blockdevices": [{rows}]}}"#);
    let row = |path: &str, label: &str, fstype: &str, kind: &str, number: &str| {
        format!(
            r#"{{"path":"{path}","label":{label},"fstype":{fstype},"type":"{kind}","maj:min":"{number}"}}"#
        )
    };
    let text = |value: &str| format!("\"{value}\"");
    let found = |rows: &[String]| answers::parse_volumes(&listing(&rows.join(",")));
    ensure!(
        found(&[
            row("/dev/vda", "null", "null", "disk", "252:0"),
            row("/dev/vda1", &text("ESP"), &text("vfat"), "part", "252:1"),
        ])?
        .is_empty(),
        "a volume with another label was taken for the answers"
    );
    let ext4 = found(&[row(
        "/dev/vdb",
        &text("ILEX-ANSWERS"),
        &text("ext4"),
        "disk",
        "252:16",
    )])?;
    ensure!(
        ext4 == [answers::Volume {
            path: "/dev/vdb".into(),
            fs: "ext4".into(),
            number: (252, 16)
        }],
        "the answers volume: {ext4:?}"
    );
    // The FAT name, on a partition (children are walked), in either case on a
    // file system that keeps no case, and nowhere else.
    for (label, fs, kind, wanted) in [
        ("ILEXANSWERS", "vfat", "part", true),
        ("ilexanswers", "vfat", "part", true),
        ("ILEX-ANSWERS", "ext4", "part", true),
        ("ilex-answers", "ext4", "part", false),
        ("ILEXANSWERS", "ext4", "part", true),
        ("ILEXANSWERS", "vfat", "loop", false),
        ("ILEXANSWERS", "vfat", "lvm", false),
        ("ILEXANSWERS", "", "part", false),
        ("ILEX-ANSWERS-2", "ext4", "part", false),
    ] {
        let rows = [format!(
            r#"{{"path":"/dev/sdb","label":null,"fstype":null,"type":"disk","maj:min":"8:16","children":[{}]}}"#,
            row(
                "/dev/sdb1",
                &text(label),
                &if fs.is_empty() {
                    "null".into()
                } else {
                    text(fs)
                },
                kind,
                "8:17"
            )
        )];
        let volumes = found(&rows)?;
        ensure!(
            volumes.len() == usize::from(wanted),
            "a {fs} {kind} labelled {label:?}: {volumes:?}"
        );
    }
    // Two answers volumes are not a choice anyone made.
    let two = found(&[
        row(
            "/dev/sdb1",
            &text("ILEX-ANSWERS"),
            &text("ext4"),
            "part",
            "8:17",
        ),
        row(
            "/dev/sdc1",
            &text("ILEXANSWERS"),
            &text("vfat"),
            "part",
            "8:33",
        ),
    ])?;
    ensure!(
        two.len() == 2 && answers::find_volume(&two).is_err(),
        "two answers volumes were not refused"
    );
    ensure!(
        answers::find_volume(&[]).is_err() && answers::find_volume(&ext4).is_ok(),
        "finding the one volume"
    );
    ensure!(
        answers::parse_volumes("{}").is_err()
            && answers::parse_volumes(&listing(&row(
                "/dev/x",
                &text("ILEXANSWERS"),
                &text("vfat"),
                "part",
                "no"
            )))
            .is_err(),
        "a listing without numbers was accepted"
    );
    // The file systems an answers volume may hold include FAT, whose labels
    // are 11 characters, and not NTFS.
    ensure!(
        answers::FILE_SYSTEMS.contains(&"vfat")
            && answers::FILE_SYSTEMS.contains(&"ext4")
            && !answers::FILE_SYSTEMS.contains(&"ntfs")
            && answers::VOLUME_LABELS.iter().any(|l| l.len() == 11),
        "the volume rules: a label a FAT volume can carry"
    );

    // What is mounted is the device that was chosen.
    let table = "36 35 252:17 / /run/ilex-installer/answers-x/volume ro,nosuid,nodev,noexec - vfat /dev/vdb1 ro\n\
                 37 35 0:44 / /run/other rw - tmpfs tmpfs rw\n";
    ensure!(
        answers::mounted_number(table, "/run/ilex-installer/answers-x/volume") == Some((252, 17))
            && answers::mounted_number(table, "/run/other") == Some((0, 44))
            && answers::mounted_number(table, "/run/nothing").is_none(),
        "the device a mount is made of"
    );

    // The volume must not be on the disk the answers would erase: by number.
    ensure!(
        answers::volume_on_disk((8, 1), Some((8, 0)), (8, 0), &[(8, 1), (8, 2)])
            && answers::volume_on_disk((8, 0), None, (8, 0), &[])
            && answers::volume_on_disk((8, 2), None, (8, 0), &[(8, 1), (8, 2)])
            && answers::volume_on_disk((8, 9), Some((8, 0)), (8, 0), &[])
            && !answers::volume_on_disk((8, 17), Some((8, 16)), (8, 0), &[(8, 1), (8, 2)])
            && !answers::volume_on_disk((9, 0), None, (8, 0), &[(8, 1)]),
        "which disk an answers volume is on"
    );

    // A disk that already holds a system is not erased by default.
    let partition = |label: &str, fs: &str| Partition {
        number: 1,
        path: "/dev/vdc1".into(),
        start_bytes: 1 << 20,
        size_bytes: 1 << 30,
        type_id: "0fc63daf-8483-4772-8e79-3d69d8477de4".into(),
        label: label.into(),
        filesystem: fs.into(),
        filesystem_label: String::new(),
    };
    let disk = |partitions: Vec<Partition>, systems: Vec<OtherSystem>| Disk {
        id: "virtio-x".into(),
        path: "/dev/vdc".into(),
        model: String::new(),
        serial: "x".into(),
        size_bytes: 40 << 30,
        sector_size: 512,
        bus: "virtio".into(),
        removable: false,
        live_medium: false,
        in_use: vec![],
        notes: vec![],
        partition_table: (!partitions.is_empty()).then(|| "gpt".to_owned()),
        partitions,
        free_extents: vec![],
        other_systems: systems,
    };
    ensure!(
        answers::existing_systems(&disk(vec![], vec![])).is_empty(),
        "a blank disk holds a system"
    );
    ensure!(
        answers::existing_systems(&disk(vec![partition("data", "")], vec![])).is_empty(),
        "an unlabelled partition of no system was taken for one"
    );
    ensure!(
        answers::existing_systems(&disk(vec![partition("ilex-root", "btrfs")], vec![]))
            .iter()
            .any(|f| f.contains("ilex-root")),
        "an ilex partition was not noticed"
    );
    let windows = OtherSystem {
        kind: "windows".into(),
        partition: Some("/dev/vdc1".into()),
        description: "NTFS filesystem \"Windows\"".into(),
    };
    let loader = OtherSystem {
        kind: "efi-loader".into(),
        partition: Some("/dev/vdc1".into()),
        description: "EFI loader directory EFI/ubuntu".into(),
    };
    ensure!(
        answers::existing_systems(&disk(vec![partition("", "ntfs")], vec![windows, loader])).len()
            == 2,
        "another system and a loader were not both noticed"
    );

    // Which composition an unattended installation is for: the answers' own, the
    // installer's selection, and nothing guessed when the boot's choice failed.
    ensure!(
        answers::choose_composition(Some("ms-7c35.gnome"), None, Some("unknown"))?.is_empty()
            && answers::choose_composition(None, Some("generic.gnome"), None)? == "generic.gnome"
            && answers::choose_composition(None, Some("generic.gnome"), Some("stale"))?
                == "generic.gnome",
        "the composition that is chosen"
    );
    let failed =
        answers::choose_composition(None, None, Some("ilex.composition=nonsense is not offered"))
            .err()
            .context("a failed boot choice was replaced by detection")?
            .to_string();
    ensure!(
        failed.contains("not valid") && failed.contains("nonsense"),
        "the refusal for a failed boot choice: {failed}"
    );
    ensure!(
        answers::choose_composition(None, None, None).is_err(),
        "no selection at all was accepted"
    );

    // Whole-disk signatures and an unreadable EFI system partition count as
    // another system: the answers must say `reinstall = true` for them too.
    let whole_pv = disks::classify_whole_disk("/dev/vdc", "LVM2_member", "")
        .context("a whole-disk LVM physical volume was not classified")?;
    let unreadable = OtherSystem {
        kind: "unreadable-esp".into(),
        partition: Some("/dev/vdc1".into()),
        description: "an EFI system partition (/dev/vdc1) that could not be read (mount failed)"
            .into(),
    };
    ensure!(
        answers::existing_systems(&disk(vec![], vec![whole_pv.clone()])).len() == 1
            && answers::existing_systems(&disk(vec![], vec![unreadable])).len() == 1,
        "a whole-disk signature or an unreadable EFI system partition was not noticed"
    );
    ensure!(
        disks::classify_whole_disk("/dev/vdc", "", "").is_none(),
        "a disk with no signature was classified"
    );

    // What the boot recorded, and the volume read later must be exactly it.
    let recorded = answers::parse_marker("252:16\n")?;
    ensure!(recorded == [(252, 16)], "the boot's record: {recorded:?}");
    ensure!(
        answers::parse_marker("")?.is_empty()
            && answers::parse_marker("252:16\n252:32\n")?.len() == 2
            && answers::parse_marker("252:x\n").is_err()
            && answers::parse_marker("garbage\n").is_err(),
        "the boot's record is parsed loosely"
    );
    let stick = |path: &str, number| answers::Volume {
        path: path.into(),
        fs: "ext4".into(),
        number,
    };
    ensure!(
        answers::check_boot_volume(&recorded, &[stick("/dev/vdb", (252, 16))]).is_ok(),
        "the volume the boot recorded was refused"
    );
    for (what, recorded, found) in [
        (
            "a stick swapped for another",
            vec![(252, 16)],
            vec![stick("/dev/vdc", (252, 32))],
        ),
        (
            "a second stick plugged in",
            vec![(252, 16)],
            vec![stick("/dev/vdb", (252, 16)), stick("/dev/vdc", (252, 32))],
        ),
        ("the stick removed", vec![(252, 16)], vec![]),
        (
            "two sticks at boot",
            vec![(252, 16), (252, 32)],
            vec![stick("/dev/vdb", (252, 16))],
        ),
        (
            "nothing recorded",
            vec![],
            vec![stick("/dev/vdb", (252, 16))],
        ),
    ] {
        ensure!(
            answers::check_boot_volume(&recorded, &found).is_err(),
            "{what}: the volume was read"
        );
    }

    // The stamp: the request's digest, never derived from a secret.
    let request = |administrator: &str, host: &str| {
        ilex_installer_model::plan::PlanRequest::parse(&format!(
            r#"{{"composition":"generic.gnome","disk":"virtio-x","mode":"whole-disk","encrypt":false,"host_name":"{host}","administrator":"{administrator}"}}"#
        ))
    };
    let first = target::request_digest(&request("ilexmaint", "ilextest")?);
    ensure!(
        first.len() == 64
            && first
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && first == target::request_digest(&request("ilexmaint", "ilextest")?)
            && first != target::request_digest(&request("ilexmaint", "other")?)
            && first != target::request_digest(&request("someone", "ilextest")?),
        "the request's digest does not follow the request"
    );
    let second = target::request_digest(&request("ilexmaint", "ilextest-two")?);
    let third = target::request_digest(&request("ilexmaint", "ilextest-three")?);
    ensure!(
        first != second && second != third,
        "different requests have the same digest"
    );

    // The stamp holds many digests, one per line: the request of this
    // installation, the earlier stamp's, and the answers refused in this boot.
    ensure!(
        stamp::holds(std::slice::from_ref(&first), &first)
            && stamp::holds(&[second.clone(), first.clone()], &first)
            && !stamp::holds(&[], &first)
            && !stamp::holds(&[second.clone(), "0".repeat(64)], &first),
        "the stamp is compared loosely"
    );
    ensure!(
        stamp::parse(&format!("{first}\n{second}\n"))? == [first.clone(), second.clone()]
            && stamp::parse(&format!("\n{first}\r\n\n{second}"))?
                == [first.clone(), second.clone()]
            && stamp::parse(&format!("{first}\n{first}\n"))? == [first.clone()],
        "a stamp of digests was misread"
    );
    for (what, text) in [
        ("an empty stamp", String::new()),
        ("a blank stamp", "\n \n".to_owned()),
        ("a short digest", format!("{}\n", &first[..63])),
        ("a long digest", format!("{first}0\n")),
        ("a capital digit", format!("{}\n", first.to_uppercase())),
        ("text after a digest", format!("{first}\nhello\n")),
        ("two digests on a line", format!("{first} {second}\n")),
    ] {
        ensure!(
            stamp::parse(&text).is_err(),
            "{what} was taken for a stamp of digests"
        );
    }
    ensure!(
        stamp::parse(&"a".repeat(64)).is_ok()
            && stamp::parse(
                &(0..=stamp::MAX_DIGESTS)
                    .map(|n| format!("{n:064x}\n"))
                    .collect::<String>()
            )
            .is_err(),
        "the stamp's size is not bounded"
    );
    // What an installation writes: the earlier stamp's digests, the answers seen
    // in this boot, then its own; each once, at the place of its last mention (a
    // digest that comes again is moved to the newest place, not left where it
    // was first), the oldest dropped when there are too many and its own never.
    ensure!(
        stamp::merge(
            std::slice::from_ref(&first),
            std::slice::from_ref(&second),
            &third
        ) == [first.clone(), second.clone(), third.clone()]
            && stamp::merge(&[], &[], &first) == [first.clone()]
            && stamp::merge(
                std::slice::from_ref(&first),
                std::slice::from_ref(&first),
                &first
            ) == [first.clone()]
            && stamp::merge(
                &[second.clone(), first.clone()],
                std::slice::from_ref(&second),
                &third
            ) == [first.clone(), second.clone(), third.clone()]
            && stamp::merge(&[first.clone(), second.clone()], &[], &first)
                == [second.clone(), first.clone()]
            && stamp::union(
                &[first.clone(), second.clone()],
                &[third.clone(), first.clone()]
            ) == [second.clone(), third.clone(), first.clone()]
            && stamp::render(&[first.clone(), second.clone()]) == format!("{first}\n{second}\n")
            && stamp::parse(&format!("{first}\n{second}\n{first}\n"))?
                == [second.clone(), first.clone()],
        "the stamp's digests were merged wrongly"
    );
    // An overfull stamp is cut at its oldest digests, and a digest that comes
    // again (as one of the answers seen, or as the installation's own) is the
    // newest, not the oldest: it is never what is cut.
    let many: Vec<String> = (0..stamp::MAX_DIGESTS + 5)
        .map(|n| format!("{n:064x}"))
        .collect();
    let merged = stamp::merge(&many, std::slice::from_ref(&second), &first);
    ensure!(
        merged.len() == stamp::MAX_DIGESTS
            && merged.last() == Some(&first)
            && merged.contains(&second)
            && !merged.contains(&many[0]),
        "an overfull stamp was not cut at its oldest digests"
    );
    let oldest = &many[0];
    for (what, seen, own) in [
        (
            "answers seen that are the oldest digest",
            std::slice::from_ref(oldest),
            &first,
        ),
        (
            "an installation whose own digest is the oldest",
            &[][..],
            oldest,
        ),
        (
            "answers seen and an installation that are both the oldest digest",
            std::slice::from_ref(oldest),
            oldest,
        ),
    ] {
        let merged = stamp::merge(&many, seen, own);
        ensure!(
            merged.len() == stamp::MAX_DIGESTS
                && merged.last() == Some(own)
                && merged.contains(oldest)
                && !merged.contains(&many[1])
                && merged
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    == merged.len(),
            "{what} was cut from an overfull stamp, or listed twice"
        );
    }
    // The digest that was seen is the one before the installation's own.
    let merged = stamp::merge(&many, std::slice::from_ref(oldest), &first);
    ensure!(
        merged[merged.len() - 2] == *oldest,
        "answers seen again were not the newest but one"
    );
    // The same at the size the stamp is read at: a stamp of exactly the most
    // digests, with one answers seen that is its oldest.
    let full = &many[..stamp::MAX_DIGESTS];
    let merged = stamp::merge(full, std::slice::from_ref(&full[0]), &second);
    ensure!(
        merged.len() == stamp::MAX_DIGESTS
            && merged.contains(&full[0])
            && !merged.contains(&full[1])
            && merged.last() == Some(&second),
        "a full stamp lost the digest of answers seen again"
    );
    // The provisional stamp, left on the new `/boot` as soon as it is mounted:
    // what the earlier stamps held and what answers mode saw, without the
    // request being installed, so that a reboot before the image is in neither
    // loses them nor makes the answers that failed "already installed".
    let fourth = target::request_digest(&request("ilexmaint", "ilextest-four")?);
    ensure!(
        stamp::provisional(
            &[first.clone(), second.clone()],
            std::slice::from_ref(&third),
            &fourth
        ) == [first.clone(), second.clone(), third.clone()],
        "the provisional stamp is not the earlier digests then the seen ones"
    );
    // Unattended answers A alone (seen, nothing carried): nothing is kept, so
    // that A is still not "already installed" after a failure and a reboot.
    ensure!(
        stamp::provisional(&[], std::slice::from_ref(&first), &first).is_empty()
            && stamp::provisional(&[], &[], &first).is_empty(),
        "the answers being installed were kept in the provisional stamp"
    );
    // Answers refused in favour of this installation stay; the installation's
    // own digest is out of what was only *seen* and the others keep their order.
    ensure!(
        stamp::provisional(
            std::slice::from_ref(&first),
            &[second.clone(), third.clone(), second.clone()],
            &third
        ) == [first.clone(), second.clone()]
            && stamp::provisional(
                std::slice::from_ref(&first),
                std::slice::from_ref(&second),
                &third
            ) == [first.clone(), second.clone()],
        "the provisional stamp lost a refused digest, or kept a seen request's own"
    );
    // A digest the disk's stamp held (or this boot's record of wiped stamps)
    // stays, whatever the request being installed is: a person's installation
    // whose plan equals answers A's (the password is not in the digest) has
    // own == A, A is in what the wiped stamp held, and A must stay refused after
    // a reboot, or a stick with A erases the person's installation.
    ensure!(
        stamp::provisional(
            std::slice::from_ref(&first),
            std::slice::from_ref(&first),
            &first
        ) == [first.clone()]
            && stamp::provisional(std::slice::from_ref(&first), &[], &first) == [first.clone()]
            && stamp::provisional(
                &[first.clone(), second.clone()],
                &[third.clone(), first.clone()],
                &first
            ) == [first.clone(), second.clone(), third.clone()]
            && stamp::provisional(
                &[first.clone(), second.clone()],
                std::slice::from_ref(&third),
                &second
            ) == [first.clone(), second.clone(), third.clone()],
        "a digest the earlier stamp held was dropped from the provisional stamp because it is this request's own"
    );
    // The finished stamp has the request's own digest last, once, wherever the
    // provisional one held it.
    ensure!(
        stamp::merge(
            std::slice::from_ref(&first),
            std::slice::from_ref(&third),
            &first
        ) == [third.clone(), first.clone()],
        "the finished stamp lost the request's own digest, or kept it twice"
    );
    // The finished stamp is the provisional one and the request's own, last.
    ensure!(
        stamp::merge(
            &[first.clone(), second.clone()],
            std::slice::from_ref(&third),
            &fourth
        ) == [first.clone(), second.clone(), third.clone(), fourth.clone()]
            && stamp::provisional(
                &[first.clone(), second.clone()],
                std::slice::from_ref(&third),
                &fourth
            )[..]
                == stamp::merge(
                    &[first.clone(), second.clone()],
                    std::slice::from_ref(&third),
                    &fourth
                )[..3],
        "the finished stamp is not the provisional one and the request's own"
    );
    // A digest of the oldest, named again among the seen, is kept (moved) and
    // the cut is of the oldest unnamed. The request's own digest is out of what
    // was only seen, never out of the carried ones.
    let many: Vec<String> = (0..stamp::MAX_DIGESTS + 5)
        .map(|n| format!("{n:064x}"))
        .collect();
    let cut = stamp::provisional(&many, std::slice::from_ref(&many[0]), &many[10]);
    ensure!(
        cut.len() == stamp::MAX_DIGESTS
            && cut.last() == Some(&many[0])
            && cut.contains(&many[10])
            && !cut.contains(&many[1]),
        "an overfull provisional stamp is cut wrongly: {} digests",
        cut.len()
    );
    // A full list of carried digests loses none because the request's own is one
    // of them (the old rule took it out and kept 255); one that is only seen is
    // not added to it.
    let full = &many[..stamp::MAX_DIGESTS];
    ensure!(
        stamp::provisional(full, &[], &full[10]) == full
            && stamp::provisional(full, std::slice::from_ref(&full[10]), &full[10]) == full
            && stamp::provisional(full, std::slice::from_ref(&many[255]), &many[255]) == full
            && stamp::provisional(&full[1..], std::slice::from_ref(&full[0]), &full[0])
                == full[1..],
        "a full list of carried digests lost one because it is the request's own"
    );
    // Written on the new `/boot` the way finalise's file is (root's mode 600, a
    // new file renamed into place and made durable); nothing for no digest.
    let boot = root.join("boot-stamp");
    let provisional_file = boot.join(target::REQUEST_STAMP);
    fs::create_dir_all(&boot)?;
    ensure!(
        !stamp::write_provisional(&provisional_file, &[])? && !provisional_file.exists(),
        "no digest to keep wrote a provisional stamp"
    );
    ensure!(
        fs::read_dir(&boot)?.next().is_none(),
        "no digest to keep left something in /boot"
    );
    ensure!(
        stamp::write_provisional(&provisional_file, &[first.clone(), second.clone()])?
            && fs::read_to_string(&provisional_file)? == format!("{first}\n{second}\n")
            && fs::metadata(&provisional_file)?.permissions().mode() & 0o777 == 0o600
            && fs::read_dir(&boot)?.count() == 1,
        "the provisional stamp is not one file of digests, mode 600, with no partial file beside it"
    );
    // What answers mode reads of it is a stamp: the digests, not "no digest".
    ensure!(
        stamp::parse(&fs::read_to_string(&provisional_file)?)? == [first.clone(), second.clone()],
        "answers mode cannot read the provisional stamp"
    );
    // The finished stamp is written over it, and the file is one file still.
    // A new file renamed over it, not the old one truncated and written in
    // place: the old file's inode is not the new file's, and the mode is 600.
    let provisional_inode = std::os::unix::fs::MetadataExt::ino(&fs::metadata(&provisional_file)?);
    stamp::write_finished(&provisional_file, &[first.clone(), fourth.clone()])?;
    ensure!(
        stamp::parse(&fs::read_to_string(&provisional_file)?)? == [first.clone(), fourth.clone()]
            && std::os::unix::fs::MetadataExt::ino(&fs::metadata(&provisional_file)?)
                != provisional_inode
            && fs::metadata(&provisional_file)?.permissions().mode() & 0o777 == 0o600
            && fs::read_dir(&boot)?.count() == 1,
        "the finished stamp did not replace the provisional one as a new file"
    );
    fs::remove_dir_all(&boot)?;
    // A link where the file goes is not followed: the stamp is a new file.
    fs::create_dir_all(&boot)?;
    symlink("elsewhere", &provisional_file)?;
    ensure!(
        stamp::write_provisional(&provisional_file, std::slice::from_ref(&first)).is_ok()
            && !fs::symlink_metadata(&provisional_file)?
                .file_type()
                .is_symlink()
            && fs::read_to_string(&provisional_file)? == format!("{first}\n"),
        "a link where the provisional stamp goes was followed"
    );
    fs::remove_dir_all(&boot)?;
    // On a real ext4 file system (a loop file; root only): the stamp is made
    // durable for a reader that does not replay the journal.
    if unsafe { libc::geteuid() } == 0 {
        check_checkpoint(root, &[first.clone(), second.clone()], &third)?;
    }
    // The record of the answers seen: root's own file in /run, written whole.
    let seen = root.join("seen");
    ensure!(
        stamp::read_record(&seen)?.is_empty(),
        "a record that is not there held digests"
    );
    if unsafe { libc::geteuid() } == 0 {
        stamp::record_seen(&seen, &first)?;
        stamp::record_seen(&seen, &second)?;
        stamp::record_seen(&seen, &first)?;
        ensure!(
            stamp::read_record(&seen)? == [second.clone(), first.clone()]
                && fs::metadata(&seen)?.permissions().mode() & 0o777 == 0o600
                && !root.join("seen.partial").exists(),
            "the record of the answers seen was not kept (the same answers seen again are the newest)"
        );
        fs::set_permissions(&seen, fs::Permissions::from_mode(0o644))?;
        ensure!(
            stamp::read_record(&seen).is_err(),
            "a record that others can read was believed"
        );
        fs::remove_file(&seen)?;
        write(&seen, "not a digest\n")?;
        fs::set_permissions(&seen, fs::Permissions::from_mode(0o600))?;
        ensure!(
            stamp::read_record(&seen).is_err(),
            "a record that is not made of digests was believed"
        );
        stamp::record_seen(&seen, &third)?;
        ensure!(
            stamp::read_record(&seen)? == [third.clone()],
            "a record that was not ours was not replaced"
        );
        fs::remove_file(&seen)?;
        symlink("elsewhere", &seen)?;
        ensure!(
            stamp::read_record(&seen).is_err(),
            "a link was followed as the record"
        );
        fs::remove_file(&seen)?;

        // The digests of the stamp a disk held, kept in /run before the wipe
        // takes it, for a retry in the same boot: a failed installation leaves
        // a new, empty `ilex-boot`, and the retry's own read of it finds
        // nothing. Its stamp is made from this record as well.
        let earlier = root.join("earlier");
        ensure!(
            stamp::read_record(&earlier)?.is_empty(),
            "a record that is not there held digests"
        );
        stamp::keep_earlier(&earlier, &[])?;
        ensure!(
            !earlier.exists(),
            "no digest to keep wrote a record all the same"
        );
        stamp::keep_earlier(&earlier, &[first.clone(), second.clone()])?;
        ensure!(
            stamp::read_record(&earlier)? == [first.clone(), second.clone()]
                && fs::metadata(&earlier)?.permissions().mode() & 0o777 == 0o600
                && !root.join("earlier.partial").exists(),
            "the digests of the stamp that was wiped were not kept (root's, mode 600)"
        );
        // The retry read an empty stamp (the disk is new): nothing is lost.
        stamp::keep_earlier(&earlier, &[])?;
        ensure!(
            stamp::read_record(&earlier)? == [first.clone(), second.clone()],
            "an empty stamp blotted out the digests an earlier attempt kept"
        );
        // And a digest that comes again is the newest, not the oldest.
        stamp::keep_earlier(&earlier, &[first.clone(), third.clone()])?;
        ensure!(
            stamp::read_record(&earlier)? == [second.clone(), first.clone(), third.clone()],
            "the digests of a second wipe were not kept after the first's"
        );
        // The stamp the retry writes: the kept digests (the retry's own read of
        // the disk added none), what answers mode saw, then its own request.
        let kept = stamp::read_record(&earlier)?;
        let carried = stamp::union(&kept, &[]);
        let stamp_of_the_retry =
            stamp::merge(&carried, std::slice::from_ref(&"0".repeat(64)), &first);
        ensure!(
            stamp_of_the_retry == [second.clone(), third.clone(), "0".repeat(64), first.clone()],
            "the retry's stamp does not hold the old digests: {stamp_of_the_retry:?}"
        );
        fs::set_permissions(&earlier, fs::Permissions::from_mode(0o644))?;
        ensure!(
            stamp::read_record(&earlier).is_err(),
            "a record of kept digests that others can read was believed"
        );
        // One that cannot be read is replaced (it was not ours), never merged.
        stamp::keep_earlier(&earlier, std::slice::from_ref(&first))?;
        ensure!(
            stamp::read_record(&earlier)? == [first.clone()],
            "a record of kept digests that was not ours was not replaced"
        );
        fs::remove_file(&earlier)?;
        symlink("elsewhere", &earlier)?;
        ensure!(
            stamp::read_record(&earlier).is_err(),
            "a link was followed as the record of kept digests"
        );
        fs::remove_file(&earlier)?;
    }

    // What `Apply`'s answer means for the flag that keeps the interface out. A
    // structured refusal of the helper (its own error name, and the model's
    // document as the message) lowers it; every other way the call can end does
    // not, because none says the installation thread had not started.
    let refusal = serde_json::to_string(&ilex_installer_model::plan::PlanError::new(
        ilex_installer_model::plan::ErrorCode::DiskChanged,
        Some("disk"),
        "the disk is not the one that was planned",
    ))?;
    let refused = "org.ilex.Installer1.Error.Refused";
    ensure!(
        answers::structured_refusal(refused, &refusal).is_some()
            && answers::structured_refusal("org.ilex.Installer1.Error.Failed", &refusal).is_some(),
        "the helper's own refusals were not recognised"
    );
    for (what, name, message) in [
        (
            "a bus error with the helper's document",
            "org.freedesktop.DBus.Error.NoReply",
            refusal.as_str(),
        ),
        (
            "another service's error",
            "org.example.Error.Refused",
            refusal.as_str(),
        ),
        (
            "the helper's name with another message",
            refused,
            "Message recipient disconnected from message bus without replying",
        ),
        (
            "a name with no kind",
            "org.ilex.Installer1.Error.",
            refusal.as_str(),
        ),
        (
            "a name below another interface",
            "org.ilex.Installer1.Errors.Refused",
            refusal.as_str(),
        ),
        (
            "a name with more parts",
            "org.ilex.Installer1.Error.Refused.More",
            refusal.as_str(),
        ),
        ("an empty message", refused, ""),
        ("a document of another shape", refused, r#"{"code":"x"}"#),
    ] {
        ensure!(
            answers::structured_refusal(name, message).is_none(),
            "{what} was taken for a refusal of the helper"
        );
    }
    let method_error = |name: &str, text: &str| -> Result<zbus::Error> {
        let call = zbus::Message::method_call("/org/ilex/Installer1", "Apply")
            .build(&())
            .context("building a method call")?;
        let reply = zbus::Message::error(&call.header(), name)
            .build(&(text,))
            .context("building an error reply")?;
        Ok(zbus::Error::from(reply))
    };
    let flag = root.join("applying");
    let raise = || fs::write(&flag, b"");
    // The helper refused: nothing was written; the flag is lowered, the
    // interface may start, and the console says why.
    raise()?;
    let mut reached = answers::Reached::default();
    let said = answers::settle(Err(method_error(refused, &refusal)?), &flag, &mut reached)
        .err()
        .context("a refusal was taken for an answer")?;
    ensure!(
        !flag.exists() && !reached.applied && format!("{said:#}").contains("DiskChanged"),
        "a refusal of the helper kept the interface out: flag {}, applied {}, said {said:#}",
        flag.exists(),
        reached.applied
    );
    // Everything else: the flag stays, the run counts as applied, and the
    // failure is what the console shows.
    let no_reply = method_error(
        "org.freedesktop.DBus.Error.NoReply",
        "Message recipient disconnected from message bus without replying",
    )?;
    for (what, error) in [
        ("no reply from the bus", no_reply),
        (
            "a connection that ended",
            zbus::Error::InputOutput(std::sync::Arc::new(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "broken pipe",
            ))),
        ),
        (
            "a failure of the client",
            zbus::Error::Failure("anything".into()),
        ),
        (
            "a bus error with a refusal's document",
            method_error("org.freedesktop.DBus.Error.Failed", &refusal)?,
        ),
        (
            "the helper's name with a message that is no document",
            method_error(refused, "no")?,
        ),
    ] {
        raise()?;
        let mut reached = answers::Reached::default();
        let said = answers::settle(Err(error), &flag, &mut reached)
            .err()
            .with_context(|| format!("{what} was taken for an answer"))?;
        ensure!(
            flag.exists()
                && reached.applied
                && format!("{said:#}").contains("interface is not started"),
            "{what} lowered the flag, or did not count as applied: flag {}, applied {}, said {said:#}",
            flag.exists(),
            reached.applied
        );
    }
    raise()?;
    let mut reached = answers::Reached::default();
    ensure!(
        answers::settle(Ok("{}".into()), &flag, &mut reached)? == "{}"
            && flag.exists()
            && reached.applied,
        "an accepted Apply lowered the flag"
    );
    fs::remove_file(&flag)?;

    // `installation-active` is the helper's own refusal, and the one that says
    // something *is* being written: the unit restarted (`systemctl reset-failed`
    // and start, as its own comment documents) while the installation of an
    // earlier attempt still runs. It keeps the flag, hands nothing over and ends
    // the run as one that keeps the interface out.
    let active = serde_json::to_string(&ilex_installer_model::plan::PlanError::new(
        ilex_installer_model::plan::ErrorCode::InstallationActive,
        None,
        "an installation is already running",
    ))?;
    raise()?;
    let mut reached = answers::Reached::default();
    let said = answers::settle(Err(method_error(refused, &active)?), &flag, &mut reached)
        .err()
        .context("installation-active was taken for an answer")?;
    ensure!(
        flag.exists()
            && reached.applied
            && answers::keeps_interface_out(&said)
            && format!("{said:#}").contains("interface is not started")
            && format!("{said:#}").contains("InstallationActive"),
        "installation-active lowered the flag, or let the interface start: flag {}, applied {}, said {said:#}",
        flag.exists(),
        reached.applied
    );
    // In any call (Probe, Plan, SetSecret) it is the same: the run is not one
    // that hands over, and every other error is.
    for method in ["Probe", "Plan", "SetSecret", "State"] {
        let said = answers::classify(method, &method_error(refused, &active)?);
        ensure!(
            answers::keeps_interface_out(&said) && format!("{said:#}").starts_with(method),
            "{method}: installation-active did not keep the interface out: {said:#}"
        );
        let said = anyhow::anyhow!("{method}").context("the run");
        ensure!(
            !answers::keeps_interface_out(&said),
            "an error that is not installation-active kept the interface out"
        );
        let said = answers::classify(method, &method_error(refused, &refusal)?);
        ensure!(
            !answers::keeps_interface_out(&said)
                && !answers::keeps_interface_out(&answers::classify(
                    method,
                    &method_error(
                        "org.freedesktop.DBus.Error.NoReply",
                        "Message recipient disconnected from message bus without replying"
                    )?
                )),
            "{method}: a refusal that wrote nothing, or a bus error, kept the interface out"
        );
    }
    // The error stays one through the context the caller adds.
    ensure!(
        answers::keeps_interface_out(
            &answers::classify("Plan", &method_error(refused, &active)?).context("planning")
        ),
        "context hid that the interface is kept out"
    );
    fs::remove_file(&flag)?;
    // A flag left by an attempt of this boot is deleted at the start of a run
    // only when `State()` says no installation is active; when one is, or the
    // helper cannot say, it stays and the run keeps the interface out.
    let idle = ilex_installer_model::state::State::idle(0);
    let mut running = idle.clone();
    running.active = true;
    // An `Apply` of that attempt that is still checking has not begun, but
    // will: it counts.
    let mut checking = idle.clone();
    checking.starting = Some("c".repeat(32));
    raise()?;
    answers::settle_leftover(&flag, Ok(idle))?;
    ensure!(
        !flag.exists(),
        "a flag left over with no installation running was kept"
    );
    for (what, state) in [
        ("an installation that is running", Ok(running)),
        ("an Apply that is still checking", Ok(checking)),
        (
            "a helper that could not be asked",
            Err(anyhow::anyhow!("State: the bus has no reply")),
        ),
    ] {
        raise()?;
        let said = answers::settle_leftover(&flag, state)
            .err()
            .with_context(|| format!("{what} was taken for no installation"))?;
        ensure!(
            flag.exists() && answers::keeps_interface_out(&said),
            "{what} lowered the flag, or let the interface start: flag {}, said {said:#}",
            flag.exists()
        );
    }
    fs::remove_file(&flag)?;

    // The by-id name: a plain name of /dev/disk/by-id, never a path.
    for bad in ["", ".", "..", "../sda", "a/b", "/dev/sda", "bad\nname"] {
        ensure!(
            disks::by_id_node(bad).is_err(),
            "{bad:?} was taken for a name in /dev/disk/by-id"
        );
    }
    ensure!(
        disks::by_id_node("this-disk-does-not-exist-0000").is_err(),
        "a name that is not there was resolved"
    );
    // Never a name that describes contents, even when the link exists.
    for contents in [
        "lvm-pv-uuid-abc",
        "dm-name-root",
        "md-uuid-abc",
        "dm-uuid-LVM-x",
    ] {
        let refused = disks::by_id_node(contents)
            .err()
            .context("a name that describes the disk's contents was accepted")?;
        ensure!(
            refused.contains("hardware"),
            "the refusal for {contents} does not say why: {refused}"
        );
    }

    // The file: regular, not a link, bounded, and opened without waiting.
    write(&root.join("answers.toml"), "schema = 1\n")?;
    ensure!(
        answers::read_file(root)?.as_slice() == b"schema = 1\n",
        "the answers file was misread"
    );
    fs::remove_file(root.join("answers.toml"))?;
    write(&root.join("elsewhere"), "schema = 1\n")?;
    symlink("elsewhere", root.join("answers.toml"))?;
    ensure!(answers::read_file(root).is_err(), "a link was followed");
    fs::remove_file(root.join("answers.toml"))?;
    fs::write(root.join("answers.toml"), vec![b'#'; 70_000])?;
    ensure!(
        answers::read_file(root).is_err(),
        "an oversized answers file was read"
    );
    fs::remove_file(root.join("answers.toml"))?;
    fs::create_dir(root.join("answers.toml"))?;
    ensure!(
        answers::read_file(root).is_err(),
        "a directory was read as the answers file"
    );
    fs::remove_dir(root.join("answers.toml"))?;
    // A FIFO with no writer would block an ordinary open for ever.
    let fifo = std::ffi::CString::new(root.join("answers.toml").to_string_lossy().as_bytes())?;
    ensure!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) } == 0,
        "mkfifo: {}",
        std::io::Error::last_os_error()
    );
    let started = std::time::Instant::now();
    ensure!(
        answers::read_file(root).is_err() && started.elapsed() < Duration::from_secs(5),
        "a FIFO was read, or waited for"
    );
    Ok(())
}

/// The recovery key between the installation and the interface: handed over
/// once, confirmed by the whole key (every group compared, then the key's own
/// keyslot opened with it), waited for, and given up on.
fn check_recovery_holder() -> Result<()> {
    use crate::recovery::{Holder, Verifier, Wait};
    use ilex_installer_model::{
        plan::ErrorCode,
        recovery::{self, KeyStatus},
        selfcheck::TEST_KEY,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    let plan = "a".repeat(32);
    let holder = Holder::default();
    fn code<T>(result: Result<T, ilex_installer_model::plan::PlanError>) -> Option<ErrorCode> {
        result.err().map(|error| error.code)
    }
    let answer =
        |text: &str| -> Result<secret::Secret> { secret::Secret::from_bytes(text.as_bytes()) };
    // The keyslot check, as the installation gives it: it counts its calls and
    // can be made to fail, and it is given what was typed.
    let opened = Arc::new(AtomicUsize::new(0));
    let slot_refuses = Arc::new(AtomicBool::new(false));
    let verifier = || -> Verifier {
        let opened = Arc::clone(&opened);
        let slot_refuses = Arc::clone(&slot_refuses);
        Box::new(move |typed: &secret::Secret| {
            anyhow::ensure!(
                typed.expose() == TEST_KEY.as_bytes(),
                "the verifier was not given the canonical key"
            );
            opened.fetch_add(1, Ordering::SeqCst);
            anyhow::ensure!(
                !slot_refuses.load(Ordering::SeqCst),
                "the keyslot refused the key"
            );
            Ok(())
        })
    };
    ensure!(
        code(holder.take(&plan)) == Some(ErrorCode::NoRecoveryKey)
            && code(holder.acknowledge(&plan, &answer(TEST_KEY)?))
                == Some(ErrorCode::NoRecoveryKey)
            && code(holder.decline(&plan)) == Some(ErrorCode::NoRecoveryKey),
        "a key nobody made was handed over, confirmed or skipped"
    );
    holder.offer(
        &plan,
        secret::Secret::from_bytes(TEST_KEY.as_bytes())?,
        verifier(),
    );
    ensure!(
        holder.status() == Some(KeyStatus::Waiting)
            && code(holder.take(&"b".repeat(32))) == Some(ErrorCode::NoRecoveryKey)
            && code(holder.decline(&"b".repeat(32))) == Some(ErrorCode::NoRecoveryKey),
        "another plan's key was taken"
    );
    // Confirming a key that was not shown is refused: nobody could have read it.
    ensure!(
        code(holder.acknowledge(&plan, &answer(TEST_KEY)?)) == Some(ErrorCode::RecoveryKeyMismatch),
        "a key that was not shown was confirmed"
    );
    // Handed over once, as a sealed memfd with the key and nothing else.
    let fd = holder.take(&plan).map_err(|e| anyhow::anyhow!(e.message))?;
    let taken = secret::Secret::from_memfd(fd, 4096)?;
    ensure!(
        taken.expose() == TEST_KEY.as_bytes() && holder.status() == Some(KeyStatus::Shown),
        "the key that was handed over"
    );
    drop(taken);
    ensure!(
        code(holder.take(&plan)) == Some(ErrorCode::RecoveryKeyTaken),
        "the key was handed over twice"
    );
    // A key that differs in any one of its eight groups, or is not a whole key,
    // changes nothing; the person tries again. (Two right groups of eight are
    // not a key: phase 0 asks for all of it.)
    let mut wrong: Vec<String> = (0..recovery::GROUPS)
        .map(|group| {
            let mut typed: Vec<char> = TEST_KEY.chars().collect();
            let at = group * (recovery::GROUP_CHARS + 1) + 5;
            typed[at] = if typed[at] == 'c' { 'b' } else { 'c' };
            typed.into_iter().collect()
        })
        .collect();
    wrong.extend([
        "klnrtuvc ijklnrtu".to_owned(),
        TEST_KEY[..TEST_KEY.len() - 9].to_owned(),
        format!("{TEST_KEY}-cdefghij"),
        String::new(),
        "2 klnrtuvc\n6 ijklnrtu\n".to_owned(),
    ]);
    for typed in &wrong {
        ensure!(
            code(holder.acknowledge(&plan, &answer(typed)?))
                == Some(ErrorCode::RecoveryKeyMismatch)
                && holder.status() == Some(KeyStatus::Shown),
            "a wrong key {typed:?} was taken"
        );
    }
    ensure!(
        code(holder.acknowledge(&plan, &secret::Secret::from_bytes(b"\xff\xfe")?))
            == Some(ErrorCode::RecoveryKeyMismatch)
            && opened.load(Ordering::SeqCst) == 0,
        "text that is not text was taken, or a keyslot was opened for a wrong key"
    );
    // A right key that its keyslot refuses is not confirmed either (the volume's
    // fault, said as such), and the person may still try again or go on without.
    slot_refuses.store(true, Ordering::SeqCst);
    ensure!(
        code(holder.acknowledge(&plan, &answer(TEST_KEY)?)) == Some(ErrorCode::Internal)
            && holder.status() == Some(KeyStatus::Shown)
            && opened.load(Ordering::SeqCst) == 1,
        "a key its keyslot refused was confirmed"
    );
    slot_refuses.store(false, Ordering::SeqCst);
    // The installation's wait ends when the key is confirmed, and not before.
    let holder = Arc::new(holder);
    let waiting = std::thread::spawn({
        let holder = Arc::clone(&holder);
        move || holder.wait_acknowledged(&mut || {})
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    ensure!(
        !waiting.is_finished(),
        "the wait ended before the key was confirmed"
    );
    // The paper's hyphens, spaces and capitals are forgiven.
    holder
        .acknowledge(&plan, &answer(&TEST_KEY.to_uppercase().replace('-', " "))?)
        .map_err(|e| anyhow::anyhow!(e.message))?;
    ensure!(
        waiting
            .join()
            .map_err(|_| anyhow::anyhow!("the waiting thread panicked"))?
            == Wait::Acknowledged
            && holder.status() == Some(KeyStatus::Acknowledged)
            && opened.load(Ordering::SeqCst) == 2,
        "the wait did not end with the confirmation"
    );
    // Once confirmed the key stays: skipping it is refused.
    ensure!(
        code(holder.decline(&plan)) == Some(ErrorCode::RecoveryKeyConfirmed)
            && holder.status() == Some(KeyStatus::Acknowledged),
        "a confirmed key was given up"
    );
    // Cleared (the installation is over): nothing is left, and a wait ends.
    holder.clear();
    ensure!(
        holder.status().is_none() && holder.wait_acknowledged(&mut || {}) == Wait::Abandoned,
        "a key was left after the installation"
    );
    // Going on without the key: allowed before the key was taken (three failed
    // takes) and after it (a window that went away), wipes the key, ends the
    // wait as declined and is not undone by a later confirmation or take.
    for taken_first in [false, true] {
        holder.offer(
            &plan,
            secret::Secret::from_bytes(TEST_KEY.as_bytes())?,
            verifier(),
        );
        if taken_first {
            holder.take(&plan).map_err(|e| anyhow::anyhow!(e.message))?;
        }
        let waiting = std::thread::spawn({
            let holder = Arc::clone(&holder);
            move || holder.wait_acknowledged(&mut || {})
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        ensure!(
            !waiting.is_finished(),
            "the wait ended before the key was given up"
        );
        holder
            .decline(&plan)
            .map_err(|e| anyhow::anyhow!(e.message))?;
        ensure!(
            waiting
                .join()
                .map_err(|_| anyhow::anyhow!("the waiting thread panicked"))?
                == Wait::Declined
                && holder.status() == Some(KeyStatus::Declined)
                && holder.decline(&plan).is_ok()
                && code(holder.acknowledge(&plan, &answer(TEST_KEY)?))
                    == Some(ErrorCode::NoRecoveryKey)
                && code(holder.take(&plan)) == Some(ErrorCode::NoRecoveryKey)
                && holder.status() == Some(KeyStatus::Declined),
            "giving up the key (taken first: {taken_first})"
        );
        holder.clear();
    }
    Ok(())
}
