//! `ilex-installerd --self-check`: the helper's rules on fixtures, then this
//! system for real (its disks, its repart and the shipped definitions). Run in
//! the installer VM by the `installer::` tier.

use crate::{disks, helper, registry, repart, sys};
use anyhow::{Context, Result, ensure};
use ilex_installer_model::probe::{Extent, Partition};
use std::{collections::BTreeSet, fs, path::Path};

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
 {"name":"/dev/sdc","kname":"/dev/sdc","type":"disk","size":16000000000,"tran":"usb","rm":true,"ro":true,"log-sec":512}]}"#;

const SFDISK: &str = r#"{"partitiontable":{"label":"gpt","id":"X","device":"/dev/vda","unit":"sectors","firstlba":2048,"lastlba":83886046,"sectorsize":512,"partitions":[]}}"#;

const MOUNTINFO: &str = "\
24 1 0:21 / /run/initramfs/live ro,relatime shared:5 - iso9660 /dev/sdb1 ro,nojoliet\n\
25 1 0:22 / /sysroot ro - squashfs /dev/loop0 ro\n\
26 1 0:23 / /run/media/x rw - iso9660 /dev/sr0 ro\n\
27 1 0:24 / /boot rw - vfat /dev/vda1 rw\n";

const REPART: &str = r#"[
 {"type":"esp","label":"ilex-esp","uuid":"e2857866-e04d-4eb3-915d-101cbf838697","partno":0,"file":"/x/10-esp.conf","node":"/dev/vdb1","offset":1048576,"old_size":0,"raw_size":536870912,"old_padding":0,"raw_padding":0,"activity":"create"},
 {"type":"root-x86-64","label":"ilex-root","uuid":"644b2311-535f-46d4-adab-8cff81f14455","partno":1,"file":"/x/30-root.conf","node":"/dev/vdb2","offset":537919488,"old_size":0,"raw_size":41337991168,"old_padding":0,"raw_padding":0,"activity":"create"}]"#;

const IMAGE: &str = r#"{"schemaVersion":2,"config":{"digest":"sha256:aa","size":100},"layers":[{"size":1000},{"size":2000}],"annotations":{"org.opencontainers.image.version":"45.1"}}"#;
const INDEX: &str = r#"{"schemaVersion":2,"manifests":[{"digest":"sha256:arm","platform":{"os":"linux","architecture":"arm64"}},{"digest":"sha256:amd","platform":{"os":"linux","architecture":"amd64"}}]}"#;

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
    let raws = disks::parse_lsblk(LSBLK, &|name| match name {
        "vda1" => Some(2048),
        "vda2" => Some(2048 + 1_048_576),
        _ => None,
    })?;
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

    // The live medium, from mountinfo.
    let parent = |name: &str| (name == "sdb1").then(|| "sdb".to_owned());
    let live = disks::live_disks(MOUNTINFO, &parent);
    ensure!(
        live == BTreeSet::from(["sdb".to_owned(), "sr0".to_owned()]),
        "live disks: {live:?}"
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
    match registry::parse_manifest(IMAGE.as_bytes())? {
        registry::Parsed::Image(m) => ensure!(
            m.download_bytes == 3100
                && m.digest.starts_with("sha256:")
                && m.annotations_version.as_deref() == Some("45.1"),
            "image manifest"
        ),
        registry::Parsed::Index { .. } => anyhow::bail!("an image parsed as an index"),
    }
    match registry::parse_manifest(INDEX.as_bytes())? {
        registry::Parsed::Index { amd64, .. } => ensure!(amd64 == "sha256:amd", "index child"),
        registry::Parsed::Image(_) => anyhow::bail!("an index parsed as an image"),
    }
    let memory = sys::parse_meminfo(
        "MemTotal:        8000000 kB\nMemFree: 1 kB\nMemAvailable:    6000000 kB\n",
    )?;
    ensure!(
        memory.total_bytes == 8_192_000_000 && memory.available_bytes == 6_144_000_000,
        "meminfo"
    );
    Ok(())
}

