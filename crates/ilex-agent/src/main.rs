//! `ilex-agent`: the on-machine ilex binary shipped in the image.
//!
//! There are no unit tests in this repo: anything worth testing has to be
//! observable from inside a booted VM, so the e2e tests call
//! `ilex-agent --self-check` over SSH and read its exit code and output.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

mod config;
mod homed;
mod sigpolicy;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        Some("--version" | "-V") => {
            println!("ilex-agent {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--self-check") => self_check(),
        Some("home-create") => homed::create(std::env::args().skip(2).collect()),
        Some("config") => match std::env::args().nth(2).as_deref() {
            None => config::run(false),
            Some("--json") => config::run(true),
            Some(other) => {
                eprintln!("ilex-agent: config: unknown argument `{other}`");
                ExitCode::from(2)
            }
        },
        Some("enforce-sigpolicy") => match std::env::args().nth(2).as_deref() {
            None => sigpolicy::run(false),
            Some("--dry-run") => sigpolicy::run(true),
            Some(other) => {
                eprintln!("ilex-agent: enforce-sigpolicy: unknown argument `{other}`");
                ExitCode::from(2)
            }
        },
        None | Some("--help" | "-h") => {
            println!(
                "ilex-agent {VERSION}\n\n\
                 Usage: ilex-agent [--self-check | --version | config [--json] | enforce-sigpolicy [--dry-run]]\n\
                 \x20      ilex-agent home-create NAME --disk-size=GiB [--admin] [--real-name=NAME]"
            );
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
    ("image configuration readable", config::check_image),
    ("default target matches the shells", config::check_target),
    ("every shell has its session", config::check_sessions),
    (
        "default shell's greeter is the display manager",
        config::check_greeter,
    ),
    ("base packages installed", config::check_base_packages),
    ("shell packages installed", config::check_shell_packages),
    ("design systems have their tokens", config::check_tokens),
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
    ("drupe helper is setuid root", check_drupe_helper),
    ("drupe mount point /app present", || exists("/app")),
    ("drupe unit enabled", || {
        exists("/etc/systemd/system/multi-user.target.wants/drupe.service")
    }),
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
