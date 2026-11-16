//! The local creation boundary, shared by future setup/profile front ends.
//! homectl owns password prompting, collision checks, signing and LUKS creation.
//! This command never reads, stores or prints a password or exports a record.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde_json::Value;

/// user@ starts only after pam_systemd_home has acquired the home. Initialize
/// its filesystem labels here, before the user manager or login shell can
/// create files. Classic accounts do not depend on homed for recovery.
pub fn label(args: Vec<String>) -> ExitCode {
    match label_inner(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("ilex-agent: home-label: {error}");
            ExitCode::FAILURE
        }
    }
}

fn label_inner(args: Vec<String>) -> Result<(), String> {
    if args.len() != 1 || !args[0].bytes().all(|b| b.is_ascii_digit()) {
        return Err("expected one numeric user@ UID".into());
    }
    let uid: u32 = args[0].parse().map_err(|_| "invalid UID")?;
    if uid < 1000 {
        return Ok(());
    }
    let local = Command::new("/usr/bin/getent")
        .args(["-s", "files", "passwd", &args[0]])
        .output()
        .map_err(|e| e.to_string())?;
    if local.status.success() {
        return Ok(());
    }
    let entry = Command::new("/usr/bin/getent")
        .args(["passwd", &args[0]])
        .output()
        .map_err(|e| e.to_string())?;
    if !entry.status.success() {
        return Err("UID does not resolve through NSS".into());
    }
    let text = String::from_utf8(entry.stdout).map_err(|e| e.to_string())?;
    let fields: Vec<_> = text.trim().split(':').collect();
    if fields.len() != 7 || fields[2].parse::<u32>().ok() != Some(uid) {
        return Err("invalid NSS passwd entry".into());
    }
    let name = fields[0];
    let record = Command::new("/usr/bin/homectl")
        .args(["inspect", name, "--json=short"])
        .output()
        .map_err(|e| e.to_string())?;
    if !record.status.success() {
        // Dynamic/system or other NSS accounts are not homed filesystems.
        return Ok(());
    }
    let record: Value = serde_json::from_slice(&record.stdout).map_err(|e| e.to_string())?;
    if !valid_name(name) || record["userName"].as_str() != Some(name) {
        return Err("unexpected homed user name".into());
    }
    let home = format!("/home/{name}");
    if record["homeDirectory"].as_str() != Some(&home) {
        return Err("homed home is outside the supported /home namespace".into());
    }
    let mounted = Command::new("/usr/bin/mountpoint")
        .args(["-q", "--", &home])
        .status()
        .map_err(|e| e.to_string())?;
    if !mounted.success() {
        return Err("homed home must be active before starting user@".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fs::metadata(&home).map_err(|e| e.to_string())?.uid() != uid {
            return Err("home owner does not match user@ UID".into());
        }
    }
    let uuid = Command::new("/usr/bin/findmnt")
        .args(["-nro", "UUID", "--target", &home])
        .output()
        .map_err(|e| e.to_string())?;
    let uuid = String::from_utf8(uuid.stdout).map_err(|e| e.to_string())?;
    let uuid = uuid.trim();
    if uuid.len() != 36 || !uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err("home has no valid filesystem UUID".into());
    }
    let state = Path::new("/var/lib/ilex/homed-labels");
    let marker = state.join(uuid);
    if marker.is_file() {
        return Ok(());
    }
    // No -F: preserve security user/role/range. No cross-filesystem walk or
    // symlink following; the mount is homed's fixed namespace, not profile data.
    let result = Command::new("/usr/sbin/restorecon")
        .args(["-R", "-x", "--", &home])
        .status()
        .map_err(|e| e.to_string())?;
    if !result.success() {
        return Err("initial home SELinux labeling failed".into());
    }
    fs::create_dir_all(state).map_err(|e| e.to_string())?;
    fs::write(marker, b"1\n").map_err(|e| e.to_string())?;
    Ok(())
}

