//! The `installer::` tier's faults. The first is a pause at a named point of
//! `Apply`, so
//! that the test can do what a person or a program could do in that moment
//! (mount a partition, open the disk exclusively) and see the helper's own
//! claim refuse. Nothing calls it unless the hidden `--pause-before-hold` flag
//! names a file.

use std::{
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// The longest a pause lasts: a test that forgets to end it does not wedge
/// the helper.
const LONGEST: Duration = Duration::from_secs(120);

/// While `file` exists: say so by creating `<file>.reached`, and wait for it to
/// be removed. Without the file this returns at once.
pub fn pause(file: &Path) {
    if !file.exists() {
        return;
    }
    let reached = format!("{}.reached", file.display());
    let _ = fs::write(&reached, b"");
    let started = Instant::now();
    while file.exists() && started.elapsed() < LONGEST {
        thread::sleep(Duration::from_millis(100));
    }
}

/// The fault a test asks for through the fixed credential
/// `ilex.installer.fault` (an SMBIOS `io.systemd.credential:` string the harness
/// puts on the VM, imported by the helper's and the answers unit, and by the
/// session's, which reads the value `session`). It can only make the installer
/// fail or stay quiet: `helper` (the helper exits as soon as it starts, so it
/// never takes its bus name), `answers` (unattended mode kills itself before
/// it asks the helper for anything) or `apply` (the helper kills itself as
/// `Apply` has started the installation thread, so that the call is never
/// answered). Nothing is read from anywhere else.
pub fn credential() -> Option<String> {
    let dir = std::env::var_os("CREDENTIALS_DIRECTORY")?;
    let text = fs::read_to_string(Path::new(&dir).join("ilex.installer.fault")).ok()?;
    Some(text.trim().to_owned())
}

/// Whether the credential names `fault`.
pub fn requested(fault: &str) -> bool {
    credential().as_deref() == Some(fault)
}
