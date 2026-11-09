//! Machine-wide cap on concurrently running VMs.
//!
//! Several Claude sessions running e2e at once can exhaust RAM, so every VM
//! boot first takes one of N slots: lock files at
//! `$XDG_RUNTIME_DIR/ilex-vm/slot-<i>.lock`. The lock is an advisory file
//! lock held for as long as the returned [`VmSlot`] lives, so a crashed
//! process frees its slot automatically. N comes from `ILEX_VM_SLOTS`, or
//! one slot per 8 GiB of RAM (1..=4).

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::workspace::vm_runtime_root;

/// A held VM slot; dropping it frees the slot.
#[derive(Debug)]
pub struct VmSlot {
    _file: File,
    pub index: usize,
    pub path: PathBuf,
}

/// Number of machine-wide VM slots.
pub fn max_slots() -> usize {
    if let Some(n) = std::env::var("ILEX_VM_SLOTS").ok().and_then(|v| v.parse().ok()) {
        return std::cmp::max(1, n);
    }
    let gib = mem_total_gib().unwrap_or(16);
    (gib / 8).clamp(1, 4) as usize
}

/// Block until a slot is free, then hold it. `owner` (usually the VM name)
/// is written into the lock file so `cat slot-*.lock` shows who holds what.
pub fn acquire(owner: &str) -> Result<VmSlot> {
    let dir = vm_runtime_root();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let n = max_slots();
    let mut announced = false;
    loop {
        for index in 0..n {
            if let Some(slot) = try_slot(&dir, index, owner)? {
                if announced {
                    eprintln!("ilex: got VM slot {index}");
                }
                return Ok(slot);
            }
        }
        if !announced {
            eprintln!("ilex: all {n} VM slots are busy (see {}), waiting...", dir.display());
            announced = true;
        }
        thread::sleep(Duration::from_secs(2));
    }
}

/// Take a free slot without waiting, if there is one.
pub fn try_acquire(owner: &str) -> Result<Option<VmSlot>> {
    let dir = vm_runtime_root();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    for index in 0..max_slots() {
        if let Some(slot) = try_slot(&dir, index, owner)? {
            return Ok(Some(slot));
        }
    }
    Ok(None)
}

fn try_slot(dir: &std::path::Path, index: usize, owner: &str) -> Result<Option<VmSlot>> {
    let path = dir.join(format!("slot-{index}.lock"));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => {
            file.set_len(0)?;
            writeln!(file, "{owner} pid={}", std::process::id())?;
            Ok(Some(VmSlot { _file: file, index, path }))
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

fn mem_total_gib() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024 / 1024)
}
