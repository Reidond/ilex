use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::os::{
    fd::AsRawFd,
    unix::fs::{FileTypeExt, MetadataExt},
};
use std::path::PathBuf;

#[derive(Serialize, Deserialize)]
pub struct Layout {
    pub uuid: String,
    pub device: std::path::PathBuf,
}

fn mounted_devices(path: &Path) -> Result<(String, Vec<PathBuf>)> {
    // Anaconda stacks bind mounts at the deployment root. findmnt --target
    // on util-linux 2.42 returns every matching mount, including hidden ones.
    // Ask the kernel which mount the opened directory actually belongs to;
    // neither mount-table order nor the anonymous Btrfs device number proves it.
    let file = fs::File::open(path).context("opening mount path")?;
    // SAFETY: statx is a plain C output structure; zero initializes every field.
    let mut mount_info: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor stays open; AT_EMPTY_PATH queries that descriptor,
    // and statx writes into a correctly sized, aligned output structure.
    ensure!(
        unsafe {
            libc::statx(
                file.as_raw_fd(),
                c"".as_ptr(),
                libc::AT_EMPTY_PATH,
                libc::STATX_MNT_ID,
                &mut mount_info,
            )
        } == 0
            && mount_info.stx_mask & libc::STATX_MNT_ID != 0,
        "cannot identify containing mount"
    );
    let mount = json(
        Command::new("/usr/bin/findmnt")
            .args(["--json", "--kernel=mountinfo", "--id"])
            .arg(mount_info.stx_mnt_id.to_string())
            .args(["--output", "MAJ:MIN,FSTYPE,ID"]),
    )?;
    let mounts = mount["filesystems"].as_array().context("mount not found")?;
    ensure!(
        mounts.len() == 1 && mounts[0]["id"].as_u64() == Some(mount_info.stx_mnt_id),
        "ambiguous mount"
    );
    let fstype = mounts[0]["fstype"].as_str().unwrap_or_default();
    if matches!(fstype, "overlay" | "composefs") {
        return Ok((fstype.into(), Vec::new()));
    }
    if fstype == "btrfs" {
        // Btrfs reports an anonymous mount device and only one mount source.
        // Use the opened filesystem's FSID to find all its block devices.
        let mut info = [0u64; 128];
        // BTRFS_IOC_FS_INFO: _IOR(0x94, 31, 1024-byte fs_info_args).
        // SAFETY: this is an aligned, zeroed 1024-byte kernel output buffer;
        // flags are zero. num_devices and the 16-byte FSID follow max_id.
        ensure!(
            unsafe {
                libc::ioctl(
                    file.as_raw_fd(),
                    0x8400941f as libc::c_ulong,
                    info.as_mut_ptr(),
                )
            } == 0,
            "cannot inspect Btrfs devices"
        );
        let mut fsid = String::with_capacity(36);
        use std::fmt::Write;
        for (index, byte) in info[2]
            .to_ne_bytes()
            .into_iter()
            .chain(info[3].to_ne_bytes())
            .enumerate()
        {
            if [4, 6, 8, 10].contains(&index) {
                fsid.push('-');
            }
            write!(fsid, "{byte:02x}")?;
        }
        let mut devices = BTreeSet::new();
        for member in fs::read_dir(Path::new("/sys/fs/btrfs").join(fsid).join("devices"))
            .context("cannot inspect Btrfs devices")?
        {
            let number = fs::read_to_string(member?.path().join("dev"))?;
            ensure!(
                devices.insert(block_path(number.trim())?),
                "duplicate Btrfs device"
            );
        }
        ensure!(
            !devices.is_empty() && devices.len() as u64 == info[1],
            "missing or unaccounted Btrfs devices"
        );
        return Ok((fstype.into(), devices.into_iter().collect()));
    }
    let number = mounts[0]["maj:min"]
        .as_str()
        .context("mount has no block device")?;
    Ok((fstype.into(), vec![block_path(number)?]))
}

fn block_path(number: &str) -> Result<PathBuf> {
    let (major, minor) = number.split_once(':').context("invalid mount device")?;
    ensure!(
        major.parse::<u32>().is_ok() && minor.parse::<u32>().is_ok(),
        "invalid mount device"
    );
    let path = Path::new("/dev/block").join(number);
    ensure!(
        fs::metadata(&path)?.file_type().is_block_device(),
        "mount device is not a block device"
    );
    Ok(path)
}

