use anyhow::Result;
#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    operation: Operation,
}
#[derive(clap::Subcommand)]
enum Operation {
    /// Fetch the reviewed lock into per-user state; verify before extraction
    Fetch,
    /// Read state without downloads, extraction, or activation
    Status,
    /// Fetch, boot a WHPX/SDL/GL firmware fixture, and atomically activate
    Update,
    /// Load real qemu-img and exercise corrupt/traversal/link rejection
    #[command(name = "self-check", long_flag = "self-check")]
    SelfCheck,
    /// Fail a first update with real SDL, verify no activation, then smoke/activate
    ActivationSelfCheck,
}
pub fn run(args: Args) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use ilex_tooling_windows::runtime;
        match args.operation {
            Operation::Fetch => println!("{}", runtime::fetch()?.display()),
            Operation::Status => println!("{}", serde_json::to_string_pretty(&runtime::status()?)?),
            Operation::Update => println!("{}", runtime::update()?.display()),
            Operation::SelfCheck => runtime::self_check()?,
            Operation::ActivationSelfCheck => println!(
                "{}",
                serde_json::to_string_pretty(&runtime::activation_self_check(
                    &std::env::current_exe()?
                )?)?
            ),
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        ilex_tooling_windows::unsupported()
    }
}
