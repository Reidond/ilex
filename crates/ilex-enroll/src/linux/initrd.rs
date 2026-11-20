//! First try one validated TPM token via cryptsetup's token-only operation.
//! Success is direct evidence of the mapping's creation, not a journal guess or
//! a later unseal trial. For a qualified header we can also restrict the
//! stock password agent to keyboard credentials and record fallback.
use super::*;
use std::{os::unix::process::CommandExt, process::ExitStatus};

const ORIGINAL: &str = "/usr/libexec/ilex/systemd-cryptsetup-original";
fn record(uuid: &str, method: &str, token: Option<&str>, slot: Option<u32>) -> Result<()> {
    write_state(
        &Path::new("/run/ilex/unlock").join(format!("{uuid}.json")),
        &serde_json::json!({
            "schema":1, "uuid":uuid, "boot_id":fs::read_to_string("/proc/sys/kernel/random/boot_id")?.trim(),
            "method":method, "token":token, "slot":slot
        }),
    )
}

fn record_best_effort(uuid: &str, method: &str, token: Option<&str>, slot: Option<u32>) {
    if record(uuid, method, token, slot).is_err() {
        eprintln!("ilex: unlock evidence could not be written");
    }
}

pub fn attach(args: &[String]) -> Result<ExitStatus> {
    ensure!(
        Path::new("/etc/initrd-release").exists(),
        "unlock wrapper is initramfs-only"
    );
    if args.first().map(String::as_str) != Some("attach") || args.len() < 3 {
        return Err(Command::new(ORIGINAL).args(args).exec().into());
    }
    let name = &args[1];
    let path = Path::new(&args[2]);
    let uuid = run(Command::new("/usr/sbin/cryptsetup")
        .arg("luksUUID")
        .arg(path))
    .ok()
    .and_then(|o| String::from_utf8(o.stdout).ok())
    .map(|s| s.trim().to_string());
    let options = args.get(4).map_or("", String::as_str);
    let mut password_only = false;
    if let Some(uuid) = &uuid {
        // If the mapping predates this invocation its method is unknown.
        if !Path::new("/dev/mapper").join(name).exists()
            && args
                .get(3)
                .is_none_or(|s| s == "-" || s == "none" || s.is_empty())
            && let Ok(header) = json(
                Command::new("/usr/sbin/cryptsetup")
                    .args(["luksDump", "--dump-json-metadata"])
                    .arg(path),
            )
            && let Some(tokens) = header["tokens"].as_object()
            && options.split(',').all(|o| {
                matches!(
                    o,
                    "" | "luks" | "x-initrd.attach" | "tpm2-device=auto" | "discard"
                )
            })
        {
            for (id, token) in tokens {
                if validate_token(token).is_err() {
                    continue;
                }
                let mut command = Command::new("/usr/sbin/cryptsetup");
                command.args(["open", "--type", "luks2", "--token-only", "--token-id", id]);
                if options.split(',').any(|o| o == "discard") {
                    command.arg("--allow-discards");
                }
                if run(command.arg(path).arg(name)).is_ok() {
                    record_best_effort(uuid, "tpm2", Some(id), Some(super::enroll::slot(token)?));
                    return Command::new("/usr/bin/true")
                        .status()
                        .context("returning TPM success");
                }
            }
            // This is the installed Anaconda mapping, with no configured or
            // discovered keyfile/credential and no third-party token policy.
            // Retain stock systemd for all other cases, with unknown evidence.
            password_only = name == &format!("luks-{uuid}")
                && super::layout::verify_header_cipher(&header).is_ok()
                && tokens
                    .values()
                    .all(|t| t["type"] == "systemd-recovery" || validate_token(t).is_ok())
                && ["/etc/cryptsetup-keys.d", "/run/cryptsetup-keys.d"]
                    .iter()
                    .all(|dir| {
                        Path::new(dir)
                            .join(format!("{name}.key"))
                            .symlink_metadata()
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                    })
                && std::env::var_os("CREDENTIALS_DIRECTORY").is_none_or(|dir| {
                    Path::new(&dir)
                        .join("cryptsetup.passphrase")
                        .symlink_metadata()
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                });
        }
    }
    let mut command = Command::new(ORIGINAL);
    if password_only {
        // Keep Plymouth and the stock recovery-key handling, without
        // retrying TPM invisibly or passing the password through a cache.
        let options = options
            .split(',')
            .filter(|option| !option.is_empty() && *option != "tpm2-device=auto")
            .chain(["luks", "password-cache=no"])
            .collect::<Vec<_>>()
            .join(",");
        command
            .args(&args[..3])
            .args(["none", &options])
            .env("SYSTEMD_CRYPTSETUP_USE_TOKEN_MODULE", "0");
    } else {
        command.args(args);
    }
    let result = command.status()?;
    if result.success()
        && let Some(uuid) = uuid
    {
        let method = if password_only && Path::new("/dev/mapper").join(name).exists() {
            "fallback"
        } else {
            "unknown"
        };
        record_best_effort(&uuid, method, None, None);
    }
    Ok(result)
}

pub fn record_root(root: &Path) -> Result<()> {
    // /sysroot is the real filesystem before ostree switches to its overlay.
    // Separate /var might not yet be mounted; use only the root block ancestry.
    let nested = root.join("sysroot");
    let physical = if nested.is_dir() { &nested } else { root };
    let (_, uuids) = super::layout::ancestors(physical)?;
    if uuids.len() != 1 {
        return Ok(());
    }
    let uuid = uuids.iter().next().unwrap();
    let source = Path::new("/run/ilex/unlock").join(format!("{uuid}.json"));
    if !source.exists() {
        record(uuid, "unknown", None, None)?;
    }
    let value: Value = serde_json::from_slice(&fs::read(source)?)?;
    write_state(Path::new("/run/ilex/root-unlock.json"), &value)
}
