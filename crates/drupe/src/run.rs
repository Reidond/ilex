//! Running a tool's command: look it up in `/run/drupe/cmd`, hand the
//! environment over in a memfd and become `drupe-enter`, which mounts the
//! tool at `/app` and executes the command as the caller.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Seek, Write};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

use serde_json::Value;

use crate::manifest::valid_name;
use crate::paths::{CMD, ENTER};

/// "Command not found", as shells report it.
const NOT_FOUND: u8 = 127;
/// "Found but could not be started."
const FAILED: u8 = 126;

/// Replace this process with `cmd` of whichever tool provides it. Returns
/// only on failure.
pub fn exec(cmd: &str, args: Vec<OsString>) -> ExitCode {
    let entry = match lookup(cmd) {
        Ok(entry) => entry,
        Err(why) => {
            eprintln!("drupe: {why}");
            return ExitCode::from(NOT_FOUND);
        }
    };
    // glibc drops LD_LIBRARY_PATH, TMPDIR and more from the environment of a
    // setuid program; the helper gets the full one through a descriptor and
    // gives it to the command once it is no longer root.
    let env_fd = match env_memfd() {
        Ok(fd) => fd.to_string(),
        Err(e) => {
            eprintln!("drupe: passing the environment on: {e}");
            return ExitCode::from(FAILED);
        }
    };
    let err = Command::new(ENTER)
        .arg(&entry.tool)
        .arg(env_fd)
        .arg(&entry.program)
        .args(&entry.argv)
        .args(args)
        .exec();
    eprintln!("drupe: {ENTER}: {err}");
    ExitCode::from(FAILED)
}

struct Entry {
    tool: String,
    program: String,
    argv: Vec<String>,
}

fn lookup(cmd: &str) -> Result<Entry, String> {
    let none = || format!("no installed tool provides `{cmd}` (see `drupe list`)");
    if !valid_name(cmd) {
        return Err(none());
    }
    let path = format!("{CMD}/{cmd}.json");
    let text = fs::read_to_string(&path).map_err(|_| none())?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let field = |key: &str| {
        doc[key]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("{path}: no `{key}`"))
    };
    let argv = doc["argv"]
        .as_array()
        .and_then(|argv| argv.iter().map(|a| a.as_str().map(str::to_string)).collect())
        .ok_or_else(|| format!("{path}: no `argv`"))?;
    Ok(Entry { tool: field("tool")?, program: field("program")?, argv })
}

/// The environment as `KEY=VALUE\0` entries in an anonymous file that stays
/// open across `exec`; returns its descriptor number.
fn env_memfd() -> io::Result<i32> {
    // SAFETY: the name is a NUL-terminated literal. Without MFD_CLOEXEC the
    // descriptor is inherited by the helper, which is the point.
    let fd = unsafe { libc::memfd_create(c"drupe-env".as_ptr(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a descriptor we just created and nothing else owns.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let mut block = Vec::new();
    for (key, value) in std::env::vars_os() {
        block.extend_from_slice(key.as_bytes());
        block.push(b'=');
        block.extend_from_slice(value.as_bytes());
        block.push(0);
    }
    file.write_all(&block)?;
    file.rewind()?;
    Ok(file.into_raw_fd())
}
