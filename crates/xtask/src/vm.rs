//! `cargo xtask vm`: boot this checkout's image as an interactive VM under
//! the machine-wide VM slot cap.
//!
//! Three modes:
//! - default: an ephemeral VM straight from the container image
//!   (`bcvk ephemeral run-ssh`, no disk); exiting the shell tears it down.
//! - `--disk`: the installed qcow2 from `cargo xtask disk`, booted headless
//!   with a 4K `virtio-vga` display exactly like the graphical test tier.
//!   Attach to it with `ilex-vmctl` (QMP) or a VNC viewer.
//! - `--gui`: the same plus a window to watch and use. When the host has a
//!   GPU the guest gets one too (virgl: `virtio-vga-gl`).
//!   - In WSL, the window is `ilex-viewer.exe`, a native Windows window:
//!     QEMU draws into a private Xvfb, [`ilex_devkit::display`] serves it to
//!     the viewer, and the viewer sends back the keyboard (the Windows key
//!     included), pointer and clipboard. `ilex-vmctl` screenshots come from
//!     the same frames.
//!   - Elsewhere, a GTK window on the desktop. The guest follows the
//!     window's size either way.
//!
//! The disk modes boot from a throwaway overlay. Windows `--persist` retains
//! that writable overlay; the earlier Linux path writes to its installed disk.
//!
//! On Windows, installed VMs run natively through `crate::vm_windows` with
//! SDL/WHPX and virgl. Ephemeral VMs retain the WSL path (`crate::wsl`).

#[cfg(target_os = "linux")]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Child;
use std::process::Command;
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use anyhow::Context;
use anyhow::{Result, bail};
#[cfg(target_os = "linux")]
use ilex_devkit::display::{Bridge, Xvfb};
#[cfg(target_os = "linux")]
use ilex_devkit::qemu::{self, Display, Vm, VmConfig};
use ilex_devkit::{Workspace, cmd, slots};

#[cfg(target_os = "linux")]
use crate::disk;

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(flatten)]
    pub selection: crate::select::Args,
    /// Image tag or reference to boot (defaults to this checkout's image)
    #[arg(long)]
    pub image: Option<String>,
    /// Print the image to boot (GUI inspects machine labels; no build or boot)
    #[arg(long)]
    pub print_image: bool,
    /// Boot the installed disk image (UEFI, 4K virtio-vga) instead of an ephemeral VM
    #[arg(long)]
    pub disk: bool,
    /// Use the qualified native Hyper-V Gen2 firmware and vTPM profile
    #[arg(long, conflicts_with_all = ["persist", "apps", "no_apps", "no_gl"])]
    pub secure: bool,
    /// Installed disk plus a native SDL/WHPX window on Windows (GTK on Linux)
    #[arg(long)]
    pub gui: bool,
    /// With --gui: no virgl, the guest renders on its CPUs (default: virgl when the host has a GPU)
    #[arg(long)]
    pub no_gl: bool,
    /// Enable the apps timer in a headless disk VM (Flathub downloads)
    #[arg(long, conflicts_with = "no_apps")]
    pub apps: bool,
    /// Disable the apps timer in a GUI VM
    #[arg(long)]
    pub no_apps: bool,
    /// With --disk/--gui: retain changes (Windows keeps a writable overlay and firmware)
    #[arg(long)]
    pub persist: bool,
    /// Guest memory, e.g. `4G` (default 4G ephemeral, 8G for --disk/--gui)
    #[arg(long)]
    pub memory: Option<String>,
    /// Guest vCPUs (default 4; for --gui half the host's threads, 4 to 8)
    #[arg(long)]
    pub vcpus: Option<u32>,
    /// Separate owned development fixture/overlay (default gui or vm)
    #[arg(long)]
    pub purpose: Option<String>,
    /// Command to run over SSH instead of an interactive shell
    #[arg(last = true)]
    pub command: Vec<String>,
}

impl Args {
    pub fn resolve(&self, ws: &Workspace) -> Result<crate::select::Resolved> {
        if let Some(purpose) = &self.purpose {
            ilex_devkit::core::validate_vm_purpose(purpose)?;
        }
        self.selection.resolve(ws, self.image.as_deref(), None)
    }

    pub fn apps_enabled(&self) -> bool {
        self.apps || (self.gui && !self.no_apps)
    }
}

