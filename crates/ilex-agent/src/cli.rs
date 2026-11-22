//! Arguments and dispatch for the machine groups composed by ilexctl.

use std::process::ExitCode;

use crate::{config, homed, run_profile, self_check, sigpolicy};

/// Inspect the installed image and its signature policy.
#[derive(clap::Args)]
pub struct SystemArgs {
    #[command(subcommand)]
    command: SystemCommand,
}

#[derive(clap::Subcommand)]
enum SystemCommand {
    /// Check the image's configuration and installed runtime
    #[command(long_flag = "self-check")]
    Check,
    /// Show the image and machine configuration
    Config {
        /// Print one schema-versioned JSON document
        #[arg(long)]
        json: bool,
    },
    /// Require signed updates for an ilex registry origin
    EnforceSigpolicy {
        /// Report the action without changing the origin
        #[arg(long)]
        dry_run: bool,
    },
}

pub fn system(args: SystemArgs) -> ExitCode {
    match args.command {
        SystemCommand::Check => self_check(),
        SystemCommand::Config { json } => config::run(json),
        SystemCommand::EnforceSigpolicy { dry_run } => sigpolicy::run(dry_run),
    }
}

/// Local password-encrypted homes; homectl owns password prompting.
#[derive(clap::Args)]
pub struct HomeArgs {
    #[command(subcommand)]
    command: HomeCommand,
}

#[derive(clap::Subcommand)]
enum HomeCommand {
    /// Create a daily home, keeping the local maintenance account separate
    Create(homed::CreateArgs),
    /// Create or resume the daily home selected in the installer
    Setup(homed::SetupArgs),
    /// Initialize a new homed filesystem before user@ starts
    #[command(hide = true)]
    Label { uid: u32 },
}

pub fn home(args: HomeArgs) -> ExitCode {
    match args.command {
        HomeCommand::Create(args) => homed::create(args),
        HomeCommand::Setup(args) => homed::setup(args),
        HomeCommand::Label { uid } => homed::label(uid),
    }
}

/// The profile implementation remains an internal Python helper.
#[derive(clap::Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    command: ProfileCommand,
}

#[derive(clap::Subcommand)]
enum ProfileCommand {
    /// List the public profiles installed in this image
    List,
    /// Validate the installed public catalogue
    #[command(long_flag = "self-check")]
    Check,
    /// Apply a profile after authenticating into your daily home
    Apply {
        /// Installed profile ID
        #[arg(required_unless_present = "selected", conflicts_with = "selected")]
        id: Option<String>,
        /// Reapply the previously selected profile (no selection is a no-op)
        #[arg(long)]
        selected: bool,
        /// Installed shell whose settings to apply
        #[arg(long, value_parser = ["gnome"])]
        shell: Option<String>,
        /// Configure your Flathub remote and install/update user apps
        #[arg(long)]
        install_apps: bool,
    },
    /// List backup sizes; close editors before explicitly deleting backups
    PruneBackups {
        /// Retain the newest N backup applies (default: zero)
        #[arg(long, default_value_t = 0, value_name = "N")]
        keep: u32,
        /// Delete listed backups; without this flag, only report a dry run
        #[arg(long)]
        execute: bool,
    },
}

pub fn profile(args: ProfileArgs) -> ExitCode {
    let mut argv = Vec::new();
    match args.command {
        ProfileCommand::List => argv.push("list".into()),
        ProfileCommand::Check => argv.push("check".into()),
        ProfileCommand::Apply {
            id,
            selected,
            shell,
            install_apps,
        } => {
            argv.push("apply".into());
            if selected {
                argv.push("--selected".into());
            }
            if let Some(shell) = shell {
                argv.push(format!("--shell={shell}"));
            }
            if install_apps {
                argv.push("--install-apps".into());
            }
            if let Some(id) = id {
                // A positional ID after the public `--` must not become an
                // option (such as --help/--selected) in the Python parser.
                argv.extend(["--".into(), id]);
            }
        }
        ProfileCommand::PruneBackups { keep, execute } => {
            argv.extend(["prune-backups".into(), format!("--keep={keep}")]);
            if execute {
                argv.push("--execute".into());
            }
        }
    }
    run_profile(argv)
}
