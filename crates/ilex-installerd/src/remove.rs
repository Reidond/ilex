//! Taking off a disk that is kept the partitions the installation's own run
//! made, when it fails: free-space mode's undo, and the only time it ever
//! removes a partition. Nothing that was on the disk before the run is removed
//! (and the plan refuses a disk that holds partitions of ilex's own).
//!
//! Only the named partitions, each checked against sysfs and the partition
//! UUIDs it was named by, and nothing but a partition table entry and the file
//! system signatures of those partitions is written.

use crate::{exec, target};
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

const TOOL: Duration = Duration::from_secs(120);

/// A partition to remove, as it was seen.
#[derive(Clone, Debug)]
pub struct Doomed {
    pub number: u32,
    pub uuid: String,
    pub label: String,
    pub size_bytes: u64,
}

/// The partition numbers the kernel lists for `disk` (a kernel name).
fn kernel_numbers(sys: &Path, disk: &str) -> Vec<u32> {
    target::kernel_partitions(sys, disk)
        .iter()
        .filter_map(|name| {
            fs::read_to_string(sys.join("block").join(disk).join(name).join("partition"))
                .ok()?
                .trim()
                .parse()
                .ok()
        })
        .collect()
}

/// Remove `doomed` from the disk addressed by `address` (its `/dev/disk/by-id`
/// name, which leads to the disk named `kname`). The disk must not be held:
/// `wipefs` and the kernel open the partitions. Each partition is found again
/// by its UUID and must be the planned number, size and disk; its signatures
/// are wiped, then the table entries are deleted with `sfdisk --delete`, and
/// the kernel must list none of them afterwards.
pub fn remove_partitions(
    address: &str,
    kname: &str,
    doomed: &[Doomed],
    log: &mut dyn FnMut(&str),
) -> Result<()> {
    ensure!(!doomed.is_empty(), "no partition to remove");
    let sys = Path::new("/sys");
    let by_partuuid = Path::new("/dev/disk/by-partuuid");
    let mut numbers = Vec::new();
    for d in doomed {
        let node =
            target::locate_partition(&d.uuid, d.number, d.size_bytes, kname, sys, by_partuuid)
                .with_context(|| format!("{} ({}) is not as it was seen", d.label, d.uuid))?;
        log(&format!(
            "removing {} ({}): partition {} of {kname}, {} bytes",
            d.label, node, d.number, d.size_bytes
        ));
        let out = exec::run("wipefs", &["--all", &node], TOOL)?;
        ensure!(
            out.success(),
            "wipefs {node}: {}",
            out.stderr.lines().last().unwrap_or("failed")
        );
        numbers.push(d.number.to_string());
    }
    let mut args = vec!["--delete", address];
    args.extend(numbers.iter().map(String::as_str));
    log(&format!("sfdisk {}", args.join(" ")));
    let out = exec::run("sfdisk", &args, TOOL)?;
    ensure!(
        out.success(),
        "sfdisk --delete: {}",
        out.stderr.lines().last().unwrap_or("failed")
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let left = kernel_numbers(sys, kname);
        if !doomed.iter().any(|d| left.contains(&d.number)) {
            break;
        }
        if Instant::now() >= deadline {
            bail!("the kernel still lists partitions {left:?} of {kname} after they were deleted");
        }
        thread::sleep(Duration::from_millis(200));
    }
    let _ = exec::run(
        "udevadm",
        &["settle", "--timeout=30"],
        Duration::from_secs(40),
    );
    Ok(())
}
