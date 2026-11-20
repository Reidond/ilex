use std::process::ExitCode;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Verify this boot's encrypted root unlock method
    Verify,
    /// Repair TPM enrollment after fallback, using the local console
    Repair,
}

pub fn run(args: Args) -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        let result = match args.command {
            Command::Verify => ilex_enroll::verify(),
            Command::Repair => ilex_enroll::local::repair(),
        };
        match result {
            Ok(result) => {
                println!("{result:#}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("ilexctl security: {error:#}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = args.command;
        ExitCode::FAILURE
    }
}
