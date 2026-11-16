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
    #[command(name = "self-check", alias = "--self-check")]
    SelfCheck,
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
        }
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = args;
        ilex_tooling_windows::unsupported()
    }
}