fn block_ancestry(device: &Path) -> Result<Value> {
    let blocks = json(
        Command::new("/usr/bin/lsblk")
            .args([
                "--json",
                "--inverse",
                "--paths",
                "--output",
                "NAME,TYPE,FSTYPE,UUID,MAJ:MIN",
            ])
            .arg(device),
    )?;
    let blocks = blocks["blockdevices"]
        .as_array()
        .context("missing block ancestors")?;
    ensure!(blocks.len() == 1, "ambiguous block ancestry");
    Ok(blocks[0].clone())
}

fn direct_block_device(node: &Value) -> bool {
    fn is_leaf(node: &Value) -> bool {
        matches!(node["type"].as_str(), Some("disk" | "loop"))
            && node["children"].as_array().is_none_or(Vec::is_empty)
    }
    is_leaf(node)
        || node["type"] == "part"
            && node["children"]
                .as_array()
                .is_some_and(|parents| parents.len() == 1 && is_leaf(&parents[0]))
}

const INSTALLER_CIPHER: &str = "aes-xts-plain64";

pub(super) fn verify_header_cipher(header: &Value) -> Result<u64> {
    let segments = header["segments"]
        .as_object()
        .context("missing LUKS2 segments")?;
    let segment = segments.get("0").context("missing LUKS2 data segment")?;
    ensure!(
        segments.len() == 1
            && segment["type"] == "crypt"
            && segment["encryption"] == INSTALLER_CIPHER
            && segment["size"] == "dynamic",
        "unsupported LUKS2 payload cipher or segment layout; expected aes-xts-plain64"
    );
    let slots = header["keyslots"]
        .as_object()
        .context("missing LUKS2 keyslots")?;
    ensure!(
        !slots.is_empty()
            && slots
                .values()
                .all(|slot| { slot["key_size"].as_u64().is_some_and(|size| size >= 32) }),
        "LUKS2 payload key must be at least 256 bits"
    );
    segment["offset"]
        .as_str()
        .context("missing LUKS2 data offset")?
        .parse()
        .context("invalid LUKS2 data offset")
}