fn valid_name(name: &str) -> bool {
    name.len() <= 31
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

pub fn create(args: Vec<String>) -> ExitCode {
    match create_inner(args) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ilex-agent: home-create: {error}");
            ExitCode::from(2)
        }
    }
}

fn create_inner(args: Vec<String>) -> Result<u8, String> {
    const USAGE: &str = "home-create NAME --disk-size=GiB [--admin] [--real-name=NAME]\n\
        GiB is an integer of at least 2 (for example --disk-size=20).\n\
        Run locally with sudo; homectl prompts twice for the new home password.\n\
        Keep the separately named local maintenance administrator.";
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return Ok(0);
    }
    let name = args.first().ok_or(USAGE)?;
    if !valid_name(name) {
        return Err("NAME must be a lowercase Unix username (at most 31 bytes)".into());
    }
    let (mut size, mut real_name, mut admin) = (None, None, false);
    for arg in &args[1..] {
        if let Some(value) = arg.strip_prefix("--disk-size=") {
            let gib: u64 = value.parse().map_err(|_| USAGE)?;
            if size.is_some() || !(2..=1_048_576).contains(&gib) {
                return Err("specify one disk size between 2 and 1048576 GiB".into());
            }
            size = Some(gib);
        } else if let Some(value) = arg.strip_prefix("--real-name=") {
            if real_name.is_some()
                || value.is_empty()
                || value.len() > 256
                || value.chars().any(char::is_control)
            {
                return Err("specify one nonempty display name without control characters".into());
            }
            real_name = Some(value);
        } else if arg == "--admin" && !admin {
            admin = true;
        } else {
            return Err(USAGE.into());
        }
    }
    let size = size.ok_or(USAGE)?;
    // Admission check, not a quota/reservation: keep at least 2 GiB on the
    // backing filesystem after the requested allocation. Homed still handles
    // races/ENOSPC atomically; administrators must budget simultaneous homes.
    let free = Command::new("/usr/bin/df")
        .args(["--block-size=1", "--output=avail", "/var/home"])
        .output()
        .map_err(|e| format!("checking home storage: {e}"))?;
    let available = String::from_utf8_lossy(&free.stdout)
        .lines()
        .nth(1)
        .and_then(|n| n.trim().parse::<u64>().ok())
        .filter(|_| free.status.success())
        .ok_or("cannot determine available home storage")?;
    if available < (size + 2) * 1024 * 1024 * 1024 {
        return Err("insufficient space for the home and a 2 GiB system reserve".into());
    }
    // No UID, record import, alternate path or arbitrary homectl arguments.
    // homed atomically rejects local/NSS username and UID collisions itself.
    let mut command = Command::new("/usr/bin/homectl");
    command.args([
        "create",
        name,
        "--storage=luks",
        "--fs-type=btrfs",
        "--auto-resize-mode=off",
        "--rebalance-weight=off",
        "--luks-discard=no",
        "--luks-offline-discard=no",
        // login's SELinux PAM hook reads the new filesystem's root before
        // user@ can initialize per-path labels. Use the ordinary home-dir
        // type for initially unlabeled inodes, not a fixed mount context.
        "--luks-extra-mount-options=defcontext=system_u:object_r:user_home_dir_t:s0",
        "--auto-login=no",
        "--stop-delay=0",
    ]);
    command
        .arg(format!("--disk-size={size}G"))
        .arg(format!("--image-path=/var/home/{name}.home"))
        .arg(format!("--home-dir=/home/{name}"))
        .arg(if admin {
            "--member-of=ilex-homed,wheel"
        } else {
            "--member-of=ilex-homed"
        });
    if let Some(real_name) = real_name {
        command.arg(format!("--real-name={real_name}"));
    }
    // Ignore homectl's test/automation password shortcuts: credentials come
    // from its private local prompt, not inherited environment or argv.
    command.env_remove("PASSWORD").env_remove("NEWPASSWORD");
    let status = command
        .status()
        .map_err(|e| format!("starting homectl: {e}"))?;
    Ok(status
        .code()
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(1))
}
