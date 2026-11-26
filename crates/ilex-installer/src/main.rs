//! `ilex-installer`: the interface of ilex's own installer for the GNOME
//! edition: GTK 4 + libadwaita (relm4) over `ilex-installer-flow`, which holds
//! the pages, the rules and the helper's client. This crate holds only what is
//! GNOME's: the widgets, GNOME Kiosk's keyboard interface and the rendering of
//! the pages to PNG files.
//!
//! It runs as the unprivileged `ilex-installer` user under GNOME Kiosk (see
//! `installer/next/`) and talks to the root helper `ilex-installerd` over the
//! system bus.
//!
//! * no arguments: the installer, fullscreen; the real helper.
//! * `--windowed`: a normal window (development).
//! * `--simulate [SCENARIO]`: the helper's answers from the compiled-in JSON
//!   fixtures (`--list-scenarios`). Refused unless the kernel command line
//!   carries `ilex.installer.simulate` or `ILEX_INSTALLER_SIMULATE_ALLOW` is
//!   set: a released ISO cannot be talked into pretending.
//! * `--self-check`: the flow's rules on the fixtures, the keyboard catalogue
//!   and the edition record, without a display (the session unit runs it
//!   first). With `--render DIR` it also draws every page and error state
//!   off-screen to PNG files (`--dark` for the dark style); that needs a
//!   running compositor and prints `ILEX-INSTALLER-RENDER`.
//! * `--marker LINE...`: print marker lines the way the page does (journal,
//!   then the serial console through a bounded worker), for the session
//!   script. Several lines in one call are written by one process, which is how
//!   the tests meet a worker that is still stuck on the first.

mod identity;
mod serial;

#[cfg(all(target_os = "linux", feature = "gui"))]
mod gui;

use anyhow::{Context, Result, bail};
use ilex_installer_flow::keyboard::Sources;
use ilex_installer_flow::simulate;
use std::path::PathBuf;

const USAGE: &str = "usage: ilex-installer [--windowed] [--simulate [SCENARIO]] | --self-check [--render DIR [--dark]] | --list-scenarios | --marker LINE... | --version";

fn main() {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--self-check" => options.self_check = true,
            "--windowed" => options.fullscreen = false,
            "--dark" => options.dark = true,
            "--render" => match args.next() {
                Some(dir) => options.render = Some(PathBuf::from(dir)),
                None => usage(),
            },
            "--simulate" => {
                let scenario = match args.peek() {
                    Some(next) if !next.starts_with("--") => args.next(),
                    _ => None,
                };
                options.simulate = Some(scenario);
            }
            "--list-scenarios" => {
                for name in simulate::names() {
                    println!("{name}");
                }
                return;
            }
            "--marker" => {
                let lines: Vec<String> = args.collect();
                if lines.is_empty() {
                    usage();
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
            _ => usage(),
        }
    }
    if options.render.is_some() && !options.self_check {
        usage();
    }
    if let Err(error) = run(options) {
        eprintln!("ilex-installer: {error:#}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2);
}

struct Options {
    self_check: bool,
    fullscreen: bool,
    dark: bool,
    render: Option<PathBuf>,
    /// `Some(None)`: `--simulate` with the scenario to be chosen from the
    /// kernel command line or the default.
    simulate: Option<Option<String>>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            self_check: false,
            fullscreen: true,
            dark: false,
            render: None,
            simulate: None,
        }
    }
}

fn run(options: Options) -> Result<()> {
    let cmdline = simulate::cmdline();
    if options.self_check {
        return self_check(&options);
    }
    // `ilex.installer.simulate=<scenario>` on the kernel command line makes a
    // test boot of the ISO run the real session against the fixtures.
    let simulated = match (&options.simulate, simulate::scenario_from_cmdline(&cmdline)) {
        (Some(Some(name)), _) => Some(name.clone()),
        (Some(None), Some(name)) => Some(name.to_owned()),
        (Some(None), None) => Some(simulate::DEFAULT_SCENARIO.to_owned()),
        (None, Some(name)) => Some(name.to_owned()),
        (None, None) => None,
    };
    if simulated.is_some() && !simulate::allowed(&cmdline) {
        bail!(
            "--simulate is refused: the kernel command line has no {} and {} is not set",
            simulate::ALLOW_ARGUMENT,
            simulate::ALLOW_ENV
        );
    }
    window(&options, simulated)
}

#[cfg(all(target_os = "linux", feature = "gui"))]
fn window(options: &Options, simulated: Option<String>) -> Result<()> {
    gui::run(gui::Launch {
        fullscreen: options.fullscreen,
        simulate: simulated,
        edition: identity::read().ok().map(|e| e.edition),
    })
}

#[cfg(not(all(target_os = "linux", feature = "gui")))]
fn window(_options: &Options, _simulated: Option<String>) -> Result<()> {
    bail!("built without the `gui` feature")
}

/// Everything the interface needs that a display is not needed for, and with
/// `--render` the pages drawn to files. Printed as one JSON line; the session
/// unit runs the plain form before starting the compositor.
fn self_check(options: &Options) -> Result<()> {
    let flow = ilex_installer_flow::selfcheck::run().context("the flow's rules")?;
    let keyboard = ilex_installer_flow::selfcheck::system(&Sources::default());
    let Some(directory) = &options.render else {
        // The live image's own files: the edition record and the keyboard
        // catalogue must be there.
        let edition = identity::read()?;
        let keyboard = keyboard.context("the keyboard catalogue")?;
        println!(
            "{}",
            serde_json::json!({"edition": edition, "flow": flow, "keyboard": keyboard})
        );
        serial::marker("ILEX-INSTALLER-SELFCHECK ok");
        return Ok(());
    };
    // A catalogue the container lacks is reported, not fatal: the pages are
    // drawn from the fixtures' own sample.
    if let Err(error) = &keyboard {
        eprintln!("ilex-installer: no system keyboard catalogue: {error:#}");
    }
    render(directory, options.dark)
}

#[cfg(all(target_os = "linux", feature = "gui"))]
fn render(directory: &std::path::Path, dark: bool) -> Result<()> {
    gui::render::run(directory, dark)
}

#[cfg(not(all(target_os = "linux", feature = "gui")))]
fn render(_directory: &std::path::Path, _dark: bool) -> Result<()> {
    bail!("built without the `gui` feature")
}
