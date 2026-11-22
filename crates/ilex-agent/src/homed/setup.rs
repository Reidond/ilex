//! First boot creates one local homed identity. A preallocated LUKS UUID
//! binds a resumed transaction to its own home, even across a lost reply.

use super::{
    CreateArgs, SetupArgs, SetupIdentity, check_capacity, create_inner, parse_real_name, valid_name,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{ErrorKind, IsTerminal, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Command, Output, Stdio},
};

const STATE: &str = "/var/lib/ilex/home-setup";
const ISSUE: &str = "/etc/issue.d/90-ilex-home-setup.issue";

type Result<T> = std::result::Result<T, String>;

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq, Clone)]
struct Selection {
    id: Option<String>,
    schema: Option<u32>,
    mode: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Intent {
    schema: u32,
    profile: Selection,
    name: String,
    size: u64,
    luks_uuid: String,
    uid: Option<u64>,
}

fn command(program: &str, args: &[&str]) -> Result<Output> {
    Command::new("/usr/bin/timeout")
        .args(["--kill-after=2s", "15s", program])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("starting {program}: {e}"))
}

fn limited(path: &Path) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    File::open(path)
        .and_then(|f| f.take(1_048_577).read_to_end(&mut data))
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    if data.len() > 1_048_576 {
        return Err(format!("{} exceeds its size limit", path.display()));
    }
    Ok(data)
}

fn read<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_slice(&limited(path)?).map_err(|_| format!("invalid {}", path.display()))
}

fn save(name: &str, data: &impl Serialize) -> Result<()> {
    let result = (|| -> std::io::Result<()> {
        let dir = Path::new(STATE);
        // All writers are serialized by the directory lock and all names are fixed.
        let temp = dir.join(format!(".{name}.tmp"));
        remove(&temp)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temp)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        serde_json::to_writer(&mut file, data)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(name))?;
        File::open(dir)?.sync_all()
    })();
    result.map_err(|e| format!("saving setup state: {e}"))
}

fn remove(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
struct Profile {
    schema: u32,
    revision: u32,
    username: String,
    display_name: String,
    shell: String,
    locale: String,
    storage: String,
    disk_size_gib: u64,
    groups: Vec<String>,
}

fn profile(id: &str, schema: u32) -> Result<Profile> {
    if schema != 1 || !valid_name(id) || id.contains('_') {
        return Err("unsupported installer profile schema or ID".into());
    }
    let catalogue: Value = read(Path::new("/usr/share/ilex/users.json")).map_err(
        |_| "The installed image has no supported user catalogue. Update the image and retry.",
    )?;
    let unavailable = || {
        format!(
            "Profile {id} (schema {schema}) is unavailable in this image. Update the image and retry."
        )
    };
    let entry = catalogue["profiles"]
        .as_array()
        .filter(|_| catalogue["schema"] == 1)
        .and_then(|entries| {
            entries
                .iter()
                .find(|row| row["id"] == id && row["schema"] == schema)
        })
        .ok_or_else(unavailable)?;
    let bytes = limited(
        &Path::new("/usr/share/ilex/users")
            .join(id)
            .join("user.json"),
    )
    .map_err(|_| unavailable())?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let profile: Profile = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
    if entry["sha256"] != digest
        || entry["revision"] != profile.revision
        || profile.schema != schema
        || profile.username != id
        || profile.storage != "luks-btrfs"
        || profile.shell != "bash"
        || !(2..=1_048_576).contains(&profile.disk_size_gib)
    {
        return Err(unavailable());
    }
    parse_real_name(&profile.display_name)?;
    Ok(profile)
}

fn account_exists(name: &str, local_only: bool) -> Result<bool> {
    let args = if local_only {
        vec!["-s", "files", "passwd", name]
    } else {
        vec!["passwd", name]
    };
    match command("/usr/bin/getent", &args)?.status.code() {
        Some(0) => Ok(true),
        Some(2) => Ok(false),
        _ => Err("Could not check the username. Retry when account lookup is available.".into()),
    }
}

fn home_uid(intent: &Intent) -> Result<Option<u64>> {
    let name = &intent.name;
    if account_exists(name, true)? {
        return Err(format!(
            "{name} is already a local account. Choose a distinct daily username."
        ));
    }
    let image_path = format!("/var/home/{name}.home");
    let image = match fs::symlink_metadata(&image_path) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Checking the daily home: {e}")),
    };
    if !account_exists(name, false)? && image.is_none() {
        return match fs::symlink_metadata(format!("/var/home/{name}")) {
            Ok(_) => Err(format!(
                "The path /var/home/{name} is already occupied. No data was changed."
            )),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Checking for an existing home directory: {e}")),
        };
    }
    // Never print, serialize or store homectl's raw record. It can include
    // privileged metadata when the caller is root.
    let output = command("/usr/bin/homectl", &["inspect", name, "--json=short"])?;
    let occupied = || {
        format!("{name} is occupied by another account or an unfinished home. No data was changed.")
    };
    if !output.status.success() {
        return Err(occupied());
    }
    let record: Value = serde_json::from_slice(&output.stdout).map_err(|_| occupied())?;
    let machine = fs::read_to_string("/etc/machine-id").map_err(|e| e.to_string())?;
    let binding = &record["binding"][machine.trim()];
    let state = record["status"][machine.trim()]["state"].as_str();
    let uid = binding["uid"].as_u64();
    // The binding is written when homework has created the LUKS filesystem.
    // The top-level requested UUID alone is insufficient evidence of success.
    let actual_uuid = binding["luksUuid"].as_str().map(|s| s.replace('-', ""));
    if record["userName"] != *name
        || binding["storage"] != "luks"
        || binding["fileSystemType"] != "btrfs"
        || !matches!(
            state,
            Some("inactive" | "active" | "lingering" | "locked" | "dirty")
        )
        || binding["imagePath"].as_str() != Some(&image_path)
        || actual_uuid.as_deref() != Some(intent.luks_uuid.as_str())
        || !image.is_some_and(|m| m.is_file() && m.len() > 0)
        || uid.is_none()
        || intent.uid.is_some_and(|previous| Some(previous) != uid)
    {
        return Err(occupied());
    }
    Ok(uid)
}

