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

/// One line per check, `ok`/`FAIL`, non-zero exit if anything failed.
fn self_check() -> ExitCode {
    let checks: &[(&str, fn() -> Result<(), String>)] = &[
        ("os-release is Fedora", check_os_release),
        ("ilex install config present", || {
            exists("/usr/lib/bootc/install/00-ilex.toml")
        }),
        ("ilex kernel arguments present", || exists("/usr/lib/bootc/kargs.d/10-ilex.toml")),
    ];
    let mut failed = 0;
    for (name, check) in checks {
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

fn exists(path: &str) -> Result<(), String> {
    if Path::new(path).exists() { Ok(()) } else { Err(format!("{path} missing")) }
}
