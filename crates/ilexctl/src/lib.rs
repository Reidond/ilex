//! Machine commands and the optional development command tree.

use std::process::ExitCode;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

#[cfg(feature = "dev")]
mod dev;

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
    #[cfg(feature = "dev")]
    #[command(flatten)]
    Dev(dev::Command),
    /// Generate a static completion script for a shell
    Completion { shell: clap_complete::Shell },
}

pub fn run() -> ExitCode {
    let args: Vec<_> = std::env::args_os().collect();
    #[cfg(feature = "dev")]
    let args = {
        let mut args = args;
        if args.get(1).is_some_and(|arg| arg == dev::IN_CHECKOUT) {
            args.remove(1);
        } else if let Some(result) = dev::launch(&args[1..]) {
            return dev::finish(result);
        }
        args
    };
    let command = Cli::command();
    #[cfg(feature = "dev")]
    let command = dev::configure(
        command,
        args.get(1)
            .is_some_and(|arg| arg == "--version" || arg == "-V"),
    );
    let cli = match command
        .try_get_matches_from(&args)
        .and_then(|mut matches| Cli::from_arg_matches_mut(&mut matches))
    {
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
    #[cfg(feature = "dev")]
    if let Command::Dev(command) = cli.command {
        return dev::finish(command.run().map(|()| ExitCode::SUCCESS));
    }
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
                .args(&args[1..])
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
        #[cfg(feature = "dev")]
        Command::Dev(_) => unreachable!(),
        Command::Completion { .. } => unreachable!(),
    }
}