/// Add the clipboard agent only to a generic image. An existing machine image
/// is used unchanged; composing machines requires an explicit future schema.
fn viewer_image(ws: &Workspace, resolved: &crate::select::Resolved, build: bool) -> Result<String> {
    resolved.require_match()?;
    let machine = ilex_devkit::machine::image_machine_label(&resolved.image)?;
    if machine.as_deref() == Some("generic") {
        if build {
            ilex_devkit::machine::build(
                ws,
                &resolved.image,
                &ws.root.join("machines/vm"),
                None,
                &[],
            )
        } else {
            ilex_devkit::machine::image_ref(ws, &resolved.image, "vm")
        }
    } else {
        if machine.as_deref() != Some("vm") {
            let description =
                machine.map_or_else(|| "unlabelled".into(), |name| format!("machine `{name}`"));
            eprintln!(
                "viewer clipboard needs machine `vm`; booting {description} image {} as it is",
                resolved.image
            );
        }
        Ok(resolved.image.clone())
    }
}

pub fn run(ws: &Workspace, args: Args, resolved: crate::select::Resolved) -> Result<()> {
    if args.print_image {
        #[cfg(target_os = "linux")]
        let viewer = args.gui && in_wsl();
        #[cfg(not(target_os = "linux"))]
        let viewer = args.gui && cfg!(windows);
        let image = if viewer {
            viewer_image(ws, &resolved, false)?
        } else {
            resolved.image
        };
        println!("{image}");
        return Ok(());
    }
    if args.disk || args.gui {
        #[cfg(target_os = "linux")]
        return run_disk(ws, args, resolved);
        #[cfg(not(target_os = "linux"))]
        bail!("--disk/--gui run in WSL2 (see CLAUDE.md)");
    }
    resolved.require_match()?;
    run_ephemeral(ws, args, resolved.image)
}

fn run_ephemeral(ws: &Workspace, args: Args, image: String) -> Result<()> {
    if args.persist {
        bail!("--persist only applies to --disk/--gui");
    }
    cmd::require("bcvk")?;
    let purpose = args.purpose.as_deref().unwrap_or("vm");
    let name = ws.vm_name(purpose);
    ws.ensure_state()?;
    ws.verify_implicit_image(&image)?;
    let _lease = ilex_devkit::ownership::vm_lease(ws, &name)?;
    let _slot = slots::acquire(&name)?;
    ilex_devkit::bcvk::require_absent_container(&name)?;

    let mut bcvk = Command::new("bcvk");
    bcvk.args(["ephemeral", "run-ssh"])
        .args(["--name", &name])
        .args(["--memory", args.memory.as_deref().unwrap_or("4G")])
        .args(["--vcpus", &args.vcpus.unwrap_or(4).to_string()])
        .arg(&image);
    if !args.command.is_empty() {
        bcvk.arg("--").args(&args.command);
    }
    cmd::run(&mut bcvk)
}

