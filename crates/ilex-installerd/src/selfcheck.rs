//! `ilex-installerd --self-check`: the helper's rules on fixtures, then this
//! system for real (its disks, its repart and the shipped definitions). Run in
//! the installer VM by the `installer::` tier.

use crate::{
    disks, exec, helper, registry, repart,
    scratch::Scratch,
    sys,
    topology::{self, FileId, LoopRow, Roots, System, Tracer},
};
use anyhow::{Context, Result, ensure};
use ilex_installer_model::probe::{Extent, Partition};
use std::{
    collections::BTreeSet,
    fs,
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
    let transports: Vec<Option<String>> = (0..9)
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
            ],
        "NVMe transports: {transports:?}"
    );
    ensure!(
        disks::nvme_transport(&sys, "nvme9n1").is_none(),
        "a namespace sysfs does not list"
    );
    // Only the local ones survive lsblk's list, though lsblk says `nvme` of all.
    let lsblk = format!(
        r#"{{"blockdevices":[{}]}}"#,
        (0..9)
            .map(|n| format!(
                r#"{{"name":"/dev/nvme{n}n1","kname":"/dev/nvme{n}n1","type":"disk","size":16000000000,"tran":"nvme","rm":false,"ro":false,"log-sec":512}}"#
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
    let raws = disks::parse_lsblk(&lsblk, &|_| None, &|name| disks::nvme_transport(&sys, name))?;
    let local: Vec<&str> = raws.iter().map(|r| r.kname.as_str()).collect();
    ensure!(
        local == ["nvme0n1", "nvme1n1", "nvme2n1", "nvme8n1"],
        "local NVMe disks: {local:?}"
    );
    Ok(())
}

// --- the live medium and what is in use -------------------------------------

/// A sysfs and `/dev` tree of the shape the kernel makes, in a directory: block
/// devices below `sys/devices/virtual/block`, `class/block` and `dev/block`
/// links to them, `slaves/` and `holders/` links, loop devices' backing files,
/// btrfs file systems' `devices/`.
struct Tree {
    root: PathBuf,
}

impl Tree {
    fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("sys/class/block"))?;
        fs::create_dir_all(root.join("sys/dev/block"))?;
        fs::create_dir_all(root.join("sys/devices/virtual/block"))?;
        fs::create_dir_all(root.join("sys/fs/btrfs"))?;
        fs::create_dir_all(root.join("dev/mapper"))?;
        fs::create_dir_all(root.join("dev/disk/by-label"))?;
        fs::create_dir_all(root.join("dev/disk/by-uuid"))?;
        Ok(Self { root })
    }

    fn roots(&self) -> Roots {
        Roots {
            sys: self.root.join("sys"),
            dev: self.root.join("dev"),
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
        fs::write(self.root.join("dev").join(name), b"")?;
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

    /// A device node link under `dev`, `target` relative to its directory.
    fn node_link(&self, path: &str, target: &str) -> Result<()> {
        symlink(target, self.root.join("dev").join(path))?;
        Ok(())
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
// devtmpfs, `/run` as tmpfs, the kernel's interfaces and a desktop session's
// FUSE services.
const KERNEL_MOUNTS: &str = "\
11 1 0:22 / /proc rw - proc proc rw\n\
12 1 0:23 / /sys rw - sysfs sysfs rw\n\
13 1 0:6 / /dev rw - devtmpfs devtmpfs rw,size=4096k,nr_inodes=1048576\n\
14 13 0:24 / /dev/pts rw - devpts devpts rw\n\
15 13 0:25 / /dev/shm rw - tmpfs tmpfs rw\n\
16 1 0:31 / /run rw - tmpfs tmpfs rw,mode=755\n\
17 12 0:26 / /sys/fs/cgroup rw - cgroup2 cgroup2 rw\n\
18 12 0:27 / /sys/kernel/security rw - securityfs securityfs rw\n\
19 12 0:29 / /sys/fs/fuse/connections rw - fusectl fusectl rw\n\
20 1 0:32 / /run/user/1000/doc rw - fuse.portal portal rw,user_id=1000\n\
21 1 0:33 / /run/user/1000/gvfs rw - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000\n\
22 1 0:34 / /proc/sys/fs/binfmt_misc rw - autofs systemd-1 rw\n\
23 1 0:35 / /sys/kernel/debug rw - debugfs debugfs rw\n\
24 1 0:36 / /dev/mqueue rw - mqueue mqueue rw\n";

/// dracut's dmsquash-live layout with the root image on device-mapper: the ISO
/// at `/run/initramfs/live` (on `sda1`), its squashfs on `loop0` at
/// `/run/rootfsbase`, the root `ext4` on `/dev/mapper/live-rw` (a snapshot of
/// `live-base`, which is the loop device `loop1` over `rootfs.img` inside the
/// squashfs, with its copy-on-write file `loop2` in RAM).
const ROOT_LIVE_RW: &str = "1 0 253:0 / / rw - ext4 /dev/mapper/live-rw rw\n";
const ROOT_OVERLAY: &str = "1 0 0:30 / / rw - overlay overlay rw,lowerdir=/run/rootfsbase\n";
const LIVE_MEDIUM: &str = "40 16 8:1 / /run/initramfs/live ro - iso9660 /dev/sda1 ro\n";
const ROOTFSBASE: &str = "41 16 7:0 / /run/rootfsbase ro - squashfs /dev/loop0 ro\n";

/// What a case traces: the base layout plus its own mounts, swap entries,
/// loop devices and files.
struct Case<'a> {
    label: &'a str,
    /// The mounts before the case's own: the root, the kernel's, the medium's.
    base: String,
    mounts: &'a str,
    swaps: &'a str,
    /// Loop devices after the live image's three (`loop0` to `loop2`).
    loops: Vec<Loop>,
    /// The three live-image loops' rows, replaced by these when given.
    live_rows: Option<Vec<Option<LoopRow>>>,
    /// What `stat` says of a path.
    files: Vec<(&'static str, FileId)>,
    /// The root is an overlay over the squashfs (only `loop0` is attached),
    /// not a device-mapper snapshot of `rootfs.img`.
    overlay_root: bool,
}

impl<'a> Case<'a> {
    fn new(label: &'a str) -> Self {
        Self {
            label,
            base: format!("{ROOT_LIVE_RW}{KERNEL_MOUNTS}{LIVE_MEDIUM}{ROOTFSBASE}"),
            mounts: "",
            swaps: "",
            loops: vec![],
            live_rows: None,
            files: vec![],
            overlay_root: false,
        }
    }

    /// dracut's other layout: an overlay root over `/run/rootfsbase`.
    fn overlay_root(mut self, medium: bool, rootfsbase: bool) -> Self {
        self.overlay_root = true;
        self.base = format!(
            "{ROOT_OVERLAY}{KERNEL_MOUNTS}{}{}",
            if medium { LIVE_MEDIUM } else { "" },
            if rootfsbase { ROOTFSBASE } else { "" }
        );
        self
    }

    fn base(mut self, base: String) -> Self {
        self.base = base;
        self
    }

    fn mounts(mut self, mounts: &'a str) -> Self {
        self.mounts = mounts;
        self
    }

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

    /// Build the case's own sysfs and device tree and trace it.
    fn trace(&self, parent: &Path) -> Result<topology::Usage> {
        let dir = TestDir::new(parent, &format!("topology-{}", self.label))?;
        let tree = Tree::new(dir.0.join("tree"))?;
        // sda: the live USB stick; the ISO file system is on sda1 and carries
        // the squashfs (loop0), whose rootfs.img (loop1) is live-base.
        tree.disk("sda", 8, 0)?;
        tree.partition("sda", "sda1", 8, 1)?;
        tree.partition("sda", "sda2", 8, 2)?;
        tree.loop_device("loop0", 7, 0, "/run/initramfs/live/LiveOS/squashfs.img")?;
        if !self.overlay_root {
            tree.loop_device("loop1", 7, 1, "/run/initramfs/squashfs/LiveOS/rootfs.img")?;
            tree.loop_device("loop2", 7, 2, "/run/initramfs/overlayfs/overlay-ilex")?;
            tree.stacked("dm-1", 253, 1, &["loop1"])?;
            tree.stacked("dm-0", 253, 0, &["dm-1", "loop2"])?;
            tree.node_link("mapper/live-base", "../dm-1")?;
            tree.node_link("mapper/live-rw", "../dm-0")?;
        }
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
        let mut rows: Vec<Option<LoopRow>> = self.live_rows.clone().unwrap_or_else(|| {
            let mut rows = vec![loop_row(
                "loop0",
                "/run/initramfs/live/LiveOS/squashfs.img",
                100,
                Some((8, 1)),
            )];
            if !self.overlay_root {
                rows.push(loop_row(
                    "loop1",
                    "/run/initramfs/squashfs/LiveOS/rootfs.img",
                    200,
                    Some((7, 0)),
                ));
                rows.push(loop_row(
                    "loop2",
                    "/run/initramfs/overlayfs/overlay-ilex",
                    300,
                    Some((0, 31)),
                ));
            }
            rows
        });
        for device in &self.loops {
            tree.loop_device(device.name, device.major, device.minor, device.sysfs)?;
            rows.push(device.row.clone());
        }
        let system = System {
            mounts: topology::parse_mountinfo(&format!("{}{}", self.base, self.mounts)),
            swaps: topology::parse_swaps(self.swaps),
            loops: rows.into_iter().flatten().collect(),
        };
        let roots = tree.roots();
        let stat = |path: &str| {
            self.files
                .iter()
                .find(|(p, _)| *p == path)
                .map(|(_, id)| *id)
        };
        Ok(Tracer::new(&roots, &system).with_stat(&stat).usage())
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

fn in_use_names(usage: &topology::Usage) -> Vec<&str> {
    usage.in_use.keys().map(String::as_str).collect()
}

/// The live medium and the disks in use, found by following every use of block
/// storage through the kernel's tree and never through the text of a name or a
/// path's prefix. Each case runs beside the mounts a live system has (the root,
/// `/dev` as devtmpfs, `/run` as tmpfs, the kernel's interfaces, dracut's live
/// layout): `/` contains every path and the detached or colliding name of a
/// file is a trap, so a case that works only without them proves nothing.
fn check_topology(parent: &Path) -> Result<()> {
    // The live layout alone: everything resolves, to the stick and nothing else.
    let live = Case::new("live").trace(parent)?;
    ensure!(
        names(&live.live) == ["dm-0", "dm-1", "loop0", "loop1", "loop2", "sda", "sda1"]
            && live.in_use.is_empty()
            && live.unresolved.is_empty(),
        "the dmsquash-live layout: {live:?}"
    );
    // The other layout: an overlay root over the squashfs (no device-mapper).
    let overlay = Case::new("overlay")
        .overlay_root(true, true)
        .trace(parent)?;
    ensure!(
        names(&overlay.live) == ["loop0", "sda", "sda1"]
            && overlay.in_use.is_empty()
            && overlay.unresolved.is_empty(),
        "an overlay root: {overlay:?}"
    );

    // --- A: the live medium's mount detached, its loop devices still bound.
    // dracut unmounts `/run/initramfs/squashfs` lazily, and so may anyone with
    // `umount -l /run/initramfs/live`: the kernel then prints `/LiveOS/…` for
    // the file, a name `/` contains. The stick is found by the device number
    // the kernel recorded, not by any name.
    let detached_medium = Case::new("lazy-live")
        .base(format!("{ROOT_LIVE_RW}{KERNEL_MOUNTS}{ROOTFSBASE}"))
        .trace(parent)?;
    ensure!(
        detached_medium.live.contains("sda")
            && detached_medium.live.contains("sda1")
            && detached_medium.unresolved.is_empty()
            && detached_medium.in_use.is_empty(),
        "a lazily unmounted /run/initramfs/live: {detached_medium:?}"
    );
    // ... and with every mount of the live medium gone but the root's chain.
    let only_root = Case::new("root-chain")
        .base(format!("{ROOT_LIVE_RW}{KERNEL_MOUNTS}"))
        .trace(parent)?;
    ensure!(
        only_root.live.contains("sda") && only_root.unresolved.is_empty(),
        "only the root's loop/dm chain is left: {only_root:?}"
    );
    // ... and with an overlay root, no medium mount and no squashfs mount: the
    // loop device is attached and nothing mounts it. The stick is in use (an
    // unmounted bound loop is a use of its file's disk).
    let bare_loops = Case::new("bare-loops")
        .overlay_root(false, false)
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
        .base(format!("{ROOT_LIVE_RW}{KERNEL_MOUNTS}{ROOTFSBASE}"))
        .live_rows(vec![
            Some(LoopRow {
                name: "/dev/loop0".into(),
                file: Some("/LiveOS/squashfs.img".into()),
                inode: Some(100),
                device: None,
            }),
            loop_row("loop1", "/x/rootfs.img", 200, Some((7, 0))),
            loop_row("loop2", "/x/overlay", 300, Some((0, 31))),
        ])
        .trace(parent)?;
    ensure!(
        !unknown.unresolved.is_empty() && !unknown.live.contains("sda"),
        "an unknown device is taken for resolved: {unknown:?}"
    );
    // A bound loop that losetup does not list at all.
    let unlisted = Case::new("lazy-unlisted")
        .base(format!("{ROOT_LIVE_RW}{KERNEL_MOUNTS}{ROOTFSBASE}"))
        .live_rows(vec![
            None,
            loop_row("loop1", "/x/rootfs.img", 200, Some((7, 0))),
            loop_row("loop2", "/x/overlay", 300, Some((0, 31))),
        ])
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
    let node = file_id((0, 6), 700, Some((8, 97)));
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
        names(&on_node.live) == ["dm-0", "dm-1", "loop0", "loop1", "loop2", "sda", "sda1"]
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
    for (what, id) in [
        ("another inode", Some(file_id((0, 6), 701, Some((8, 97))))),
        ("a regular file", Some(file_id((0, 6), 700, None))),
        ("gone", None),
    ] {
        let mut case = Case::new("node-lost");
        case = case.loop_device(loop_of(
            "loop3",
            3,
            "/dev/sdg1",
            loop_row("loop3", "/dev/sdg1", 700, Some((0, 6))),
        ));
        if let Some(id) = id {
            case = case.file("/dev/sdg1", id);
        }
        let lost = case.trace(parent)?;
        ensure!(
            !lost.unresolved.is_empty(),
            "a loop on /dev/sdg1 that is {what} was taken for resolved: {lost:?}"
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

    // --- D: a loop device's file is told by the kernel's device and inode,
    // not by the name. A name that now means another file does not matter when
    // the device number names a disk ...
    let collide = Case::new("collide")
        .loop_device(loop_of(
            "loop4",
            4,
            "/live.iso",
            loop_row("loop4", "/live.iso", 9, Some((8, 33))),
        ))
        // `/live.iso` is, today, another file on the root's file system.
        .file("/live.iso", file_id((253, 0), 1234, None))
        .trace(parent)?;
    ensure!(
        in_use_names(&collide).contains(&"sdc") && collide.unresolved.is_empty(),
        "a colliding name: {collide:?}"
    );
    // ... and a file that was deleted is still on its device.
    let deleted = Case::new("deleted")
        .loop_device(loop_of(
            "loop4",
            4,
            "/mnt/host/live.iso (deleted)",
            loop_row("loop4", "/mnt/host/live.iso (deleted)", 9, Some((8, 33))),
        ))
        .trace(parent)?;
    ensure!(
        in_use_names(&deleted).contains(&"sdc") && deleted.unresolved.is_empty(),
        "a deleted file: {deleted:?}"
    );
    // An anonymous number that no mount carries is a btrfs subvolume only if
    // the name is confirmed to be the file; a colliding name is not.
    let subvolume = |label: &str, id: Option<FileId>| -> Result<topology::Usage> {
        let mut case = Case::new(label)
            .mounts("50 1 0:70 / /mnt rw - btrfs /dev/sdj1 rw\n")
            .loop_device(loop_of(
                "loop4",
                4,
                "/mnt/sub/live.iso",
                loop_row("loop4", "/mnt/sub/live.iso", 9, Some((0, 99))),
            ));
        if let Some(id) = id {
            case = case.file("/mnt/sub/live.iso", id);
        }
        case.trace(parent)
    };
    let confirmed = subvolume("subvolume", Some(file_id((0, 99), 9, None)))?;
    ensure!(
        in_use_names(&confirmed).contains(&"sdj") && confirmed.unresolved.is_empty(),
        "a btrfs subvolume's file: {confirmed:?}"
    );
    for (what, id) in [
        ("another inode", Some(file_id((0, 99), 8, None))),
        ("another device", Some(file_id((0, 98), 9, None))),
        ("gone", None),
    ] {
        let lost = subvolume("subvolume-lost", id)?;
        ensure!(
            !lost.unresolved.is_empty(),
            "a subvolume file that is {what} was taken for resolved: {lost:?}"
        );
    }
    // A file in RAM holds no disk; a file on a remote file system neither; one
    // on an overlay has its layers' mounts, not a place of its own.
    for (what, device, mounts, resolved) in [
        ("tmpfs", (0, 31), "", true),
        (
            "nfs",
            (0, 90),
            "50 1 0:90 / /nfs rw - nfs4 server:/share rw\n",
            true,
        ),
        ("overlay", (0, 30), "", false),
    ] {
        let case = Case::new("ram")
            .overlay_root(true, true)
            .mounts(mounts)
            .loop_device(loop_of(
                "loop4",
                4,
                "/x/live.iso",
                loop_row("loop4", "/x/live.iso", 9, Some(device)),
            ))
            .trace(parent)?;
        ensure!(
            case.unresolved.is_empty() == resolved
                && in_use_names(&case).iter().all(|n| n.starts_with("loop")),
            "an ISO file on {what}: {case:?}"
        );
    }

    // --- E: a loop device nothing mounts is a use of its file's disk.
    let unmounted = Case::new("unmounted")
        .loop_device(loop_of(
            "loop5",
            5,
            "/mnt/x/live.iso",
            loop_row("loop5", "/mnt/x/live.iso", 11, Some((8, 161))),
        ))
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
        .mounts("50 1 7:6 / /mnt/inner ro - squashfs /dev/loop6 ro\n")
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
        .trace(parent)?;
    ensure!(
        isoscan.live.contains("sdc")
            && isoscan.live.contains("loop3")
            && isoscan.in_use.is_empty()
            && isoscan.unresolved.is_empty(),
        "iso-scan: {isoscan:?}"
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
        .swaps("Filename\tType\tSize\tUsed\tPriority\n/dev/sde2 partition 1024 0 -2\n")
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
        tracer.holders("sde3") == ["dm-5"] && tracer.holders("sde1").is_empty(),
        "holders"
    );
    // A swap file cannot be placed from what the kernel lists: no disk can be
    // shown not to hold it. Nor can a swap device that is not there.
    for (what, swaps) in [
        (
            "a swap file",
            "Filename\tType\tSize\tUsed\tPriority\n/mnt/data/swap\\040file file 1024 0 -3\n",
        ),
        (
            "a swap device that is not there",
            "Filename\tType\tSize\tUsed\tPriority\n/dev/nowhere partition 1024 0 -2\n",
        ),
    ] {
        let case = Case::new("swap").swaps(swaps).trace(parent)?;
        ensure!(
            !case.unresolved.is_empty(),
            "{what} is taken for no disk at all: {case:?}"
        );
    }
    // The tables are read or the probe fails: no mount table, or a swap list
    // that is not a file, is an error, never an empty system; no swap list at
    // all is a system with no swap.
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
    let (mounts, swaps) =
        topology::read_tables(&tables.0.join("mountinfo"), &tables.0.join("swaps"))?;
    ensure!(
        mounts.len() == 1 && swaps == ["/dev/sde2"],
        "tables: {mounts:?} {swaps:?}"
    );
    let (_, no_swaps) =
        topology::read_tables(&tables.0.join("mountinfo"), &tables.0.join("no-such-swaps"))?;
    ensure!(no_swaps.is_empty(), "swaps of a kernel without swap");
    ensure!(
        topology::read_tables(&tables.0.join("no-such-mountinfo"), &tables.0.join("swaps"))
            .is_err()
            && topology::read_tables(&tables.0.join("mountinfo"), &tables.0.join("swapdir"))
                .is_err(),
        "an unreadable table was taken for an empty one"
    );
    // mountinfo's escapes.
    let escaped =
        topology::parse_mountinfo("50 1 8:65 / /mnt/with\\040space rw - ext4 /dev/sde1 rw\n");
    ensure!(
        escaped.len() == 1 && escaped[0].mount_point == "/mnt/with space",
        "escapes: {escaped:?}"
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
    check_topology(parent.path()).context("the live medium and disks in use")?;
    check_files(parent.path()).context("files")?;
    let repart = check_repart(config, parent.path()).context("repart")?;
    let system = check_system(config).context("system")?;
    Ok(format!(
        "{model}; parsers, registry, subprocess limits, scratch, topology, files, {repart}; {system}"
    ))
}
