//! Machine operations for `ilexctl`, with no standalone agent binary.
//!
//! There are no unit tests in this repo: anything worth testing has to be
//! observable from inside a booted VM, so the e2e tests call
//! `ilexctl system --self-check` over SSH and read its exit code and output.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

mod apps;
mod config;
pub use apps::{AppsArgs, UpdatesArgs, run as apps, updates};
mod homed;
mod sigpolicy;

mod cli;
pub use cli::{HomeArgs, ProfileArgs, SystemArgs, home, profile, system};

type Check = (&'static str, fn() -> Result<(), String>);

/// Everything the image promises that can be seen from inside a running
/// guest. One line per check, `ok`/`FAIL`, non-zero exit if anything failed.
const CHECKS: &[Check] = &[
    ("os-release is Fedora", check_os_release),
    ("ilex install config present", || {
        exists("/usr/lib/bootc/install/00-ilex.toml")
    }),
    ("install config selects btrfs", check_install_btrfs),
    ("ilex kernel arguments present", || {
        exists("/usr/lib/bootc/kargs.d/10-ilex.toml")
    }),
    ("image configuration readable", config::check_image),
    ("system app manifests and timer", apps::check),
    ("public user catalogue", || {
        if run_profile(["check"]) == ExitCode::SUCCESS {
            Ok(())
        } else {
            Err("profile catalogue validation failed".into())
        }
    }),
    ("default target matches the shells", config::check_target),
    ("every shell has its session", config::check_sessions),
    (
        "default shell's greeter is the display manager",
        config::check_greeter,
    ),
    ("base packages installed", config::check_base_packages),
    ("shell packages installed", config::check_shell_packages),
    ("machine configuration readable", config::check_machine),
    ("machine packages installed", config::check_packages),
    ("bootc is installed", || in_path("bootc")),
    ("sigpolicy decisions", sigpolicy::check_decisions),
    (
        "sigpolicy default supports enforcement",
        sigpolicy::check_policy,
    ),
    ("sigpolicy unit enabled", || {
        exists("/etc/systemd/system/multi-user.target.wants/ilex-sigpolicy.service")
    }),
    ("ilexctl is not setuid", check_ilexctl),
    ("ilexctl completions present", check_completions),
    ("drupe helper is setuid root", check_drupe_helper),
    ("drupe mount point /app present", || exists("/app")),
    ("drupe unit enabled", || {
        exists("/etc/systemd/system/multi-user.target.wants/drupe.service")
    }),
];

fn run_profile(args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> ExitCode {
    match std::process::Command::new("/usr/bin/python3")
        .args(["-I", "-S", "/usr/libexec/ilex/profile.py"])
        .args(args)
        .status()
    {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("ilexctl: error: profile: {error}");
            ExitCode::FAILURE
        }
    }
}

fn self_check() -> ExitCode {
    let mut failed = 0;
    for (name, check) in CHECKS {
        match check() {
            Ok(()) => println!("ok   {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed += 1;
            }
        }
    }
    if failed == 0 {
        println!("ilexctl system self-check: ok");
        ExitCode::SUCCESS
    } else {
        println!("ilexctl system self-check: {failed} failed");
        ExitCode::FAILURE
    }
}

fn check_os_release() -> Result<(), String> {
    let text = fs::read_to_string("/usr/lib/os-release").map_err(|e| e.to_string())?;
    if text.lines().any(|l| l == "ID=fedora") {
        Ok(())
    } else {
        Err("ID=fedora not found in /usr/lib/os-release".into())
    }
}

fn check_install_btrfs() -> Result<(), String> {
    let text =
        fs::read_to_string("/usr/lib/bootc/install/00-ilex.toml").map_err(|e| e.to_string())?;
    if text.lines().any(|l| l.replace(' ', "") == "type=\"btrfs\"") {
        Ok(())
    } else {
        Err("no `type = \"btrfs\"` in 00-ilex.toml".into())
    }
}

/// `drupe-enter` makes the mount namespace a tool runs in, which takes root:
/// without the setuid bit no tool command starts. That the tools then run
/// is the `tool::` tier's business.
fn check_drupe_helper() -> Result<(), String> {
    const HELPER: &str = "/usr/libexec/drupe/drupe-enter";
    let meta = fs::metadata(HELPER).map_err(|e| format!("{HELPER}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.uid() != 0 || meta.mode() & 0o4000 == 0 {
            return Err(format!(
                "{HELPER} is not setuid root (uid {}, mode {:o})",
                meta.uid(),
                meta.mode() & 0o7777
            ));
        }
    }
    let _ = meta;
    Ok(())
}

fn check_ilexctl() -> Result<(), String> {
    const BINARY: &str = "/usr/bin/ilexctl";
    let meta = fs::metadata(BINARY).map_err(|e| format!("{BINARY}: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !meta.is_file() || meta.uid() != 0 || meta.mode() & 0o6000 != 0 {
            return Err(format!(
                "{BINARY} must be a regular root-owned binary without setuid/setgid"
            ));
        }
    }
    let _ = meta;
    if Path::new("/usr/bin/ilex-agent").exists() {
        return Err("obsolete /usr/bin/ilex-agent is still installed".into());
    }
    Ok(())
}

fn check_completions() -> Result<(), String> {
    for path in [
        "/usr/share/bash-completion/completions/ilexctl",
        "/usr/share/zsh/site-functions/_ilexctl",
        "/usr/share/fish/vendor_completions.d/ilexctl.fish",
    ] {
        let data = fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
        if !data.contains("ilexctl") {
            return Err(format!("{path}: empty or invalid completion"));
        }
    }
    Ok(())
}

fn exists(path: &str) -> Result<(), String> {
    if Path::new(path).exists() {
        Ok(())
    } else {
        Err(format!("{path} missing"))
    }
}

fn in_path(bin: &str) -> Result<(), String> {
    for dir in ["/usr/bin", "/usr/sbin", "/bin", "/sbin"] {
        if Path::new(dir).join(bin).exists() {
            return Ok(());
        }
    }
    Err(format!("{bin} not found in /usr/bin or /usr/sbin"))
}
