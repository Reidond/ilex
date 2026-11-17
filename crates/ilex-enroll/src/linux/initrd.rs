//! First try one validated TPM token via cryptsetup's token-only operation.
//! Success is direct evidence of the mapping's creation, not a journal guess or
//! a later unseal trial. Any other successful path is conservatively unknown.
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
        {
            for (id, token) in tokens {
                if validate_token(token).is_err() {
                    continue;
                }
                let options = args.get(4).map_or("", String::as_str);
                // Do not silently discard crypttab semantics. Anaconda's root
                // options are the only qualified set for the fast path.
                if !options.split(',').all(|o| {
                    matches!(
                        o,
                        "" | "luks" | "x-initrd.attach" | "tpm2-device=auto" | "discard"
                    )
                }) {
                    break;
                }
                let mut command = Command::new("/usr/sbin/cryptsetup");
                command.args(["open", "--type", "luks2", "--token-only", "--token-id", id]);
                if options.split(',').any(|o| o == "discard") {
                    command.arg("--allow-discards");
                }
                if run(command.arg(path).arg(name)).is_ok() {
                    record(uuid, "tpm2", Some(id), Some(super::enroll::slot(token)?))?;
                    return Command::new("/usr/bin/true")
                        .status()
                        .context("returning TPM success");
                }
            }
        }
    }
    // Stock systemd retains Plymouth/console passphrase fallback. It may also
    // use other configured tokens; absent structured proof we say unknown.
    let result = Command::new(ORIGINAL).args(args).status()?;
    if result.success()
        && let Some(uuid) = uuid
    {
        record(&uuid, "unknown", None, None)?;
    }
    Ok(result)
}

pub fn record_root(root: &Path) -> Result<()> {
    // /sysroot is the real filesystem before ostree switches to its overlay.
    // Separate /var might not yet be mounted; use only the root block ancestry.
    let nested = root.join("sysroot");
    let physical = if nested.is_dir() { &nested } else { root };
    let mount = json(
        Command::new("/usr/bin/findmnt")
            .args(["--json", "--target"])
            .arg(physical)
            .args(["--output", "MAJ:MIN"]),
    )?;
    let number = mount["filesystems"][0]["maj:min"]
        .as_str()
        .context("root block mount unavailable")?;
    let tree = json(
        Command::new("/usr/bin/lsblk")
            .args(["--json", "--inverse", "--output", "FSTYPE,UUID"])
            .arg(format!("/dev/block/{number}")),
    )?;
    fn find(node: &Value, uuids: &mut Vec<String>) {
        if node["fstype"] == "crypto_LUKS"
            && let Some(uuid) = node["uuid"].as_str()
        {
            uuids.push(uuid.into());
        }
        if let Some(children) = node["children"].as_array() {
            for child in children {
                find(child, uuids);
            }
        }
    }
    let mut uuids = Vec::new();
    for node in tree["blockdevices"]
        .as_array()
        .context("no root ancestors")?
    {
        find(node, &mut uuids);
    }
    uuids.sort();
    uuids.dedup();
    if uuids.len() != 1 {
        return Ok(());
    }
    let uuid = &uuids[0];
    let source = Path::new("/run/ilex/unlock").join(format!("{uuid}.json"));
    if !source.exists() {
        record(uuid, "unknown", None, None)?;
    }
    let value: Value = serde_json::from_slice(&fs::read(source)?)?;
    write_state(Path::new("/run/ilex/root-unlock.json"), &value)
}
