#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("ilex-enroll: {error:#}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ilex-enroll requires Linux; no TPM was accessed");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn run() -> anyhow::Result<()> {
    use clap::{Parser, Subcommand};
    use ilex_enroll::*;
    use std::path::PathBuf;
    #[derive(Parser)]
    #[command(about = "LUKS2 recovery and explicit PCR 7 enrollment")]
    struct Args {
        #[arg(long)]
        self_check: bool,
        #[command(subcommand)]
        command: Option<Cmd>,
    }
    #[derive(Subcommand)]
    enum Cmd {
        Probe {
            #[arg(long)]
            json: bool,
        },
        #[command(hide = true)]
        ToolManifest,
        Status {
            #[arg(long)]
            device: PathBuf,
            #[arg(long)]
            json: bool,
        },
        Recovery {
            #[arg(long)]
            device: PathBuf,
            #[arg(long)]
            unlock_fd: i32,
            #[arg(long)]
            key_out_fd: i32,
        },
        Tpm {
            #[arg(long)]
            device: PathBuf,
            #[arg(long)]
            unlock_fd: i32,
            #[arg(long, default_value = "pcr7-sha256", value_parser = ["pcr7-sha256"])]
            policy: String,
            #[arg(long)]
            target_root: PathBuf,
        },
        Layout {
            #[arg(long)]
            root: PathBuf,
        },
        /// Local installer completion. Never use a serial or remote terminal.
        Complete {
            #[arg(long, default_value = "/mnt/sysroot")]
            root: PathBuf,
            #[arg(long, default_value = "/dev/tty8")]
            tty: PathBuf,
            /// Explicitly issue another recovery key after interrupted display.
            #[arg(long)]
            reissue_recovery: bool,
        },
        #[command(hide = true)]
        Initrd {
            #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
            args: Vec<String>,
        },
        #[command(hide = true)]
        RecordRoot {
            #[arg(long, default_value = "/sysroot")]
            root: PathBuf,
        },
        #[command(hide = true)]
        ReportBoot,
    }
    let args = Args::parse();
    harden()?;
    if args.self_check {
        return local::self_check();
    }
    let value = match args
        .command
        .ok_or_else(|| anyhow::anyhow!("a subcommand is required"))?
    {
        Cmd::Probe { .. } => serde_json::to_value(probe())?,
        Cmd::ToolManifest => tool_manifest()?,
        Cmd::Status { device, .. } => status(&Device::open(&device)?)?,
        Cmd::Recovery {
            device,
            unlock_fd,
            key_out_fd,
        } => {
            anyhow::ensure!(
                unlock_fd != key_out_fd,
                "input/output descriptors must differ"
            );
            let unlock = inherited(unlock_fd)?;
            let output = inherited(key_out_fd)?;
            serde_json::to_value(recovery(&Device::open(&device)?, unlock, output)?)?
        }
        Cmd::Tpm {
            device,
            unlock_fd,
            target_root,
            ..
        } => {
            let layout = discover(&target_root)?
                .ok_or_else(|| anyhow::anyhow!("TPM enrollment requires an encrypted root"))?;
            anyhow::ensure!(
                layout.device == device,
                "TPM enrollment device differs from the active encrypted root"
            );
            let unlock = Secret::read(inherited(unlock_fd)?)?;
            let device = Device::open(&device)?;
            let (_, _, reason) = eligibility(&target_root);
            anyhow::ensure!(
                reason == "pcr7-sha256",
                "TPM enrollment unavailable: {reason}"
            );
            let state = SecurityState::open(&target_root, &device)?;
            serde_json::to_value(
                Reconciled::open(&device, &unlock, state, true)?.enroll_tpm(false)?,
            )?
        }
        Cmd::Layout { root } => serde_json::to_value(discover(&root)?)?,
        Cmd::Complete {
            root,
            tty,
            reissue_recovery,
        } => return local::complete(&root, &tty, reissue_recovery),
        Cmd::Initrd { args } => {
            let status = initrd::attach(&args)?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::RecordRoot { root } => return initrd::record_root(&root),
        Cmd::ReportBoot => return local::report_boot(),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

#[cfg(target_os = "linux")]
fn inherited(fd: i32) -> anyhow::Result<std::os::fd::OwnedFd> {
    use std::os::fd::{FromRawFd, OwnedFd};
    anyhow::ensure!(fd >= 3, "credential descriptors must be >= 3");
    // SAFETY: this CLI owns the explicitly supplied descriptor. Mark the
    // original close-on-exec before any probe/status subprocess can inherit it.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    anyhow::ensure!(flags >= 0, "invalid credential descriptor");
    anyhow::ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == 0,
        "cannot protect credential descriptor"
    );
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
