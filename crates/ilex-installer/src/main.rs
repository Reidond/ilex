//! `ilex-installer`: the interface of ilex's own installer for the GNOME
//! edition. In this version it is a placeholder: one page naming the edition
//! and showing the machine's DMI identity. Nothing is installed.
//!
//! It runs as the unprivileged `ilex-installer` user under GNOME Kiosk (see
//! `installer/next/`). `--self-check` needs no display: it reads what the page
//! would show and fails when the live image is incomplete. `--marker LINE`
//! prints marker lines the way the page does (journal, then the serial
//! console through a bounded worker), for the session script. Several lines in
//! one call are written by one process, which is how the tests meet a worker
//! that is still stuck on the first.

mod identity;
mod serial;

#[cfg(all(target_os = "linux", feature = "gui"))]
mod window;

use anyhow::Result;

const USAGE: &str =
    "usage: ilex-installer [--self-check | --windowed | --marker LINE... | --version]";

fn main() {
    let mut mode = Mode::Window { fullscreen: true };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--self-check" => mode = Mode::SelfCheck,
            "--windowed" => mode = Mode::Window { fullscreen: false },
            "--marker" => {
                let lines: Vec<String> = args.collect();
                if lines.is_empty() {
                    eprintln!("{USAGE}");
                    std::process::exit(2);
                }
                for line in &lines {
                    serial::marker(line);
                }
                return;
            }
            "--version" => {
                println!("ilex-installer {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            _ => {
                eprintln!("{USAGE}");
                std::process::exit(2);
            }
        }
    }
    if let Err(error) = run(mode) {
        eprintln!("ilex-installer: {error:#}");
        std::process::exit(1);
    }
}

enum Mode {
    SelfCheck,
    Window { fullscreen: bool },
}

fn run(mode: Mode) -> Result<()> {
    match mode {
        Mode::SelfCheck => self_check(),
        Mode::Window { fullscreen } => {
            #[cfg(all(target_os = "linux", feature = "gui"))]
            {
                window::run(identity::Page::read()?, fullscreen)
            }
            #[cfg(not(all(target_os = "linux", feature = "gui")))]
            {
                let _ = fullscreen;
                anyhow::bail!("built without the `gui` feature")
            }
        }
    }
}

/// Everything the page needs, without a display. Printed as one JSON line; the
/// session unit runs it before starting the compositor.
fn self_check() -> Result<()> {
    let page = identity::Page::read()?;
    println!("{}", serde_json::to_string(&page)?);
    serial::marker("ILEX-INSTALLER-SELFCHECK ok");
    Ok(())
}
