//! Typed command boundary shared by the Windows sender and Linux worker.
use anyhow::Result;
use ilex_devkit::{Workspace, core::Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg(target_os = "windows")]
mod client;
#[cfg(target_os = "windows")]
mod receive;
#[cfg(target_os = "windows")]
mod source;

#[cfg(target_os = "windows")]
pub fn development_disk(
    ws: &Workspace,
    selection: crate::select::Args,
    image: Option<String>,
) -> Result<std::path::PathBuf> {
    client::execute(
        ws,
        Command::Disk(crate::disk::Args {
            selection,
            image,
            development: true,
            ..Default::default()
        }),
    )?
    .ok_or_else(|| anyhow::anyhow!("development disk was not delivered"))
}

#[cfg(target_os = "windows")]
pub fn gen2_disk(
    ws: &Workspace,
    args: &crate::test::Args,
    image: Option<String>,
) -> Result<std::path::PathBuf> {
    client::execute(
        ws,
        Command::Disk(crate::disk::Args {
            selection: args.selection.clone(),
            image,
            gen2: Some(crate::disk::Gen2 {
                build: args.build,
                no_pull: args.no_pull,
            }),
            ..Default::default()
        }),
    )?
    .ok_or_else(|| anyhow::anyhow!("Gen2 disk was not delivered"))
}

#[cfg(target_os = "windows")]
pub fn import_kache(ws: &Workspace, archive: std::path::PathBuf, version: String) -> Result<()> {
    let sha256 = crate::disk::hash_file(&archive)?;
    client::execute(
        ws,
        Command::ImportKache {
            archive: Some(archive),
            version,
            sha256,
        },
    )?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "args",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Command {
    Build(crate::build::Args),
    Rechunk(crate::rechunk::Args),
    Disk(crate::disk::Args),
    Iso(crate::iso::Args),
    Tool(crate::tool::Args),
    Test(crate::test::Args),
    Vm(crate::vm::Args),
    ImportKache {
        #[serde(skip)]
        archive: Option<std::path::PathBuf>,
        version: String,
        sha256: String,
    },
    #[cfg(target_os = "windows")]
    Oemdrv {
        kickstart: String,
        public_key: String,
    },
    Export {
        image: Option<String>,
        output: Option<std::path::PathBuf>,
    },
    SelfCheck {
        hold_seconds: u64,
        #[serde(default)]
        allocation_image: Option<String>,
        #[serde(default)]
        fingerprint_image: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub schema: u32,
    pub job: String,
    pub snapshot: Snapshot,
    pub command: Command,
    pub environment: BTreeMap<String, String>,
    pub inputs: BTreeMap<String, Vec<String>>,
}

#[derive(Debug)]
pub struct Failure {
    pub code: u8,
    pub message: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub operation: Operation,
}
#[derive(clap::Subcommand)]
pub enum Operation {
    /// Inspect exact source chunks without starting the builder
    Snapshot,
    #[cfg(target_os = "windows")]
    #[command(hide = true)]
    Oemdrv {
        #[arg(long)]
        source: std::path::PathBuf,
    },
    /// Explicitly export this checkout's image to an OCI archive (no registry)
    Export {
        #[arg(long)]
        image: Option<String>,
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Reconnect, stream logs and retrieve outputs for an interrupted client
    Resume {
        #[arg(long)]
        job: Option<String>,
    },
    /// Request cancellation immediately; the original client or resume collects evidence
    Cancel {
        #[arg(long)]
        job: Option<String>,
    },
    /// Verify staged source and real nested KVM userspace; optionally hold for Ctrl-C
    #[command(name = "self-check", long_flag = "self-check")]
    SelfCheck {
        #[arg(long, default_value_t = 0)]
        hold_seconds: u64,
        /// Pause a real detached ephemeral allocation before its CID receipt
        #[arg(long)]
        allocation_image: Option<String>,
        /// Report actual root/rechunk fingerprints for a native/remote comparison
        #[arg(long)]
        fingerprint_image: Option<String>,
    },
    /// Exercise the Windows untrusted-artifact receiver without a builder
    SelfCheckReceiver,
}

pub fn route(ws: &Workspace, cmd: crate::Cmd) -> Result<Option<crate::Cmd>> {
    #[cfg(target_os = "windows")]
    {
        use crate::{CheckoutCmd, Cmd};

        let remote = match cmd {
            Cmd::Checkout(CheckoutCmd::Build(a)) => Command::Build(a),
            Cmd::Checkout(CheckoutCmd::Rechunk(a)) => Command::Rechunk(a),
            Cmd::Checkout(CheckoutCmd::Disk(a)) => Command::Disk(a),
            Cmd::Checkout(CheckoutCmd::Iso(a)) if !a.due => Command::Iso(a),
            Cmd::Tool(a) => Command::Tool(a),
            Cmd::Vm(a) if !a.disk && !a.gui && !a.secure && !a.print_image => Command::Vm(a),
            Cmd::Checkout(CheckoutCmd::Test(a))
                if a.filters.iter().any(|f| f.contains("security")) =>
            {
                let (legacy, native) = crate::security_windows::partition(a)?;
                // Both groups run once, even if the first reports a failure.
                let linux = legacy.map_or(Ok(()), |a| {
                    client::execute(ws, Command::Test(a)).map(|_| ())
                });
                let windows = crate::security_windows::test(ws, native);
                linux?;
                windows?;
                return Ok(None);
            }
            Cmd::Checkout(CheckoutCmd::Test(a)) => Command::Test(a),
            other => return Ok(Some(other)),
        };
        let _ = client::execute(ws, remote)?;
        Ok(None)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ws;
        Ok(Some(cmd))
    }
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        client::run(ws, args.operation)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (ws, args);
        anyhow::bail!("remote commands require Windows; Linux commands execute locally")
    }
}