/// A scratch directory under `parent`, removed on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(parent: &Path, name: &str) -> Result<Self> {
        let path = parent.join(format!("{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn check_files(parent: &Path) -> Result<()> {
    let scratch = Scratch::new(parent, "files")?;
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
    // Zone and keymap names stay inside their trees.
    fs::create_dir_all(root.join("zoneinfo/Europe"))?;
    fs::write(root.join("zoneinfo/Europe/Berlin"), b"TZif")?;
    fs::write(root.join("outside"), b"x")?;
    std::os::unix::fs::symlink("../../outside", root.join("zoneinfo/Europe/Escape"))?;
    let zones = root.join("zoneinfo");
    ensure!(
        helper::time_zone_exists(&zones, "Europe/Berlin")
            && !helper::time_zone_exists(&zones, "Europe/Escape")
            && !helper::time_zone_exists(&zones, "Europe")
            && !helper::time_zone_exists(&zones, "Nowhere/Land"),
        "zone names"
    );
    fs::create_dir_all(root.join("keymaps/i386/qwertz"))?;
    fs::write(root.join("keymaps/i386/qwertz/de-latin1.map.gz"), b"")?;
    ensure!(
        helper::keymap_exists(&root.join("keymaps"), "de-latin1")
            && !helper::keymap_exists(&root.join("keymaps"), "de"),
        "keymap names"
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

/// The sizes the model plans with are the sizes the shipped files ask for.
fn check_definition_sizes(definitions: &Path) -> Result<()> {
    use ilex_installer_model::plan::{BOOT_BYTES, ESP_BYTES, MIN_ROOT_BYTES};
    let size = |file: &str, key: &str| -> Result<u64> {
        let text = fs::read_to_string(definitions.join("whole-disk").join(file))?;
        text.lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .and_then(|v| parse_size(v.trim()))
            .with_context(|| format!("{file} has no {key}"))
    };
    ensure!(
        size("10-esp.conf", "SizeMinBytes")? == ESP_BYTES
            && size("10-esp.conf", "SizeMaxBytes")? == ESP_BYTES,
        "ESP size"
    );
    ensure!(
        size("20-boot.conf", "SizeMinBytes")? == BOOT_BYTES
            && size("20-boot.conf", "SizeMaxBytes")? == BOOT_BYTES,
        "boot size"
    );
    for file in ["30-root.conf", "30-root-encrypted.conf"] {
        ensure!(
            size(file, "SizeMinBytes")? == MIN_ROOT_BYTES,
            "{file}: root minimum"
        );
    }
    Ok(())
}

/// The shipped definitions and this system's `systemd-repart`, on a sparse file.
fn check_repart(config: &helper::Config, parent: &Path) -> Result<String> {
    check_definition_sizes(&config.definitions)?;
    let scratch = Scratch::new(parent, "repart")?;
    let image = scratch.0.join("disk.img");
    fs::File::create(&image)?.set_len(40 * 1024 * 1024 * 1024)?;
    let device = image.to_string_lossy().into_owned();
    let mut sizes = Vec::new();
    for encrypt in [false, true] {
        let layout = repart::assemble(
            &config.definitions,
            encrypt,
            &scratch.0.join(encrypt.to_string()),
        )?;
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
    let probe = helper.probe()?;
    ensure!(
        probe.schema == 1 && probe.catalogue.default == helper.table.default,
        "probe header"
    );
    ensure!(
        probe.memory.total_bytes > 0 && probe.memory.available_bytes <= probe.memory.total_bytes,
        "memory"
    );
    ensure!(
        probe
            .catalogue
            .compositions
            .iter()
            .all(|c| c.shells.first() == Some(&probe.edition.id)),
        "an offered composition is not this edition's"
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
    let parent = config.run_dir.join("selfcheck");
    fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;
    check_files(&parent).context("files")?;
    let repart = check_repart(config, &parent).context("repart")?;
    let system = check_system(config).context("system")?;
    let _ = fs::remove_dir(&parent);
    Ok(format!("{model}; parsers, files, {repart}; {system}"))
}
