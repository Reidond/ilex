//! Small facts about this system: memory, firmware, randomness, time.

use anyhow::{Context, Result};
use ilex_installer_model::probe::{Firmware, Memory};
use std::{fs, io::Read, path::Path};

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Time since boot on `CLOCK_BOOTTIME`: it keeps counting while the machine is
/// suspended (`CLOCK_MONOTONIC` and `Instant` do not), and neither a wall
/// clock set backwards nor an NTP step moves it. A plan's age is read from it.
pub fn boottime() -> std::time::Duration {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a timespec to fill; CLOCK_BOOTTIME is always present.
    let read = unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) };
    if read != 0 {
        // Cannot happen on Linux 2.6.39 or later; the monotonic clock then.
        return std::time::Duration::from_secs(0);
    }
    std::time::Duration::new(
        now.tv_sec.max(0) as u64,
        now.tv_nsec.clamp(0, 999_999_999) as u32,
    )
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut bytes)?;
    Ok(bytes)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random version-4 UUID: repart's `--seed`.
pub fn random_uuid() -> Result<String> {
    let mut b = random_bytes::<16>()?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// `MemTotal` and `MemAvailable` of `/proc/meminfo`, in bytes.
pub fn parse_meminfo(text: &str) -> Result<Memory> {
    let field = |name: &str| -> Result<u64> {
        let line = text
            .lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
            .with_context(|| format!("meminfo has no {name}"))?;
        let kib: u64 = line
            .split_whitespace()
            .next()
            .context("meminfo value")?
            .parse()
            .context("meminfo number")?;
        Ok(kib * 1024)
    };
    Ok(Memory {
        total_bytes: field("MemTotal")?,
        available_bytes: field("MemAvailable")?,
    })
}

pub fn memory() -> Result<Memory> {
    parse_meminfo(&fs::read_to_string("/proc/meminfo")?)
}

fn efi_flag(efi_dir: &Path, name: &str) -> Option<bool> {
    let data = fs::read(efi_dir.join(format!(
        "efivars/{name}-8be4df61-93ca-11d2-aa0d-00e098032b8c"
    )))
    .ok()?;
    (data.len() == 5 && data[4] <= 1).then(|| data[4] == 1)
}

/// The firmware this computer booted with, from `/sys/firmware/efi` (or the
/// directory standing in for it): that directory exists only after a UEFI boot.
pub fn firmware(efi_dir: &Path) -> Firmware {
    Firmware {
        uefi: efi_dir.is_dir(),
        secure_boot: efi_flag(efi_dir, "SecureBoot"),
        setup_mode: efi_flag(efi_dir, "SetupMode"),
    }
}
