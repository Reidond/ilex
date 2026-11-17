//! Typed command boundary shared by the Windows sender and Linux worker.
use anyhow::Result;
use ilex_devkit::{Workspace, core::Snapshot};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg(target_os = "windows")]
mod client;
#[cfg(target_os = "windows")]
mod source;

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
    SelfCheck { hold_seconds: u64 },
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
pub struct Failure { pub code: u8, pub message: String }
impl std::fmt::Display for Failure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { formatter.write_str(&self.message) }
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
    /// Reconnect, stream logs and retrieve outputs for an interrupted client
    Resume {
        #[arg(long)]
        job: Option<String>,
    },
    /// Cancel only this checkout's selected remote job, then retrieve evidence
    Cancel {
        #[arg(long)]
        job: Option<String>,
    },
    /// Verify staged source and real nested KVM userspace; optionally hold for Ctrl-C
    SelfCheck {
        #[arg(long, default_value_t = 0)]
        hold_seconds: u64,
    },
}

pub fn route(ws: &Workspace, cmd: crate::Cmd) -> Result<Option<crate::Cmd>> {
    #[cfg(target_os = "windows")]
    {
        let remote = match cmd {
            crate::Cmd::Build(a) => Command::Build(a),
            crate::Cmd::Rechunk(a) => Command::Rechunk(a),
            crate::Cmd::Disk(a) => Command::Disk(a),
            crate::Cmd::Iso(a) if !a.due => Command::Iso(a),
            crate::Cmd::Tool(a) => Command::Tool(a),
            crate::Cmd::Test(a) => Command::Test(a),
            other => return Ok(Some(other)),
        };
        client::execute(ws, remote)?;
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
