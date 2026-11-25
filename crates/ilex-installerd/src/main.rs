//! ilex-installerd: the installer's root helper.

#[cfg(target_os = "linux")]
mod bus;
#[cfg(target_os = "linux")]
mod disks;
#[cfg(target_os = "linux")]
mod exec;
#[cfg(target_os = "linux")]
mod helper;
#[cfg(target_os = "linux")]
mod registry;
#[cfg(target_os = "linux")]
mod repart;
#[cfg(target_os = "linux")]
mod selfcheck;
#[cfg(target_os = "linux")]
mod sys;

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("ilex-installerd: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("ilex-installerd requires Linux; no disk was touched");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn run() -> anyhow::Result<()> {
    use clap::Parser;
    use std::{path::PathBuf, sync::Arc};

    #[derive(Parser)]
    #[command(
        about = "The ilex installer's root helper (org.ilex.Installer1)",
        version
    )]
    struct Args {
        /// Check the helper's rules and this system's tools, then exit
        #[arg(long)]
        self_check: bool,
        /// The composition table
        #[arg(long)]
        table: Option<PathBuf>,
        /// The edition (shell) this installer installs; default: the
        /// edition file beside the table, else the table's default shell
        #[arg(long)]
        edition: Option<String>,
        /// The repart definitions
        #[arg(long)]
        definitions: Option<PathBuf>,
        /// The helper's runtime directory
        #[arg(long)]
        run_dir: Option<PathBuf>,
        /// Read the SMBIOS identity from this directory instead of
        /// /sys/class/dmi/id (the `installer::` tier's fixtures)
        #[arg(long, hide = true)]
        dmi_dir: Option<PathBuf>,
        /// Read the kernel command line from this file instead of /proc/cmdline
        #[arg(long, hide = true)]
        cmdline: Option<PathBuf>,
    }
    let args = Args::parse();
    let mut config = helper::Config::default();
    if let Some(table) = args.table {
        config.table = table;
    }
    config.edition = args.edition;
    if let Some(definitions) = args.definitions {
        config.definitions = definitions;
    }
    if let Some(run_dir) = args.run_dir {
        config.run_dir = run_dir;
    }
    if let Some(dmi_dir) = args.dmi_dir {
        config.dmi_dir = dmi_dir;
    }
    if let Some(cmdline) = args.cmdline {
        config.cmdline = cmdline;
    }
    if args.self_check {
        println!("ILEX-INSTALLERD-CHECK OK: {}", selfcheck::run(&config)?);
        return Ok(());
    }
    // No secrets are held yet, but the helper never dumps core: Apply will.
    ilex_enroll::harden()?;
    let helper = Arc::new(helper::Helper::new(config)?);
    helper.publish_state();
    eprintln!(
        "ilex-installerd: edition {} offers {}",
        helper.edition.id(),
        helper.table.offered()
    );
    bus::serve(helper)
}