#[cfg(target_os = "linux")]
fn run_disk(ws: &Workspace, args: Args, resolved: crate::select::Resolved) -> Result<()> {
    let viewer = args.gui && in_wsl();
    // `ILEX_DISK` boots a ready-made disk (same override the e2e tiers honour).
    let disk = match std::env::var_os("ILEX_DISK").filter(|p| !p.is_empty()) {
        Some(path) => path.into(),
        None => {
            let image = if viewer {
                viewer_image(ws, &resolved, true)?
            } else {
                resolved.require_match()?;
                resolved.image
            };
            disk::ensure_image(ws, &disk::Args::default(), &image)?
        }
    };

    let xvfb = if viewer { Some(Xvfb::start()?) } else { None };

    let purpose = args
        .purpose
        .as_deref()
        .unwrap_or(if args.gui { "gui" } else { "vm" });
    let mut config = VmConfig::graphical(purpose, disk);
    config.snapshot = !args.persist;
    config.apps = args.apps_enabled();
    config.vcpus = args.vcpus.unwrap_or(if args.gui { gui_vcpus() } else { 4 });
    if let Some(memory) = &args.memory {
        config.memory = memory.clone();
    }
    config.display = match (args.gui, &xvfb) {
        (false, _) => Display::None,
        (true, Some(_)) => Display::Viewer,
        (true, None) => Display::Gtk,
    };
    config.x_display = xvfb.as_ref().map(|x| x.display.clone());
    config.gl = args.gui && !args.no_gl && qemu::host_gl();

    let mut vm = Vm::boot(ws, &config)?;
    let info = vm.info.clone();
    eprintln!(
        "{} booted ({}x{}, {} vCPUs, qemu pid {})",
        info.name, info.width, info.height, config.vcpus, info.pid
    );
    eprintln!("  ssh:    {}", info.ssh_command_line());
    if config.gl {
        eprintln!("  gpu:    virgl (the host renders the guest's OpenGL)");
    }
    match config.display {
        Display::Viewer => eprintln!(
            "  vmctl:  ilex-vmctl screenshot   (frames from the display bridge, input over QMP)"
        ),
        Display::Gtk if config.gl => eprintln!(
            "  vmctl:  input only (QMP {}): QEMU can't take screenshots of virgl output",
            info.qmp_sock.display()
        ),
        _ => {
            eprintln!(
                "  vmctl:  ilex-vmctl screenshot   (QMP {})",
                info.qmp_sock.display()
            );
            eprintln!("  vnc:    unix socket {}", info.vnc_sock.display());
        }
    }

    let _bridge;
    let mut viewer_proc = None;
    if let Some(x) = &xvfb {
        let bridge = Bridge::start(x, &info.dir).context("starting the display bridge")?;
        let ep = &bridge.endpoint;
        let viewer_args = [
            "--connect".to_string(),
            ep.addr.to_string(),
            "--token".into(),
            ep.token.clone(),
            "--title".into(),
            info.name.clone(),
        ];
        // `cargo xtask vm` on Windows reads this line and starts the window.
        eprintln!("{VIEWER_LINE}{}", viewer_args.join(" "));
        if std::env::var_os("ILEX_VIEWER_ON_WINDOWS").is_none() {
            match viewer_exe(ws).map(|exe| spawn_viewer(&exe, &viewer_args)) {
                Some(Ok(child)) => viewer_proc = Some(child),
                Some(Err(e)) => {
                    eprintln!("  window: {e:#}; run `cargo xtask vm --gui` from Windows instead")
                }
                None => eprintln!(
                    "  window: run `cargo xtask vm --gui` from Windows, or there: ilex-viewer.exe {}",
                    viewer_args.join(" ")
                ),
            }
        }
        _bridge = bridge;
    }

    if args.command.is_empty() {
        eprintln!("  close the window or press Ctrl-C to stop the VM");
        let status = loop {
            if let Some(status) = vm.exited()? {
                break status;
            }
            if let Some(child) = &mut viewer_proc
                && let Some(status) = child.try_wait()?
            {
                // The window closed (and asked QEMU to quit): give it a
                // moment, then make sure.
                if !status.success() {
                    eprintln!("ilex-viewer exited with {status}");
                }
                thread::sleep(Duration::from_secs(2));
                if vm.exited()?.is_none() {
                    eprintln!("stopping the VM");
                }
                drop(vm);
                return Ok(());
            }
            thread::sleep(Duration::from_millis(200));
        };
        if let Some(mut child) = viewer_proc {
            let _ = child.kill();
        }
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

/// Is this Linux the WSL2 distro of a Windows machine?
#[cfg(target_os = "linux")]
fn in_wsl() -> bool {
    std::env::var_os("WSL_DISTRO_NAME").is_some()
        || Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

/// The stderr line that carries the viewer's arguments to `cargo xtask vm`
/// on Windows ([`crate::wsl`]).
pub const VIEWER_LINE: &str = "ilex-viewer: ";

/// `ilex-viewer.exe` in the checkout's Windows target directory.
#[cfg(target_os = "linux")]
fn viewer_exe(ws: &Workspace) -> Option<PathBuf> {
    ["debug", "release"]
        .map(|p| ws.root.join("target").join(p).join("ilex-viewer.exe"))
        .into_iter()
        .find(|p| p.is_file())
}

/// Start the Windows program from WSL through interop. (That needs the
/// execute bit, which this repo's WSL setup leaves off for `/mnt/c` files,
/// hence the Windows side starting it normally.)
#[cfg(target_os = "linux")]
fn spawn_viewer(exe: &Path, args: &[String]) -> Result<Child> {
    Command::new(exe)
        .args(args)
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))
}

/// Half the host's threads, at least 4 and at most 8: GNOME under virgl and
/// the window's frame copies want more than the tests' 4.
#[cfg(target_os = "linux")]
fn gui_vcpus() -> u32 {
    let threads = thread::available_parallelism().map_or(8, |n| n.get() as u32);
    (threads / 2).clamp(4, 8)
}
