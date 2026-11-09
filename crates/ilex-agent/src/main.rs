//! `ilex-agent`: the on-machine ilex binary shipped in the image.
//!
//! There are no unit tests in this repo: anything worth testing has to be
//! observable from inside a booted VM, so the e2e tests call
//! `ilex-agent --self-check` over SSH and read its exit code and output.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("ilex-agent {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--self-check") => self_check(),
        None | Some("--help" | "-h") => {
            println!("ilex-agent {VERSION}\n\nUsage: ilex-agent [--self-check | --version]");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("ilex-agent: unknown argument `{other}` (try --help)");
            ExitCode::from(2)
        }
    }
}

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
    ("default target is graphical", check_graphical_default),
    ("desktop packages installed", check_desktop_binaries),
    ("bootc is installed", || in_path("bootc")),
];

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
        println!("ilex-agent self-check: ok");
        ExitCode::SUCCESS
    } else {
        println!("ilex-agent self-check: {failed} failed");
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

/// `systemctl set-default graphical.target` leaves this symlink behind.
fn check_graphical_default() -> Result<(), String> {
    let target = fs::read_link("/etc/systemd/system/default.target").map_err(|e| e.to_string())?;
    if target.ends_with("graphical.target") {
        Ok(())
    } else {
        Err(format!("default.target points to {}", target.display()))
    }
}

/// The GNOME session, its display manager, the browser and Flatpak.
fn check_desktop_binaries() -> Result<(), String> {
    let missing: Vec<&str> = ["gnome-shell", "gdm", "firefox", "flatpak"]
        .into_iter()
        .filter(|bin| in_path(bin).is_err())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("missing: {}", missing.join(", ")))
    }
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
