//! Shared command tree. Machine operations live in the ilex-agent library;
//! K2 will compose the dev-host libraries behind the `dev` feature.

use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};

#[cfg(feature = "dev")]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (dev)");
#[cfg(not(feature = "dev"))]
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (machine)");

#[derive(Parser)]
#[command(name = "ilexctl", version = VERSION, about = "Manage an ilex machine", help_template = "{about}\n\n{usage-heading} {usage}\n\nOn an ilex machine:\n  system   Inspect the image and enforce its signature policy\n  home     Create password-encrypted homes\n  profile  Apply public user profiles\n  apps     Reconcile system Flatpaks\n  updates  List OS and Flatpak updates\n\nBoth builds:\n  completion  Generate a shell completion script\n\n{options}")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect the image and enforce its signature policy (on an ilex machine)
    System(ilex_agent::SystemArgs),
    /// Create password-encrypted homes (on an ilex machine)
    Home(ilex_agent::HomeArgs),
    /// Apply public user profiles (on an ilex machine)
    Profile(ilex_agent::ProfileArgs),
    /// Manage the image's system Flatpaks (on an ilex machine)
    Apps(ilex_agent::AppsArgs),
    /// List OS and system Flatpak updates (on an ilex machine)
    Updates(ilex_agent::UpdatesArgs),
    /// Generate a static completion script for a shell
    Completion { shell: clap_complete::Shell },
}

pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code() as u8;
            if error.use_stderr() {
                let message = error.to_string();
                eprint!(
                    "ilexctl: error: {}",
                    message.strip_prefix("error: ").unwrap_or(&message)
                );
            } else {
                let _ = error.print();
            }
            return ExitCode::from(code);
        }
    };
    if let Command::Completion { shell } = cli.command {
        clap_complete::generate(
            shell,
            &mut Cli::command(),
            "ilexctl",
            &mut std::io::stdout(),
        );
        return ExitCode::SUCCESS;
    }
    if !cfg!(target_os = "linux") {
        eprintln!("ilexctl: error: machine commands require an ilex machine running Linux");
        return ExitCode::FAILURE;
    }
    // On an ilex dev host, its installed image owns machine behaviour. Avoid
    // recursion when this executable is already the image's command.
    #[cfg(all(feature = "dev", target_os = "linux"))]
    {
        let machine = std::path::Path::new("/usr/bin/ilexctl");
        if machine.is_file()
            && std::env::current_exe()
                .ok()
                .and_then(|p| p.canonicalize().ok())
                != machine.canonicalize().ok()
        {
            return match std::process::Command::new(machine)
                .args(std::env::args_os().skip(1))
                .status()
            {
                Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
                Err(error) => {
                    eprintln!("ilexctl: error: starting {}: {error}", machine.display());
                    ExitCode::FAILURE
                }
            };
        }
    }
    match cli.command {
        Command::System(args) => ilex_agent::system(args),
        Command::Home(args) => ilex_agent::home(args),
        Command::Profile(args) => ilex_agent::profile(args),
        Command::Apps(args) => ilex_agent::apps(args),
        Command::Updates(args) => ilex_agent::updates(args),
        Command::Completion { .. } => unreachable!(),
    }
}
