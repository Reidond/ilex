use super::{
    transaction::{OwnedTpm, current_boot},
    *,
};
use serde_json::json;
use std::{
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
};

fn observation() -> Result<Option<Value>> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open("/run/ilex/root-unlock.json");
    let file = match file {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == 0
            && metadata.mode() & 0o022 == 0
            && metadata.len() <= 4096,
        "invalid root unlock evidence file"
    );
    let mut data = Vec::new();
    file.take(4097).read_to_end(&mut data)?;
    Ok(Some(
        serde_json::from_slice(&data).context("invalid root unlock evidence")?,
    ))
}

pub(super) fn current_observation(device: &Device, boot: &str) -> Result<Value> {
    Ok(observation()?
        .filter(|evidence| {
            evidence["schema"] == 1
                && evidence["uuid"] == device.uuid
                && evidence["boot_id"] == boot
        })
        .unwrap_or(Value::Null))
}

pub fn verify() -> Result<Value> {
    harden()?;
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "security verification requires root"
    );
    let root = Path::new("/");
    let boot = current_boot()?;
    let state_path = root.join("var/lib/ilex/security/enrollment.json");
    if !state_path.try_exists()? {
        return Ok(json!({"schema":1, "state":"pending", "reason":"no-enrollment-record"}));
    }
    let layout = match discover(root) {
        Ok(Some(layout)) => layout,
        Ok(None) => return invalidate(root, &boot, "encrypted-root-unavailable"),
        Err(_) => return invalidate(root, &boot, "storage-discovery-failed"),
    };
    if SecurityState::read_bound(root)?.is_some_and(|state| state.value["uuid"] != layout.uuid) {
        return invalidate(root, &boot, "enrollment-device-mismatch");
    }
    let device = match Device::open(&layout.device) {
        Ok(device) => device,
        Err(_) => return invalidate(root, &boot, "encrypted-root-unavailable"),
    };
    let mut state = SecurityState::open(root, &device)?;
    let observation = match current_observation(&device, &boot) {
        Ok(observation) => observation,
        Err(_) => {
            return record(
                &mut state,
                &boot,
                "pending",
                "unlock-evidence-unreadable",
                &Value::Null,
            );
        }
    };
    let tpm: Option<Enrollment> = serde_json::from_value(state.value["tpm"].clone())?;
    let current = tpm.and_then(|entry| {
        (observation["token"]
            .as_str()
            .and_then(|id| id.parse::<u32>().ok())
            == Some(entry.token)
            && observation["slot"] == entry.slot)
            .then_some(entry)
    });
    let (outcome, reason) = if state.tpm_pending()? || state.value["tpm_enrolled_boot_id"] == boot {
        ("pending", "awaiting-cold-boot")
    } else if observation["method"] == "tpm2" {
        let fingerprint = current
            .and_then(|entry| OwnedTpm::new(entry, &device.header().ok()?).ok())
            .and_then(|entry| serde_json::to_value(entry).ok());
        if let Some(fingerprint) = fingerprint
            && (state.value["tpm_header"].is_null() || state.value["tpm_header"] == fingerprint)
            && state.acknowledged()
        {
            state.value["tpm_header"] = fingerprint;
            ("verified", "tpm-unlock-observed")
        } else {
            ("error", "tpm-evidence-mismatch")
        }
    } else if observation["method"] == "fallback"
        && tpm.is_none()
        && state.value["state"] == "unavailable"
    {
        ("pending", "fallback-unlock-deferred")
    } else if observation["method"] == "fallback" {
        ("error", "fallback-unlock-observed")
    } else {
        ("pending", "unlock-method-unknown")
    };
    record(&mut state, &boot, outcome, reason, &observation["method"])
}

fn invalidate(root: &Path, boot: &str, reason: &str) -> Result<Value> {
    let Some(bound) = SecurityState::read_bound(root)? else {
        return Ok(json!({"schema":1, "boot_id":boot, "state":"pending", "reason":reason}));
    };
    let uuid = bound.value["uuid"]
        .as_str()
        .context("missing enrollment UUID")?;
    let _lock = Device::lock_uuid(uuid)?;
    let mut state = SecurityState::read_bound(root)?.context("enrollment state changed")?;
    ensure!(state.value["uuid"] == uuid, "enrollment UUID changed");
    record(&mut state, boot, "pending", reason, &Value::Null)
}

fn record(
    state: &mut SecurityState,
    boot: &str,
    outcome: &str,
    reason: &str,
    method: &Value,
) -> Result<Value> {
    let result = json!({"schema":1, "uuid":state.value["uuid"], "boot_id":boot,
        "state":outcome, "reason":reason, "method":method});
    state.value["verification"] = result.clone();
    if state.value["state"] == "verified"
        || (state.acknowledged()
            && (outcome == "verified" || state.value["state"] != "unavailable"))
    {
        state.value["state"] = json!(outcome);
        state.value["reason"] = json!(reason);
    }
    state.save()?;
    Ok(result)
}
