//! `cargo xtask vm`: boot this checkout's image as an interactive VM under
//! the machine-wide VM slot cap.
//!
//! Three modes:
//! - default: an ephemeral VM straight from the container image
//!   (`bcvk ephemeral run-ssh`, no disk); exiting the shell tears it down.
//! - `--disk`: the installed qcow2 from `cargo xtask disk`, booted headless
//!   with a 4K `virtio-vga` display exactly like the graphical test tier.
//!   Attach to it with `ilex-vmctl` (QMP) or a VNC viewer.
//! - `--gui`: the same plus a GTK window to watch (on Windows, WSLg shows it
//!   on the desktop). The guest follows the window's size. When the host has
//!   a GPU the guest gets one too (virgl: `virtio-vga-gl`); QEMU then can't
//!   take screenshots, so agents use `--disk`.
//!
//! The disk modes boot from a throwaway overlay, so the disk file stays
//! pristine; `--persist` writes to it instead.
//!
//! On Windows, `cargo xtask vm` hands itself to WSL ([`crate::wsl`]), and
//! with `--gui` first makes sure WSLg can show the window.

use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use ilex_devkit::qemu::{self, Display, Vm, VmConfig};
use ilex_devkit::wslg::{self, State};
use ilex_devkit::{Workspace, cmd, slots};

use crate::disk;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Image tag or reference to boot (defaults to this checkout's image)
    #[arg(long)]
    pub image: Option<String>,
    /// Boot the installed disk image (UEFI, 4K virtio-vga) instead of an ephemeral VM
    #[arg(long)]
    pub disk: bool,
    /// Like --disk, plus a GTK window (shown through WSLg on Windows)
    #[arg(long)]
    pub gui: bool,
    /// With --gui: no virgl, the guest renders on its CPUs (default: virgl when the host has a GPU)
    #[arg(long)]
    pub no_gl: bool,
    /// With --disk/--gui: write changes to the disk instead of a throwaway overlay
    #[arg(long)]
    pub persist: bool,
    /// Guest memory, e.g. `4G` (default 4G ephemeral, 8G for --disk/--gui)
    #[arg(long)]
    pub memory: Option<String>,
    /// Guest vCPUs (default 4; for --gui half the host's threads, 4 to 8)
    #[arg(long)]
    pub vcpus: Option<u32>,
    /// On Windows with --gui: restart WSL for a working WSLg even when other work runs in it
    #[arg(long)]
    pub restart_wsl: bool,
    /// Command to run over SSH instead of an interactive shell
    #[arg(last = true)]
    pub command: Vec<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    if args.disk || args.gui {
        run_disk(ws, args)
    } else {
        run_ephemeral(ws, args)
    }
}

fn run_ephemeral(ws: &Workspace, args: Args) -> Result<()> {
    if args.persist {
        bail!("--persist only applies to --disk/--gui");
    }
    cmd::require("bcvk")?;
    let image = ws.resolve_image(args.image.as_deref());
    let name = ws.vm_name("vm");
    let _slot = slots::acquire(&name)?;

    let mut bcvk = Command::new("bcvk");
    bcvk.args(["ephemeral", "run-ssh"])
        .args(["--memory", args.memory.as_deref().unwrap_or("4G")])
        .args(["--vcpus", &args.vcpus.unwrap_or(4).to_string()])
        .arg(&image);
    if !args.command.is_empty() {
        bcvk.arg("--").args(&args.command);
    }
    cmd::run(&mut bcvk)
}

fn run_disk(ws: &Workspace, args: Args) -> Result<()> {
    if args.gui {
        check_wslg()?;
    }
    // `ILEX_DISK` boots a ready-made disk (same override the e2e tiers honour).
    let disk = match std::env::var_os("ILEX_DISK").filter(|p| !p.is_empty()) {
        Some(path) => path.into(),
        None => disk::ensure(ws, &disk::Args { image: args.image.clone(), ..disk::Args::default() })?,
    };

    let purpose = if args.gui { "gui" } else { "vm" };
    let mut config = VmConfig::graphical(purpose, disk);
    config.snapshot = !args.persist;
    config.vcpus = args.vcpus.unwrap_or(if args.gui { gui_vcpus() } else { 4 });
    if let Some(memory) = &args.memory {
        config.memory = memory.clone();
    }
    config.display = if args.gui { Display::Gtk } else { Display::None };
    config.gl = args.gui && !args.no_gl && qemu::host_gl();

    let mut vm = Vm::boot(ws, &config)?;
    let info = vm.info.clone();
    eprintln!("{} booted ({}x{}, {} vCPUs, qemu pid {})", info.name, info.width, info.height, config.vcpus, info.pid);
    eprintln!("  ssh:    {}", info.ssh_command_line());
    if config.gl {
        eprintln!("  gpu:    virgl (the host renders the guest's OpenGL)");
        eprintln!("  vmctl:  input only (QMP {}): QEMU can't take screenshots of virgl output", info.qmp_sock.display());
    } else {
        eprintln!("  vmctl:  ilex-vmctl screenshot   (QMP {})", info.qmp_sock.display());
        eprintln!("  vnc:    unix socket {}", info.vnc_sock.display());
    }

    if args.command.is_empty() {
        eprintln!("  close the window or press Ctrl-C to stop the VM");
        let status = vm.wait()?;
        eprintln!("qemu exited with {status}");
        return Ok(());
    }

    vm.wait_ssh(Duration::from_secs(300))?;
    let script = args.command.join(" ");
    let out = vm.ssh(&script)?;
    print!("{}", String::from_utf8_lossy(&out.stdout));
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("`{script}` failed with {}", out.status);
    }
    Ok(())
}

/// Half the host's threads, at least 4 and at most 8: GNOME under virgl and
/// the window's frame copies want more than the tests' 4.
fn gui_vcpus() -> u32 {
    let threads = thread::available_parallelism().map_or(8, |n| n.get() as u32);
    (threads / 2).clamp(4, 8)
}

/// Refuse to open a window WSLg would never draw. (From Windows, `xtask vm`
/// already restarted WSL until WSLg was healthy; see [`crate::wsl`].)
fn check_wslg() -> Result<()> {
    let started = Instant::now();
    loop {
        match wslg::local_state() {
            None | Some(State::Healthy) => return Ok(()),
            Some(State::Broken) => bail!("{}", wslg::BROKEN_HELP),
            // Weston logs its settings within a second or two of WSL's start.
            Some(State::Starting) if started.elapsed() < Duration::from_secs(20) => {
                thread::sleep(Duration::from_millis(500));
            }
            Some(State::Starting) => return Ok(()),
        }
    }
}
