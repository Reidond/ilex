//! The dev command tree and the installed binary's checkout launcher.

use std::ffi::OsString;
use std::fmt::Write;
use std::path::Path;
use std::process::{Command as Process, ExitCode};

use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use ilex_devkit::Workspace;
use ilex_vmctl::cli::{Cmd as Control, Target};
use xtask::{CheckoutCmd, Cmd, EntryPoint};

pub const IN_CHECKOUT: &str = "--ilexctl-in-checkout";

#[derive(Subcommand)]
pub enum Command {
    #[command(flatten)]
    Checkout(CheckoutCmd),
    /// Boot an image or drive a running VM
    Vm(VmArgs),
    /// Inspect or check the checkout's tooling, security qualification and app manifests
    Doctor(DoctorArgs),
    /// Build drupe tool images from Fedora packages
    Drupe {
        #[command(subcommand)]
        command: Drupe,
    },
    /// Serve the VM tools as an MCP server on stdio
    Mcp(Target),
}

#[derive(Args)]
pub struct VmArgs {
    #[command(flatten)]
    target: Target,
    #[command(subcommand)]
    command: Vm,
}

#[derive(Subcommand)]
enum Vm {
    /// Boot an interactive ephemeral VM or the installed disk
    Run(xtask::VmArgs),
    #[command(flatten)]
    Control(Control),
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct DoctorArgs {
    #[command(flatten)]
    args: xtask::DoctorArgs,
    #[command(subcommand)]
    command: Option<Doctor>,
}

#[derive(Subcommand)]
enum Doctor {
    /// Probe secure firmware and check or measure guest qualification
    Security(xtask::SecurityArgs),
    /// Check configured Flatpak IDs and trust metadata without installing apps
    Apps,
}

#[derive(Subcommand)]
pub enum Drupe {
    /// Build a signed drupe tool image from Fedora packages
    Build(xtask::ToolArgs),
}

impl Command {
    pub fn run(self) -> Result<()> {
        let command = match self {
            Self::Checkout(cmd) => Cmd::Checkout(cmd),
            Self::Vm(vm) => match vm.command {
                Vm::Run(mut args) => {
                    if let Some(slug) = vm.target.slug {
                        let ws = Workspace::discover()?;
                        ensure!(
                            slug == ws.slug,
                            "vm run: --slug {slug} does not match this checkout ({})",
                            ws.slug
                        );
                    }
                    if let Some(name) = vm.target.name {
                        ensure!(
                            args.purpose.as_ref().is_none_or(|purpose| purpose == &name),
                            "vm run: --name and --purpose name different VMs"
                        );
                        args.purpose = Some(name);
                    }
                    Cmd::Vm(args)
                }
                Vm::Control(cmd) => return vm.target.run(cmd),
            },
            Self::Doctor(doctor) => match doctor.command {
                None => Cmd::Doctor(doctor.args),
                Some(Doctor::Security(args)) => Cmd::Security(args),
                Some(Doctor::Apps) => Cmd::Apps(xtask::AppsArgs { check: true }),
            },
            Self::Drupe {
                command: Drupe::Build(args),
            } => Cmd::Tool(args),
            Self::Mcp(target) => return target.serve(),
        };
        xtask::run(command, EntryPoint::Ilexctl)
    }
}

pub fn finish(result: Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("ilexctl: error: {err:#}");
            ExitCode::from(xtask::exit_code(&err))
        }
    }
}

pub fn configure(command: clap::Command, version_requested: bool) -> clap::Command {
    let mut help = String::from("{about}\n\n{usage-heading} {usage}\n");
    for heading in ["On an ilex machine", "On a dev host", "Both builds"] {
        write!(help, "\n{heading}:\n").unwrap();
        for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
            let name = sub.get_name();
            let section = if name == "completion" {
                "Both builds"
            } else if Command::has_subcommand(name) {
                "On a dev host"
            } else {
                "On an ilex machine"
            };
            if section == heading {
                let about = sub.get_about().map(ToString::to_string).unwrap_or_default();
                let about = about
                    .strip_suffix(" (on an ilex machine)")
                    .unwrap_or(&about);
                writeln!(help, "  {name:16} {about}").unwrap();
            }
        }
    }
    help.push_str("\n{options}");

    let mut command = command.about("Develop and manage ilex").help_template(help);
    if version_requested && let Ok(ws) = Workspace::discover() {
        command = command.version(format!("{} (dev, {})", env!("CARGO_PKG_VERSION"), ws.slug));
    }
    command
}

/// Forward before parsing dev arguments so the checkout supplies its own
/// help, argument definitions and validation even with an older launcher.
pub fn launch(args: &[OsString]) -> Option<Result<ExitCode>> {
    let path = args.strip_prefix(&[OsString::from("help")]).unwrap_or(args);
    let (first, rest) = path.split_first()?;
    let name = first.to_str()?;
    let dev_command = Command::has_subcommand(name);
    match name {
        "vm" if !vm_run_requested(rest) => return None,
        "mcp" | "completion" => return None,
        _ if name.starts_with('-') || (!dev_command && super::Command::has_subcommand(name)) => {
            return None;
        }
        _ => {}
    }

    let root = match Workspace::checkout_root() {
        Ok(Some(root)) => Ok(root),
        // These commands have standalone entry points, as in xtask.
        Ok(None)
            if !dev_command
                || matches!(name, "builder" | "runtime" | "identity" | "remote-worker") =>
        {
            return None;
        }
        Ok(None) => Workspace::require_checkout_root(),
        Err(err) => Err(err),
    };
    Some(root.and_then(|root| run_checkout(&root, args)))
}

fn vm_run_requested(mut args: &[OsString]) -> bool {
    while let Some((first, rest)) = args.split_first() {
        match first.to_str() {
            Some("--slug" | "--name" | "--vm") => args = rest.get(1..).unwrap_or_default(),
            Some(arg)
                if ["--slug=", "--name=", "--vm="]
                    .iter()
                    .any(|prefix| arg.starts_with(prefix)) =>
            {
                args = rest;
            }
            Some("help") => return rest.first().is_some_and(|arg| arg == "run"),
            Some("run") => return true,
            _ => return false,
        }
    }
    false
}

fn run_checkout(root: &Path, args: &[OsString]) -> Result<ExitCode> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut command = Process::new(cargo);
    command
        .args(["run", "-q", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .args(["-p", "ilexctl", "--", IN_CHECKOUT])
        .args(args);
    let context = || format!("starting Cargo for {}", root.display());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).with_context(context)
    }
    #[cfg(not(unix))]
    {
        let status = command.status().with_context(context)?;
        Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
    }
}