fn verify_luks_member(device: &Path, mapping: &Value) -> Result<()> {
    let children = mapping["children"]
        .as_array()
        .context("backing device is not a direct dm-crypt LUKS2 mapping")?;
    ensure!(
        mapping["type"] == "crypt" && children.len() == 1 && children[0]["fstype"] == "crypto_LUKS",
        "backing device is not a direct dm-crypt LUKS2 mapping"
    );
    // Allow only dm-crypt directly over a disk, partition or loop device.
    // A regular partition's parent must itself be a disk or loop device.
    let backing_tree = &children[0];
    ensure!(
        direct_block_device(backing_tree),
        "unsupported block stack below the LUKS2 mapping"
    );
    let backing = Path::new(
        children[0]["name"]
            .as_str()
            .context("LUKS backing device missing")?,
    );
    let uuid = children[0]["uuid"]
        .as_str()
        .context("LUKS ancestor has no UUID")?;
    let by_uuid = Path::new("/dev/disk/by-uuid").join(uuid);
    ensure!(
        fs::metadata(&by_uuid)?.rdev() == fs::metadata(backing)?.rdev(),
        "LUKS UUID resolves to a different device"
    );

    let number = device
        .file_name()
        .and_then(|name| name.to_str())
        .context("crypt mapping number missing")?;
    let sysfs = Path::new("/sys/dev/block").join(number);
    let sectors: u64 = fs::read_to_string(sysfs.join("size"))?.trim().parse()?;
    let (major, minor) = number
        .split_once(':')
        .context("invalid crypt mapping number")?;
    let mut table =
        run(Command::new("/usr/sbin/dmsetup").args(["table", "--major", major, "--minor", minor]))?;
    // Query the entire active table without --target or --showkeys. Even
    // suppressed-key output can contain a kernel key reference.
    let active = std::str::from_utf8(&table.stdout).ok().and_then(|table| {
        let mut targets = table.lines();
        let target = targets.next()?;
        let mut fields = target.split_whitespace();
        let start = fields.next()?.parse::<u64>().ok()?;
        let length = fields.next()?.parse::<u64>().ok()?;
        if targets.next().is_some()
            || start != 0
            || sectors == 0
            || length != sectors
            || fields.next() != Some("crypt")
        {
            return None;
        }
        let cipher_allowed = fields.next()? == INSTALLER_CIPHER;
        // Skip the suppressed key and the IV offset, keeping only the data offset.
        if fields.nth(2) != backing_tree["maj:min"].as_str() {
            return None;
        }
        let offset = fields.next()?.parse::<u64>().ok()?.checked_mul(512)?;
        Some((cipher_allowed, offset))
    });
    table.stdout.zeroize();
    let (cipher_allowed, offset) =
        active.context("active mapping must have one crypt target covering the entire device")?;
    ensure!(
        cipher_allowed,
        "unsupported active payload cipher; expected aes-xts-plain64"
    );
    let sysfs = sysfs.join("dm");
    let dm_uuid = fs::read_to_string(sysfs.join("uuid"))?;
    ensure!(
        dm_uuid.starts_with(&format!("CRYPT-LUKS2-{}-", uuid.replace('-', ""))),
        "crypt mapping is not bound to its LUKS2 UUID"
    );
    let name = fs::read_to_string(sysfs.join("name"))?;
    let status = run(Command::new("/usr/sbin/cryptsetup")
        .arg("status")
        .arg(name.trim()))?;
    let status = String::from_utf8_lossy(&status.stdout);
    ensure!(
        status.lines().any(|line| line
            .trim()
            .strip_prefix("type:")
            .is_some_and(|kind| kind.trim() == "LUKS2")),
        "backing device is not an active dm-crypt LUKS2 mapping"
    );
    ensure!(
        status.lines().any(|line| line
            .trim()
            .strip_prefix("keysize:")
            .and_then(|size| size.trim().strip_suffix(" bits"))
            .and_then(|bits| bits.parse::<u32>().ok())
            .is_some_and(|bits| bits >= 256)),
        "active payload key must be at least 256 bits"
    );
    let header = json(
        Command::new("/usr/sbin/cryptsetup")
            .args(["luksDump", "--dump-json-metadata"])
            .arg(backing),
    )?;
    ensure!(
        verify_header_cipher(&header)? == offset,
        "active crypt data offset differs from the LUKS2 header"
    );
    Ok(())
}

fn root_devices(root: &Path) -> Result<(String, Vec<PathBuf>)> {
    let (fstype, devices) = mounted_devices(root)?;
    if matches!(fstype.as_str(), "overlay" | "composefs") {
        let (fstype, devices) = mounted_devices(&root.join("sysroot"))?;
        ensure!(
            !devices.is_empty(),
            "cannot identify the backing root filesystem"
        );
        Ok((fstype, devices))
    } else {
        Ok((fstype, devices))
    }
}

/// Identify a recovery device independently of TPM eligibility. Every path
/// from every root member must pass through the same verified LUKS2 mapping.
pub fn recovery_layout(root: &Path) -> Result<Layout> {
    fn crypt_roots(node: &Value, uuids: &mut BTreeSet<String>) -> Result<()> {
        if node["type"] == "crypt" {
            let mapping = block_path(node["maj:min"].as_str().context("missing mapping")?)?;
            verify_luks_member(&mapping, node)?;
            let uuid = node["children"][0]["uuid"]
                .as_str()
                .context("missing LUKS UUID")?;
            uuids.insert(uuid.to_owned());
        } else {
            let children = node["children"]
                .as_array()
                .filter(|nodes| !nodes.is_empty())
                .context("root has a path without verified LUKS2 encryption")?;
            for child in children {
                crypt_roots(child, uuids)?;
            }
        }
        Ok(())
    }
    let (_, devices) = root_devices(root)?;
    let mut uuids = BTreeSet::new();
    for device in devices {
        crypt_roots(&block_ancestry(&device)?, &mut uuids)?;
    }
    ensure!(uuids.len() == 1, "no unique verified recovery device");
    let uuid = uuids.into_iter().next().unwrap();
    Ok(Layout {
        device: Path::new("/dev/disk/by-uuid").join(&uuid),
        uuid,
    })
}

