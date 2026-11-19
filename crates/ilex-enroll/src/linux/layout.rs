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
                "NAME,TYPE,FSTYPE,UUID",
            ])
            .arg(device),
    )?;
    let blocks = blocks["blockdevices"]
        .as_array()
        .context("missing block ancestors")?;
    ensure!(blocks.len() == 1, "ambiguous block ancestry");
    Ok(blocks[0].clone())
}

fn verify_luks_member(device: &Path, mapping: &Value) -> Result<()> {
    let children = mapping["children"]
        .as_array()
        .context("backing device is not a direct dm-crypt LUKS2 mapping")?;
    ensure!(
        mapping["type"] == "crypt" && children.len() == 1 && children[0]["fstype"] == "crypto_LUKS",
        "backing device is not a direct dm-crypt LUKS2 mapping"
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

    let number = device.file_name().context("crypt mapping number missing")?;
    let sysfs = Path::new("/sys/dev/block").join(number).join("dm");
    let dm_uuid = fs::read_to_string(sysfs.join("uuid"))?;
    ensure!(
        dm_uuid.starts_with(&format!("CRYPT-LUKS2-{}-", uuid.replace('-', ""))),
        "crypt mapping is not bound to its LUKS2 UUID"
    );
    let name = fs::read_to_string(sysfs.join("name"))?;
    let status = run(Command::new("/usr/sbin/cryptsetup")
        .arg("status")
        .arg(name.trim()))?;
    ensure!(
        String::from_utf8_lossy(&status.stdout)
            .lines()
            .any(|line| line
                .trim()
                .strip_prefix("type:")
                .is_some_and(|kind| kind.trim() == "LUKS2")),
        "backing device is not an active dm-crypt LUKS2 mapping"
    );
    run(Command::new("/usr/sbin/cryptsetup")
        .args(["isLuks", "--type", "luks2"])
        .arg(backing))?;
    Ok(())
}

pub(super) fn ancestors(path: &Path) -> Result<(String, BTreeSet<String>)> {
    let (fstype, devices) = mounted_devices(path)?;
    fn collect(node: &Value, out: &mut BTreeSet<String>) -> Result<()> {
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
                collect(child, out)?;
            }
        }
        Ok(())
    }
    let mut uuids = BTreeSet::new();
    let trees = devices
        .iter()
        .map(|device| block_ancestry(device))
        .collect::<Result<Vec<_>>>()?;
    for block in &trees {
        collect(block, &mut uuids)?;
    }
    if !uuids.is_empty() {
        for (device, tree) in devices.iter().zip(&trees) {
            verify_luks_member(device, tree)?;
        }
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
