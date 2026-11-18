use super::*;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::os::{fd::AsRawFd, unix::fs::FileTypeExt};

#[derive(Serialize, Deserialize)]
pub struct Layout {
    pub uuid: String,
    pub device: std::path::PathBuf,
}

pub(super) fn ancestors(path: &Path) -> Result<(String, BTreeSet<String>)> {
    let mount = json(
        Command::new("/usr/bin/findmnt")
            .args(["--json", "--evaluate", "--nofsroot", "--target"])
            .arg(path)
            .args(["--output", "MAJ:MIN,SOURCE,FSTYPE"]),
    )?;
    let mounts = mount["filesystems"].as_array().context("mount not found")?;
    ensure!(mounts.len() == 1, "ambiguous mount");
    let fstype = mounts[0]["fstype"].as_str().unwrap_or_default();
    if matches!(fstype, "overlay" | "composefs") {
        return Ok((fstype.into(), BTreeSet::new()));
    }
    let device = if fstype == "btrfs" {
        // Btrfs mount MAJ:MIN is an anonymous superblock number (e.g. 0:37),
        // not a block device. SOURCE without its subvolume suffix identifies
        // the backing device. Refuse multiple/missing devices rather than
        // examining just one member of an only partly encrypted filesystem.
        let file = fs::File::open(path)?;
        let mut info = [0u64; 128];
        // BTRFS_IOC_FS_INFO: _IOR(0x94, 31, 1024-byte fs_info_args).
        // SAFETY: this is an aligned, zeroed 1024-byte kernel output buffer;
        // flags are zero. num_devices is the second u64 in the stable UAPI.
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
        ensure!(info[1] == 1, "requires single-device Btrfs filesystem");
        let source = Path::new(
            mounts[0]["source"]
                .as_str()
                .context("Btrfs source missing")?,
        );
        ensure!(
            source.starts_with("/dev") && fs::metadata(source)?.file_type().is_block_device(),
            "Btrfs source is not a block device"
        );
        source.to_path_buf()
    } else {
        let number = mounts[0]["maj:min"]
            .as_str()
            .context("mount has no block device")?;
        ensure!(
            number.bytes().all(|b| b.is_ascii_digit() || b == b':'),
            "invalid mount device"
        );
        Path::new("/dev/block").join(number)
    };
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
    for block in blocks["blockdevices"]
        .as_array()
        .context("missing block ancestors")?
    {
        collect(block, &mut uuids)?;
    }
    Ok((
        mounts[0]["fstype"].as_str().unwrap_or_default().into(),
        uuids,
    ))
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