fn new_uuid() -> Result<String> {
    fs::read_to_string("/proc/sys/kernel/random/uuid")
        .map(|uuid| uuid.trim().replace('-', ""))
        .map_err(|e| e.to_string())
}

fn verify_existing_home(name: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("Run setup in a terminal to unlock and verify the existing daily home.".into());
    }
    println!("Unlock the daily home {name} to verify it.");
    // Hold a homed reference while testing the mounted home. Homed releases
    // it when verification exits and retains any other sessions' references.
    let status = Command::new("/usr/bin/timeout")
        .args([
            "--foreground",
            "--kill-after=5s",
            "5m",
            "/usr/bin/homectl",
            "with",
            name,
            "/usr/bin/mountpoint",
            ".",
        ])
        .env_remove("PASSWORD")
        .env_remove("NEWPASSWORD")
        .env_remove("SYSTEMD_PLEASE_SUSPEND_HOME")
        .status()
        .map_err(|e| format!("Starting daily home verification: {e}"))?;
    if !status.success() {
        return Err(format!(
            "Could not verify the daily home {name}. Check its password or restore its backing image, then retry setup."
        ));
    }
    Ok(())
}

pub(super) fn run(args: SetupArgs) -> Result<()> {
    if command("/usr/bin/id", &["-u"])?.stdout != b"0\n" {
        return Err("Run sudo ilexctl home setup from the maintenance account.".into());
    }
    let handoff: Value = read(Path::new("/etc/ilex/setup.json"))?;
    let selected: Selection = serde_json::from_value(handoff["profile"].clone())
        .map_err(|_| "Invalid installer profile selection.")?;
    if handoff["schema"] != 1 {
        return Err("Unsupported installer setup schema.".into());
    }
    let profile = match (&selected.id, selected.schema, selected.mode.as_deref()) {
        (Some(id), Some(schema), None) => Some(profile(id, schema)?),
        (None, None, Some("custom")) => None,
        _ => return Err("The installer did not select daily home setup. Use ilexctl home create for a standalone home.".into()),
    };
    if profile.is_some() && (args.name.is_some() || args.disk_size.is_some()) {
        return Err(
            "The named profile sets its username and capacity. Omit --name and --disk-size.".into(),
        );
    }
    let dir = Path::new(STATE);
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let meta = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
    if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Err("Invalid setup state directory.".into());
    }
    fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    let lock = File::open(dir).map_err(|e| e.to_string())?;
    lock.try_lock()
        .map_err(|_| "Home setup is already running. Retry after it finishes.")?;
    if !command(
        "/usr/bin/systemctl",
        &["is-active", "--quiet", "systemd-homed.service"],
    )?
    .status
    .success()
    {
        return Err(
            "systemd-homed is unavailable. Start systemd-homed.service and run setup again.".into(),
        );
    }
    let state = dir.join("state.json");
    let mut intent: Intent = if state.try_exists().map_err(|e| e.to_string())? {
        let intent: Intent = read(&state)?;
        if intent.schema != 1
            || intent.profile != selected
            || !valid_name(&intent.name)
            || !(2..=1_048_576).contains(&intent.size)
            || intent.luks_uuid.len() != 32
            || !intent.luks_uuid.bytes().all(|b| b.is_ascii_hexdigit())
            || selected.id.as_ref().is_some_and(|id| *id != intent.name)
        {
            return Err("Existing setup state does not match the installer selection.".into());
        }
        intent
    } else {
        let (name, size) = if let Some(profile) = &profile {
            (profile.username.clone(), profile.disk_size_gib)
        } else {
            (args.name.clone().ok_or("Choose a daily username with sudo ilexctl home setup --name NAME. Use --disk-size to set its capacity.")?,
             args.disk_size.unwrap_or(20))
        };
        Intent {
            schema: 1,
            profile: selected,
            name,
            size,
            luks_uuid: new_uuid()?,
            uid: None,
        }
    };
    let uid = match home_uid(&intent)? {
        Some(uid) => {
            if args.name.as_ref().is_some_and(|name| *name != intent.name)
                || args.disk_size.is_some_and(|size| size != intent.size)
            {
                return Err(format!(
                    "Setup is already recorded for {} ({} GiB). Omit the options to resume.",
                    intent.name, intent.size
                ));
            }
            verify_existing_home(&intent.name)?;
            uid
        }
        None if intent.uid.is_some() => {
            return Err(
                "The previously configured home is missing. Restore it before retrying setup."
                    .into(),
            );
        }
        None => {
            // An intent without a registered account, backing file or home
            // directory can be corrected. Recheck the new name before saving.
            if let Some(name) = args.name.filter(|name| *name != intent.name) {
                intent.name = name;
                intent.luks_uuid = new_uuid()?;
                if home_uid(&intent)?.is_some() {
                    return Err("The requested daily home already exists.".into());
                }
            }
            intent.size = args.disk_size.unwrap_or(intent.size);
            check_capacity(intent.size)?;
            if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
                return Err(
                    "Open a terminal as the maintenance administrator and rerun setup.".into(),
                );
            }
            let gamemode = match profile.as_ref() {
                Some(p) if p.groups.iter().any(|g| g == "gamemode") => {
                    command("/usr/bin/getent", &["-s", "files", "group", "gamemode"])?
                        .status
                        .success()
                }
                _ => false,
            };
            save("state.json", &intent)?;
            save(
                "identity.json",
                &json!({"userName": intent.name, "luksUuid": intent.luks_uuid}),
            )?;
            println!(
                "Creating the encrypted daily home {} ({} GiB).",
                intent.name, intent.size
            );
            let result = create_inner(
                CreateArgs {
                    name: intent.name.clone(),
                    disk_size: intent.size,
                    admin: false,
                    real_name: profile.as_ref().map(|p| p.display_name.clone()),
                },
                Some(SetupIdentity {
                    path: &dir.join("identity.json"),
                    locale: profile.as_ref().map(|p| p.locale.as_str()),
                    gamemode,
                }),
            );
            remove(&dir.join("identity.json")).map_err(|e| e.to_string())?;
            match result {
                Ok(0) => (),
                Ok(_) => {
                    return Err(
                        "Daily home creation did not complete. Run setup again to retry.".into(),
                    );
                }
                Err(error) => return Err(format!("{error}. Run setup again to retry.")),
            }
            home_uid(&intent)?
                .ok_or("Daily home creation did not complete. Run setup again to retry.")?
        }
    };
    if intent.uid.is_none() {
        intent.uid = Some(uid);
        save("state.json", &intent)?;
    }
    remove(&dir.join("identity.json")).map_err(|e| e.to_string())?;
    remove(Path::new(ISSUE)).map_err(|e| e.to_string())?;
    println!(
        "Daily home {} is ready. Sign out of the maintenance account and sign in as {}.",
        intent.name, intent.name
    );
    Ok(())
}
