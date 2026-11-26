//! The `installer::` tier's one fault: a pause at a named point of `Apply`, so
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
