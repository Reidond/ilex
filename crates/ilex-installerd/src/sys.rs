//! Small facts about this system: memory, firmware, randomness, time.

use anyhow::{Context, Result};
use ilex_installer_model::probe::{Firmware, Memory};
use std::{fs, io::Read, path::Path};

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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

fn efi_flag(name: &str) -> Option<bool> {
    let data = fs::read(format!(
        "/sys/firmware/efi/efivars/{name}-8be4df61-93ca-11d2-aa0d-00e098032b8c"
    ))
    .ok()?;
    (data.len() == 5 && data[4] <= 1).then(|| data[4] == 1)
}

pub fn firmware() -> Firmware {
    Firmware {
        uefi: Path::new("/sys/firmware/efi").is_dir(),
        secure_boot: efi_flag("SecureBoot"),
        setup_mode: efi_flag("SetupMode"),
    }
}
