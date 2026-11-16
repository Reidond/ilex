//! The local creation boundary, shared by future setup/profile front ends.
//! homectl owns password prompting, collision checks, signing and LUKS creation.
//! This command never reads, stores or prints a password or exports a record.

use std::process::{Command, ExitCode};

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
    if name.len() > 31
        || !name.starts_with(|c: char| c.is_ascii_lowercase())
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
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
