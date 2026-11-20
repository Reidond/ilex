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
pub use enroll::{Device, Enrollment, recovery, status, tpm, validate_token};
mod layout;
pub use layout::{Layout, discover, recovery_layout};
pub mod initrd;

/// Never propagate a subprocess's output in an error: cryptenroll's recovery
/// stdout is a secret, and even an unexpected error must remain safe to log.
fn run(command: &mut Command) -> Result<Output> {
    command
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin")
        .env("LANG", "C.UTF-8")
        .env("SYSTEMD_LOG_LEVEL", "err")
        .env("SYSTEMD_LOG_TARGET", "console")
        .env("SYSTEMD_COLORS", "0")
        .env("SYSTEMD_URLIFY", "0")
        .env("SYSTEMD_ASK_PASSWORD_KEYRING_TIMEOUT_SEC", "0")
        .stdin(Stdio::null())
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
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let parent = path.parent().context("state has no parent")?;
    let existed = parent.exists();
    fs::create_dir_all(parent)?;
    ensure!(
        !fs::symlink_metadata(parent)?.file_type().is_symlink(),
        "state directory is a symlink"
    );
    if !existed {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }
    let temporary = parent.join(format!(".enrollment-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
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
