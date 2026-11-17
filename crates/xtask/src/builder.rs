use anyhow::Result;
#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    operation: Operation,
}
#[derive(clap::Subcommand)]
enum Operation {
    /// Idempotent bootstrap/start; fixed 16 GiB, eight vCPUs by default
    Up {
        #[arg(long)]
        memory_gib: Option<u32>,
        #[arg(long)]
        cpus: Option<u32>,
        /// Wait for the live host reserve instead of failing immediately
        #[arg(long, default_value_t = 0)]
        wait_for_memory_seconds: u32,
    },
    /// Read VM/journal state; does not start or probe SSH
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Graceful shutdown if idle, retaining disks and identity
    Down,
    /// Update packages with a powered-off recovery checkpoint and KVM smoke
    Update {
        /// Exercise rollback after the real guest update (returns failure after recovery)
        #[arg(long)]
        rollback_probe: bool,
        #[arg(long, default_value_t = 0)]
        wait_for_memory_seconds: u32,
    },
    /// Boot real nested KVM userspace and report the pinned identity/revision
    #[command(name = "self-check", long_flag = "self-check")]
    SelfCheck,
    /// Interrupt real host operations at recovery boundaries; retains the ready VM off
    RecoverySelfCheck {
        #[arg(long, default_value_t = 10800)]
        wait_for_memory_seconds: u32,
    },
    /// Exercise maintenance/checkpoint and failed-lookup recovery in an isolated off VM
    MaintenanceSelfCheck {
        #[arg(long, default_value_t = 10800)]
        wait_for_memory_seconds: u32,
    },
    /// Remove only journal-owned resources from an incomplete bootstrap
    CleanupIncomplete,
    /// Restore and smoke-test the exact retained recovery checkpoint
    Rollback {
        #[arg(long, default_value_t = 0)]
        wait_for_memory_seconds: u32,
    },
    /// Explicitly discard the retained update recovery checkpoint
    AcceptUpdate,
}
pub fn run(args: Args) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use ilex_tooling_windows::builder::{self, Operation as Op, Options};
        let mut options = Options::default();
        let op = match args.operation {
            Operation::Up {
                memory_gib,
                cpus,
                wait_for_memory_seconds,
            } => {
                options.memory_gib = memory_gib;
                options.cpus = cpus;
                options.wait_for_memory_seconds = wait_for_memory_seconds;
                Op::Up
            }
            Operation::Status { .. } => Op::Status,
            Operation::Down => Op::Down,
            Operation::Update {
                rollback_probe,
                wait_for_memory_seconds,
            } => {
                options.rollback_probe = rollback_probe;
                options.wait_for_memory_seconds = wait_for_memory_seconds;
                Op::Update
            }
            Operation::SelfCheck => Op::SelfCheck,
            Operation::RecoverySelfCheck {
                wait_for_memory_seconds,
            } => {
                options.wait_for_memory_seconds = wait_for_memory_seconds;
                Op::RecoverySelfCheck
            }
            Operation::MaintenanceSelfCheck {
                wait_for_memory_seconds,
            } => {
                options.wait_for_memory_seconds = wait_for_memory_seconds;
                Op::MaintenanceSelfCheck
            }
            Operation::CleanupIncomplete => Op::CleanupIncomplete,
            Operation::Rollback {
                wait_for_memory_seconds,
            } => {
                options.wait_for_memory_seconds = wait_for_memory_seconds;
                Op::Rollback
            }
            Operation::AcceptUpdate => Op::AcceptUpdate,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&builder::run(op, options)?)?
        );
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        ilex_tooling_windows::unsupported()
    }
}
