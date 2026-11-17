#[cfg(target_os = "linux")]
mod console;

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
            #[arg(long, default_value = "/dev/tty6")]
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
    }
    let args = Args::parse();
    harden()?;
    if args.self_check {
        return console::self_check();
    }
    let value = match args
        .command
        .ok_or_else(|| anyhow::anyhow!("a subcommand is required"))?
    {
        Cmd::Probe { .. } => serde_json::to_value(probe())?,
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
            serde_json::to_value(recovery(
                &Device::open(&device)?,
                duplicate(unlock_fd)?,
                duplicate(key_out_fd)?,
            )?)?
        }
        Cmd::Tpm {
            device,
            unlock_fd,
            target_root,
            ..
        } => serde_json::to_value(tpm(
            &Device::open(&device)?,
            duplicate(unlock_fd)?,
            &tool_versions(&target_root)?,
        )?)?,
        Cmd::Layout { root } => serde_json::to_value(discover(&root)?)?,
        Cmd::Complete {
            root,
            tty,
            reissue_recovery,
        } => return console::complete(&root, &tty, reissue_recovery),
        Cmd::Initrd { args } => {
            let status = initrd::attach(&args)?;
            std::process::exit(status.code().unwrap_or(1));
        }
        Cmd::RecordRoot { root } => return initrd::record_root(&root),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
