//! Shared CLI arguments and execution for VM controls.

use std::path::PathBuf;
use std::time::Duration;

use crate::{Button, WaitCond, attached::Attached, mcp};
use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use ilex_devkit::Workspace;

#[derive(Args)]
pub struct Target {
    /// Worktree slug (must match the current checkout for vm run)
    #[arg(long, global = true, env = "ILEX_VM_SLUG")]
    pub slug: Option<String>,
    /// VM to attach to or purpose to boot (e.g. vm, gui, graphical)
    #[arg(long, visible_alias = "vm", global = true, env = "ILEX_VM")]
    pub name: Option<String>,
}

impl Target {
    pub fn run(self, cmd: Cmd) -> Result<()> {
        run(cmd, self.slug.as_deref(), self.name.as_deref())
    }

    pub fn serve(self) -> Result<()> {
        serve(self.slug.as_deref(), self.name)
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List the running VMs of the checkout
    List,
    /// Inspect the actual guest compositor mode (native development SSH)
    Display,
    /// Exercise live QMP and two fresh captures; never boots a VM
    SelfCheck,
    /// Show this VM's actual screenshot, input and lifecycle capabilities
    Capabilities {
        #[arg(long)]
        json: bool,
    },
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
        #[arg(required_unless_present = "stdin", conflicts_with = "stdin")]
        text: Option<String>,
        /// Read private text from stdin and withhold screenshots until guest acknowledgement
        #[arg(long)]
        stdin: bool,
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
}

fn workspace(slug: Option<&str>) -> Result<Workspace> {
    match slug {
        Some(slug) => Ok(Workspace {
            root: PathBuf::new(),
            slug: slug.to_owned(),
            worktree: None,
        }),
        None => Workspace::discover()
            .context("pass --slug (see `cargo ilexctl info`) or run inside the checkout"),
    }
}

pub fn serve(slug: Option<&str>, vm: Option<String>) -> Result<()> {
    // Missing checkout context is an error for tools that need it, and does
    // not prevent the stdio server from starting.
    mcp::serve(workspace(slug).ok(), vm)
}

pub fn run(cmd: Cmd, slug: Option<&str>, vm: Option<&str>) -> Result<()> {
    let ws = workspace(slug)?;
    if let Cmd::List = cmd {
        let vms = Attached::list(&ws)?;
        if vms.is_empty() {
            println!("no VM running for `{}`", ws.slug);
        }
        for v in vms {
            println!("{v}");
        }
        return Ok(());
    }
    let vm = Attached::attach(&ws, vm)?;
    match cmd {
        Cmd::List => unreachable!(),
        Cmd::Display => {
            println!("{}", serde_json::to_string_pretty(&vm.display()?)?);
        }
        Cmd::SelfCheck => {
            let first = vm.capture()?;
            std::thread::sleep(Duration::from_millis(100));
            let second = vm.capture()?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"schema":1,"vm":vm.handle(),"qmp":vm.status()?,"first_timestamp_ms":first.captured_unix_ms,"second_timestamp_ms":second.captured_unix_ms,"size":[second.width,second.height],"generation":second.generation,"coordinate_space":second.coordinate_space,"fresh":second.captured_unix_ms > first.captured_unix_ms,"capabilities":vm.capabilities()?})
                )?
            );
            anyhow::ensure!(
                second.captured_unix_ms > first.captured_unix_ms,
                "capture timestamps did not advance"
            );
        }
        Cmd::Capabilities { json } => {
            let capabilities = vm.capabilities()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&capabilities)?);
            } else {
                println!(
                    "{}: {}",
                    vm.name(),
                    serde_json::to_string_pretty(&capabilities)?
                );
            }
        }
        Cmd::Screenshot { out } => {
            let frame = vm.save_screenshot(&out)?;
            println!("{} ({}x{})", out.display(), frame.width, frame.height);
        }
        Cmd::Click {
            x,
            y,
            button,
            double,
        } => vm.click(x, y, Button::parse(&button)?, if double { 2 } else { 1 })?,
        Cmd::Move { x, y } => vm.move_to(x, y)?,
        Cmd::Scroll { x, y, amount, up } => vm.scroll(x, y, up, amount)?,
        Cmd::Type { text, stdin, enter } => {
            if stdin {
                use std::io::{IsTerminal, Read};
                let stream = std::io::stdin();
                anyhow::ensure!(!stream.is_terminal(), "supply private input through a pipe");
                let mut value = String::new();
                stream
                    .take(129)
                    .read_to_string(&mut value)
                    .map_err(|_| anyhow::anyhow!("reading private keyboard input failed"))?;
                anyhow::ensure!(value.len() <= 128, "private keyboard input too long");
                vm.type_sensitive(&value)?;
            } else {
                vm.type_text(&text.context("missing keyboard input")?)?;
            }
            if enter {
                vm.key("Return")?;
            }
        }
        Cmd::Key { combo } => vm.key(&combo)?,
        Cmd::WaitScreen {
            timeout,
            nonblank,
            changed,
            stable,
            size,
            out,
        } => {
            let size = size.as_deref().map(crate::parse_size).transpose()?;
            let cond = WaitCond {
                nonblank,
                changed,
                stable: stable.map(Duration::from_secs_f64),
                size,
            };
            let started = std::time::Instant::now();
            let (png, frame) = vm.wait_screen(&cond, Duration::from_secs_f64(timeout))?;
            if let Some(out) = out {
                std::fs::write(&out, &png)?;
            }
            println!(
                "screen ready after {:.1}s ({}x{})",
                started.elapsed().as_secs_f64(),
                frame.width,
                frame.height
            );
        }
    }
    Ok(())
}