pub(super) fn ancestors(path: &Path) -> Result<(String, BTreeSet<String>)> {
    let (fstype, devices) = mounted_devices(path)?;
    fn collect(node: &Value, out: &mut BTreeSet<String>, dm: &mut bool) -> Result<()> {
        let number = node["maj:min"]
            .as_str()
            .context("missing block device number")?;
        *dm |= Path::new("/sys/dev/block")
            .join(number)
            .join("dm")
            .try_exists()?;
        if node["fstype"] == "crypto_LUKS" {
            out.insert(
                node["uuid"]
                    .as_str()
                    .context("LUKS ancestor has no UUID")?
                    .into(),
            );
        }
        if let Some(children) = node["children"].as_array() {
            for child in children {
                collect(child, out, dm)?;
            }
        }
        Ok(())
    }
    let mut uuids = BTreeSet::new();
    let mut device_mapper = false;
    let trees = devices
        .iter()
        .map(|device| block_ancestry(device))
        .collect::<Result<Vec<_>>>()?;
    for block in &trees {
        collect(block, &mut uuids, &mut device_mapper)?;
    }
    if device_mapper || !uuids.is_empty() {
        for (device, tree) in devices.iter().zip(&trees) {
            verify_luks_member(device, tree).context(
                "unsupported device-mapper layout. Use the existing disk passphrase or recovery key at the local console if available. Enrollment requires a single fully encrypted LUKS2 mapping directly over a disk or partition",
            )?;
        }
    } else {
        ensure!(
            trees.iter().all(direct_block_device),
            "unsupported plaintext layout. Root enrollment only supports a disk or partition with a known backing device"
        );
    }
    ensure!(
        fstype != "btrfs" || devices.len() == 1,
        "requires single-device Btrfs filesystem"
    );
    Ok((fstype, uuids))
}

/// Refuse layouts with plaintext /var, encrypted boot, stacked or ambiguous
/// LUKS ancestors, keyfiles or physical swap. Never guess a partition number.
pub fn discover(root: &Path) -> Result<Option<Layout>> {
    let (mut fstype, mut uuids) = ancestors(root)?;
    // A running OSTree deployment exposes an overlay at / and the backing
    // filesystem at /sysroot. Configuration and /var belong to the deployment.
    if fstype == "overlay" || fstype == "composefs" {
        (fstype, uuids) = ancestors(&root.join("sysroot"))?;
        ensure!(
            !matches!(fstype.as_str(), "overlay" | "composefs"),
            "cannot identify the backing root filesystem"
        );
    }
    if uuids.is_empty() {
        return Ok(None);
    }
    ensure!(
        fstype == "btrfs" && uuids.len() == 1,
        "requires one LUKS2 ancestor and btrfs root"
    );
    let (_, var) = ancestors(&root.join("var"))?;
    ensure!(var == uuids, "/var does not share encrypted root");
    for path in ["boot", "boot/efi"] {
        ensure!(
            ancestors(&root.join(path))?.1.is_empty(),
            "ESP and /boot must be outside LUKS"
        );
    }
    let swaps = fs::read_to_string("/proc/swaps")?;
    ensure!(
        swaps.lines().skip(1).all(|line| line
            .split_whitespace()
            .next()
            .is_some_and(|p| p.starts_with("/dev/zram"))),
        "disk swap is active"
    );
    let uuid = uuids.into_iter().next().unwrap();
    let crypttab = fs::read_to_string(root.join("etc/crypttab"))?;
    let matching: Vec<_> = crypttab
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|f| f.get(1) == Some(&format!("UUID={uuid}").as_str()))
        .collect();
    ensure!(
        matching.len() == 1 && matches!(matching[0].get(2), None | Some(&"none") | Some(&"-")),
        "root crypttab is ambiguous or contains a keyfile"
    );
    ensure!(
        !crypttab.contains("headless") && !crypttab.contains("swap"),
        "unsupported crypttab options"
    );
    let fstab = fs::read_to_string(root.join("etc/fstab"))?;
    ensure!(
        !fstab
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .any(|line| line.split_whitespace().nth(2) == Some("swap")),
        "disk swap configured in target fstab"
    );
    Ok(Some(Layout {
        device: Path::new("/dev/disk/by-uuid").join(&uuid),
        uuid,
    }))
}
