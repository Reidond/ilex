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
mod scratch;
#[cfg(target_os = "linux")]
mod selfcheck;
#[cfg(target_os = "linux")]
mod sys;
#[cfg(target_os = "linux")]
mod topology;

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
        /// The composition table; its edition record is `edition.json` in
        /// the same directory
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
        /// Look for the UEFI firmware's directory here instead of
        /// /sys/firmware/efi (the `installer::` tier's fixtures)
        #[arg(long, hide = true)]
        efi_dir: Option<PathBuf>,
        /// Internal: mount an EFI system partition read-only at the directory,
        /// print its loader directories as JSON and unmount it. The helper
        /// starts this inside `unshare --mount`.
        #[arg(long, hide = true, num_args = 2, value_names = ["DEVICE", "DIR"])]
        scan_esp: Option<Vec<PathBuf>>,
    }
    let args = Args::parse();
    if let Some(scan) = args.scan_esp {
        let result = disks::scan_esp_child(&scan[0].to_string_lossy(), &scan[1]);
        println!("{}", serde_json::to_string(&result)?);
        return Ok(());
    }
    let mut config = helper::Config::default();
    if let Some(table) = args.table {
        // The edition record is written beside the table: a table somewhere
        // else brings its own, not the installed image's.
        config.edition_file = table.with_file_name("edition.json");
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
    if let Some(efi_dir) = args.efi_dir {
        config.efi_dir = efi_dir;
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
