use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
};
use zeroize::Zeroize;

mod secret;
pub use secret::{Secret, harden, secret_fd};
mod probe;
pub use probe::{Probe, probe, tool_manifest, tool_versions};
mod enroll;
pub use enroll::{Device, Enrollment, recovery, status, validate_token};
mod layout;
pub use layout::{Layout, discover, installer_root, recovery_layout};
pub mod initrd;
mod pending;
mod transaction;
pub use transaction::{
    Reconciled, SecurityState, acknowledged_recovery, eligibility, recorded_recovery,
    recovery_record,
};
mod crypt;
pub mod local;
mod verify;
pub use verify::verify;

/// Never propagate a subprocess's output in an error: cryptenroll's recovery
/// stdout is a secret, and even an unexpected error must remain safe to log.
fn run(command: &mut Command) -> Result<Output> {
    run_with_input(command, Stdio::null())
}

fn run_with_input(command: &mut Command, input: Stdio) -> Result<Output> {
    command
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin")
        .env("LANG", "C.UTF-8")
        .env("SYSTEMD_LOG_LEVEL", "err")
        .env("SYSTEMD_LOG_TARGET", "console")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_URLIFY", "0")
        .env("SYSTEMD_ASK_PASSWORD_KEYRING_TIMEOUT_SEC", "0")
        .stdin(input)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut output = command.output().context("starting enrollment tool")?;
    output.stderr.zeroize();
    if !output.status.success() {
        output.stdout.zeroize();
        anyhow::bail!(
            "enrollment tool failed (exit {:?}); secret output suppressed",
            output.status.code()
        );
    }
    Ok(output)
}

fn header(path: &Path) -> Result<Value> {
    let value = json(
        Command::new("/usr/sbin/cryptsetup")
            .args(["luksDump", "--dump-json-metadata"])
            .arg(path),
    )?;
    ensure!(
        value["keyslots"].is_object() && value["tokens"].is_object(),
        "not LUKS2"
    );
    Ok(value)
}

fn json(command: &mut Command) -> Result<Value> {
    let mut output = run(command)?;
    let value = serde_json::from_slice(&output.stdout).context("invalid tool JSON");
    output.stdout.zeroize();
    value
}

/// Atomic, nonsecret state. Parent is inside the encrypted filesystem and is
/// never shipped as image content. Callers must hold the completion lock.
pub fn write_state(path: &Path, value: &Value) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let parent = path.parent().context("state has no parent")?;
    let missing: Vec<_> = parent.ancestors().take_while(|p| !p.exists()).collect();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)?;
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "state directory is a symlink"
    );
    for directory in missing.into_iter().rev() {
        fs::File::open(
            directory
                .parent()
                .context("state directory has no parent")?,
        )?
        .sync_all()?;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid state filename")?;
    let temporary = parent.join(format!("{name}.tmp"));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
