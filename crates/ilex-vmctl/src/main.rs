//! `ilex-vmctl`: screenshots and input for ilex VMs, over QMP.
//!
//! Attaches to a VM that `cargo xtask vm --disk|--gui` (or the e2e tests)
//! booted for this checkout; never starts one. Pixel coordinates are
//! framebuffer pixels (3840x2160 for the graphical test machine).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilex_devkit::Workspace;
use ilex_devkit::qemu;
use ilex_vmctl::{Button, Vmctl, WaitCond, mcp};

#[derive(Parser)]
#[command(name = "ilex-vmctl", version, about = "Screenshots and input for ilex VMs (QMP)")]
struct Cli {
    /// Worktree slug of the VM to attach to (default: the checkout you are in; see `cargo xtask info`)
    #[arg(long, global = true, env = "ILEX_VM_SLUG")]
    slug: Option<String>,
    /// Which VM when several run for the slug (`gui`, `vm`, `graphical`, ...)
    #[arg(long, global = true, env = "ILEX_VM")]
    vm: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List the running VMs of the checkout
    List,
    /// Save a screenshot (PNG)
    Screenshot {
        /// Where to write it
        #[arg(short, long, default_value = "screenshot.png")]
        out: PathBuf,
    },
    /// Move the pointer to X Y (framebuffer pixels) and click
    Click {
        x: u32,
        y: u32,
        #[arg(long, default_value = "left")]
        button: String,
        /// Double click
        #[arg(long)]
        double: bool,
    },
    /// Move the pointer to X Y without clicking
    Move { x: u32, y: u32 },
    /// Scroll the wheel at X Y
    Scroll {
        x: u32,
        y: u32,
        /// Notches
        #[arg(long, default_value_t = 3)]
        amount: u32,
        /// Scroll up instead of down
        #[arg(long)]
        up: bool,
    },
    /// Type TEXT on a US keyboard
    Type {
        text: String,
        /// Press Return afterwards
        #[arg(long)]
        enter: bool,
    },
    /// Press a key or combination: Return, Escape, ctrl+alt+t, super, alt+F4
    Key { combo: String },
    /// Wait until the screen is not blank (default), changed, or stable
    WaitScreen {
        /// Give up after this many seconds
        #[arg(long, default_value_t = 120.0)]
        timeout: f64,
        /// Require a non-blank screen (the default when nothing else is asked)
        #[arg(long)]
        nonblank: bool,
        /// Require the picture to differ from the first one seen
        #[arg(long)]
        changed: bool,
        /// Require the picture to be unchanged for this many seconds
        #[arg(long)]
        stable: Option<f64>,
        /// Require the framebuffer to have this size, e.g. 3840x2160 (the
        /// first frames of a boot are drawn by firmware at another size)
        #[arg(long)]
        size: Option<String>,
        /// Also save the final screenshot here
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    /// Serve the same tools as an MCP server on stdio
    ///
    /// Register with: claude mcp add --transport stdio ilex-vm -- ilex-vmctl mcp
    Mcp,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ilex-vmctl: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn workspace(slug: &Option<String>) -> Result<Workspace> {
    match slug {
        Some(slug) => Ok(Workspace { root: PathBuf::new(), slug: slug.clone(), worktree: None }),
        None => Workspace::discover().context("pass --slug (see `cargo xtask info`) or run inside the checkout"),
    }
}

fn run(cli: Cli) -> Result<()> {
    if let Cmd::Mcp = cli.cmd {
        // Resolve lazily per call so a missing checkout is an error result,
        // not a server that never starts.
        let ws = workspace(&cli.slug).ok();
        return mcp::serve(ws, cli.vm);
    }
    let ws = workspace(&cli.slug)?;
    if let Cmd::List = cli.cmd {
        let vms = qemu::running_vms(&ws);
        if vms.is_empty() {
            println!("no VM running for `{}`", ws.slug);
        }
        for v in vms {
            println!("{}\t{}\tpid {}\t{}x{} {}\t{}", v.purpose, v.name, v.pid, v.width, v.height, v.display, v.ssh_command_line());
        }
        return Ok(());
    }
    let vm = Vmctl::attach(&ws, cli.vm.as_deref())?;
    match cli.cmd {
        Cmd::List | Cmd::Mcp => unreachable!(),
        Cmd::Screenshot { out } => {
            let frame = vm.save_screenshot(&out)?;
            println!("{} ({}x{})", out.display(), frame.width, frame.height);
        }
        Cmd::Click { x, y, button, double } => {
            vm.click(x, y, Button::parse(&button)?, if double { 2 } else { 1 })?
        }
        Cmd::Move { x, y } => vm.move_to(x, y)?,
        Cmd::Scroll { x, y, amount, up } => vm.scroll(x, y, up, amount)?,
        Cmd::Type { text, enter } => {
            vm.type_text(&text)?;
            if enter {
                vm.key("Return")?;
            }
        }
        Cmd::Key { combo } => vm.key(&combo)?,
        Cmd::WaitScreen { timeout, nonblank, changed, stable, size, out } => {
            let size = size.as_deref().map(ilex_vmctl::parse_size).transpose()?;
            let cond = WaitCond { nonblank, changed, stable: stable.map(Duration::from_secs_f64), size };
            let started = std::time::Instant::now();
            let (png, frame) = vm.wait_screen(&cond, Duration::from_secs_f64(timeout))?;
            if let Some(out) = out {
                std::fs::write(&out, &png)?;
            }
            println!("screen ready after {:.1}s ({}x{})", started.elapsed().as_secs_f64(), frame.width, frame.height);
        }
    }
    Ok(())
}
