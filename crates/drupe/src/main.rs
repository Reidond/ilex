//! `drupe`: developer tools from signed disk images.
//!
//! An image-based system has no place to install `strace` or `qemu` into, and
//! such tools can't live in a container: they have to see the machine as it
//! is. A drupe tool is one disk image (a discoverable disk image: an erofs
//! root with dm-verity and a signature) that holds the tool and every library
//! it loads, built to be found at `/app`. Running one of its commands mounts
//! that image at `/app` for that process tree only and executes the command
//! as the calling user with everything else untouched: the same `/usr`, the
//! same home, the same processes. Nothing is added to the host's library
//! path, so a tool can't break the system and the system's updates can't
//! break a tool.
//!
//! ```text
//! drupe install <image>     copy an image into the store and activate it (root)
//! drupe remove <tool>       deactivate a tool and delete its image (root)
//! drupe activate            mount every installed image; drupe.service at boot (root)
//! drupe list                installed tools and the commands they provide
//! drupe run <cmd> [args]    run a command of an installed tool
//! ```
//!
//! Each command of an active tool is also a symlink to this binary in
//! `/run/drupe/bin`, which `/etc/profile.d/drupe.sh` puts first in `PATH`:
//! called under another name, `drupe` runs that command.
//!
//! State: images in `/var/lib/drupe/images/<tool>.raw`; everything else is
//! rebuilt from them under `/run/drupe` (see [`paths`]). An image is only
//! ever mounted through `systemd-dissect` with an image policy that requires
//! a signed root partition, so it must carry a signature by a certificate in
//! `/etc/verity.d`, `/run/verity.d` or `/usr/lib/verity.d` (or the kernel's
//! keyring), and dm-verity checks every block read from it afterwards.

#[cfg(target_os = "linux")]
mod manifest;
#[cfg(target_os = "linux")]
mod run;
#[cfg(target_os = "linux")]
mod store;

/// Where everything lives.
#[cfg(target_os = "linux")]
mod paths {
    /// This binary; what the command symlinks point at.
    pub const DRUPE: &str = "/usr/bin/drupe";
    /// The setuid helper (`enter.rs`).
    pub const ENTER: &str = "/usr/libexec/drupe/drupe-enter";
    /// Where a tool's image appears while one of its commands runs.
    pub const APP: &str = "/app";
    /// Installed images, `<tool>.raw`. The only persistent state.
    pub const STORE: &str = "/var/lib/drupe/images";
    /// Runtime state, rebuilt by `drupe activate`.
    pub const RUN: &str = "/run/drupe";
    /// The images, mounted. Mode 0700: users reach a tool through
    /// `drupe-enter` only, where it is nosuid.
    pub const PRIV: &str = "/run/drupe/priv";
    /// `<command>.json`: which tool provides a command and how to start it.
    pub const CMD: &str = "/run/drupe/cmd";
    /// `<command>` symlinks to [`DRUPE`]; the directory that goes into `PATH`.
    pub const BIN: &str = "/run/drupe/bin";
    /// `<tool>.json`: a readable copy of each active tool's manifest.
    pub const TOOLS: &str = "/run/drupe/tools";
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::ffi::OsString;
    use std::path::Path;
    use std::process::ExitCode;

    const USAGE: &str = "Usage: drupe install <image> | remove <tool> | activate | list | run <cmd> [args...]";

    let mut args = std::env::args_os();
    let argv0 = args.next().unwrap_or_default();
    let called_as = Path::new(&argv0)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if called_as != "drupe" {
        return run::exec(&called_as, args.collect());
    }

    let sub = args.next().map(|a| a.to_string_lossy().into_owned());
    let rest: Vec<OsString> = args.collect();
    let one = |what: &str| -> Result<String, String> {
        match rest.as_slice() {
            [arg] => Ok(arg.to_string_lossy().into_owned()),
            _ => Err(format!("expected exactly one argument: {what}\n{USAGE}")),
        }
    };
    let result = match sub.as_deref() {
        Some("install") => one("<image>").and_then(|image| store::install(Path::new(&image))),
        Some("remove") => one("<tool>").and_then(|tool| store::remove(&tool)),
        Some("activate") => store::activate_all(),
        Some("list") => store::list(),
        Some("run") => match rest.split_first() {
            Some((cmd, args)) => return run::exec(&cmd.to_string_lossy(), args.to_vec()),
            None => Err(format!("run: which command?\n{USAGE}")),
        },
        Some("--version" | "-V") => {
            println!("drupe {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        None | Some("--help" | "-h") => {
            println!("drupe {}\n\n{USAGE}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(format!("unknown command `{other}`\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("drupe: {why}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("drupe: Linux only");
    std::process::ExitCode::FAILURE
}
