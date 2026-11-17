//! Shared infrastructure identity is derived from repository + Windows SID,
//! rather than a checkout owner. VM mutations also require the journal's UUID.
use crate::{host::*, runtime};
use anyhow::{Context, Result, ensure};
use pgp::{
    composed::{CleartextSignedMessage, Deserializable, SignedPublicKey},
    types::KeyDetails,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Stdio};

const IMAGE_URL: &str = "https://download.fedoraproject.org/pub/fedora/linux/releases/45/Cloud/x86_64/images/Fedora-Cloud-Base-Generic-45-1.4.x86_64.qcow2";
const IMAGE_SHA: &str = "50bdc5e70f6a24f540932d15411c1c6ae9fcfa40fb990d876d83c5cc823b2ad0";
const FINGERPRINT: &str = "4F50A6114CD5C6976A7F1179655A4B02F577861E";

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Up,
    Status,
    Down,
    Update,
    SelfCheck,
    RecoverySelfCheck,
    MaintenanceSelfCheck,
    CleanupIncomplete,
    Rollback,
    AcceptUpdate,
}
/// Endpoint metadata is returned only after host-key and provisioning-identity checks.
/// T3 adds finite job operations to the forced-command protocol; no shell is exposed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderEndpoint {
    pub schema: u32,
    pub vm_id: String,
    pub owner: String,
    pub address: String,
    pub user: String,
    pub identity_file: PathBuf,
    pub known_hosts_file: PathBuf,
    pub host_key_alias: String,
    pub provisioning_revision: String,
}
#[derive(Debug, Default, Serialize)]
pub struct Options {
    pub memory_gib: Option<u32>,
    pub cpus: Option<u32>,
    pub rollback_probe: bool,
    pub wait_for_memory_seconds: u32,
}
pub fn revision() -> String {
    hash(
        format!(
            "{}\n{}\n{}",
            provision(),
            include_str!("../assets/builder-control.sh"),
            include_str!("../assets/smoke.sh")
        )
        .as_bytes(),
    )
}
fn provision() -> String {
    include_str!("../assets/provision.sh").replace("# @ILEX_JOB_RUNNER@", &format!(
        "cat > /usr/local/libexec/ilex-builder-jobs <<'ILEX_JOB_RUNNER_EOF'\n{}\nILEX_JOB_RUNNER_EOF\nchmod 0755 /usr/local/libexec/ilex-builder-jobs",
        include_str!("../assets/builder-jobs.py")))
}
pub fn verify_fedora_metadata() -> Result<()> {
    let keys = SignedPublicKey::from_bytes_many(
        include_bytes!("../assets/fedora-signing-keys.gpg").as_slice(),
    )?;
    let mut pinned = None;
    for key in keys {
        let key = key?;
        if format!("{:X}", key.fingerprint()) == FINGERPRINT {
            pinned = Some(key);
            break;
        }
    }
    let key = pinned.context("pinned Fedora 45 signing key absent")?;
    let (message, _) =
        CleartextSignedMessage::from_string(include_str!("../assets/Fedora-Cloud-45-CHECKSUM"))?;
    message
        .verify(&key)
        .context("Fedora checksum signature verification")?;
    ensure!(
        message.signed_text().lines().any(|line| line
            == format!("SHA256 (Fedora-Cloud-Base-Generic-45-1.4.x86_64.qcow2) = {IMAGE_SHA}")),
        "Fedora signed checksum does not match reviewed image"
    );
    Ok(())
}
pub fn run(operation: Operation, options: Options) -> Result<Value> {
    ilex_tooling_core::check_version(1)?;
    verify_fedora_metadata()?;
    if matches!(operation, Operation::MaintenanceSelfCheck) {
        return maintenance_self_check(options);
    }
    let base = root()?.join("builder");
    // Status never creates state, downloads, starts a VM, or repairs a journal.
    if matches!(operation, Operation::Status) && !base.exists() {
        return Ok(json!({"schema":1,"state":"absent","provisioning_revision":revision()}));
    }
    if matches!(operation, Operation::Status) {
        let temp = std::env::temp_dir().join(format!("ilex-builder-status-{}", std::process::id()));
        fs::create_dir(&temp)?;
        let result = (|| -> Result<Value> {
            let script = temp.join("status.ps1");
            let request = temp.join("request.json");
            fs::write(&script, include_str!("../assets/builder.ps1"))?;
            fs::write(
                &request,
                serde_json::to_vec(
                    &json!({"schema":1,"operation":operation,"root":base,"revision":revision()}),
                )?,
            )?;
            let output = output_owned(&mut powershell(&script, &request), Stdio::piped())?;
            ensure!(
                output.status.success(),
                "builder status failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(serde_json::from_slice(&output.stdout)?)
        })();
        fs::remove_dir_all(&temp)?;
        return result;
    }
    secure_dir(&root()?)?;
    secure_dir(&base)?;
    let _guard = lock(&base.join("host.lock"))?;
    let qemu = if matches!(operation, Operation::Up | Operation::RecoverySelfCheck) {
        Some(runtime::fetch()?)
    } else {
        None
    };
    let image = base.join("Fedora-Cloud-Base-Generic-45-1.4.x86_64.qcow2");
    if matches!(operation, Operation::Up | Operation::RecoverySelfCheck) {
        ensure!(
            fs2::available_space(&base)? >= 45 * 1024 * 1024 * 1024,
            "builder bootstrap requires 45 GiB free including the 30 GiB reserve"
        );
        download(IMAGE_URL, &image, IMAGE_SHA, 606732288)?;
    }
    let helper = base.join("builder.ps1");
    // Fixed reviewed scripts, with parameter data in a versioned request file.
    fs::write(&helper, include_str!("../assets/builder.ps1"))?;
    fs::write(base.join("provision.sh"), provision())?;
    fs::write(
        base.join("builder-control.sh"),
        include_str!("../assets/builder-control.sh"),
    )?;
    fs::write(base.join("smoke.sh"), include_str!("../assets/smoke.sh"))?;
    let checks = root()?.join("builder-checks");
    let script = if matches!(operation, Operation::RecoverySelfCheck) {
        secure_dir(&checks)?;
        let script = base.join("builder-recovery-check.ps1");
        fs::write(
            &script,
            include_str!("../assets/builder-recovery-check.ps1"),
        )?;
        script
    } else {
        helper.clone()
    };
    let request = base.join("request.json");
    atomic(
        &request,
        &serde_json::to_vec(
            &json!({"schema":1,"operation":operation,"root":base,"runtime":qemu,"image":image,"revision":revision(),"options":options,"helper":helper,"checks":checks}),
        )?,
    )?;
    let output = output_owned(&mut powershell(&script, &request), Stdio::inherit())?;
    ensure!(
        output.status.success(),
        "builder helper failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let value: Value =
        serde_json::from_slice(&output.stdout).context("invalid builder protocol reply")?;
    ilex_tooling_core::check_version(
        value["schema"]
            .as_u64()
            .context("missing builder reply schema")? as u32,
    )?;
    Ok(value)
}

// This diagnostic has its own installation and helper files. It must not hold
// the shared builder lock, resize it, or replace files another checkout uses.
fn maintenance_self_check(options: Options) -> Result<Value> {
    let parent = root()?.join("builder-checks");
    secure_dir(&root()?)?;
    secure_dir(&parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let work = parent.join(format!(
        "maintenance-{}-{nonce}.partial",
        std::process::id()
    ));
    secure_dir(&work)?;
    let fixture_guard = lock(&work.join("host.lock"))?;
    let qemu = runtime::fetch()?;
    let cached = root()?.join("builder/Fedora-Cloud-Base-Generic-45-1.4.x86_64.qcow2");
    let image = if cached.exists() {
        verify(&cached, IMAGE_SHA)?;
        cached
    } else {
        let image = work.join("Fedora-Cloud-Base-Generic-45-1.4.x86_64.qcow2");
        download(IMAGE_URL, &image, IMAGE_SHA, 606732288)?;
        image
    };
    let helper = work.join("builder.ps1");
    fs::write(&helper, include_str!("../assets/builder.ps1"))?;
    for (name, content) in [
        ("provision.sh", include_str!("../assets/provision.sh")),
        (
            "builder-control.sh",
            include_str!("../assets/builder-control.sh"),
        ),
        ("smoke.sh", include_str!("../assets/smoke.sh")),
    ] {
        fs::write(work.join(name), content)?;
    }
    let script = work.join("check.ps1");
    let request = work.join("request.json");
    fs::write(
        &script,
        include_str!("../assets/builder-maintenance-check.ps1"),
    )?;
    fs::write(
        &request,
        serde_json::to_vec(&json!({
            "schema":1,"work":work,"helper":helper,"runtime":qemu,
            "image":image,"revision":revision(),"options":options
        }))?,
    )?;
    let output = output_owned(&mut powershell(&script, &request), Stdio::inherit())?;
    ensure!(
        output.status.success(),
        "maintenance check failed; diagnostic/recovery state retained at {}",
        work.display()
    );
    let value: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        value["fixture_removed"] == true,
        "maintenance fixture cleanup is unconfirmed"
    );
    drop(fixture_guard);
    // Same bounded, no-reparse-point removal used for private runtime fixtures.
    runtime::remove_owned_tree(&work, &parent)?;
    Ok(value)
}
