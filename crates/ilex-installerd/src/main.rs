//! ilex-installerd: the installer's root helper.

#[cfg(target_os = "linux")]
mod answers;
#[cfg(target_os = "linux")]
mod apply;
#[cfg(target_os = "linux")]
mod bus;
#[cfg(target_os = "linux")]
mod disks;
#[cfg(target_os = "linux")]
mod exec;
#[cfg(target_os = "linux")]
mod helper;
#[cfg(target_os = "linux")]
mod install;
#[cfg(target_os = "linux")]
mod policy;
#[cfg(target_os = "linux")]
mod progress;
#[cfg(target_os = "linux")]
mod registry;
#[cfg(target_os = "linux")]
mod repart;
#[cfg(target_os = "linux")]
mod scratch;
#[cfg(target_os = "linux")]
mod secret;
#[cfg(target_os = "linux")]
mod selfcheck;
#[cfg(target_os = "linux")]
mod stagelog;
#[cfg(target_os = "linux")]
mod sys;
#[cfg(target_os = "linux")]
mod target;
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
        /// How long a plan is valid, in seconds (the `installer::` tier's
        /// fixture; the interface's contract is 15 minutes)
        #[arg(long, hide = true)]
        plan_valid_seconds: Option<u64>,
        /// Look for the UEFI firmware's directory here instead of
        /// /sys/firmware/efi (the `installer::` tier's fixtures)
        #[arg(long, hide = true)]
        efi_dir: Option<PathBuf>,
        /// Install without an interface, from the answers on the volume
        /// labelled ILEX-ANSWERS, by calling this helper's own methods as an
        /// interface would
        #[arg(long)]
        answers: bool,
        /// With `--answers`: what to do when the installation succeeded
        #[arg(long, value_enum, default_value = "nothing", requires = "answers")]
        then: ThenArg,
        /// With `--answers`: read the answers from this file instead of the
        /// volume (the `installer::` tier's fixtures)
        #[arg(long, hide = true, requires = "answers")]
        answers_file: Option<PathBuf>,
        /// Internal: mount an EFI system partition read-only at the directory,
        /// print its loader directories as JSON and unmount it. The helper
        /// starts this inside `unshare --mount`.
        #[arg(long, hide = true, num_args = 2, value_names = ["DEVICE", "DIR"])]
        scan_esp: Option<Vec<PathBuf>>,
    }
    #[derive(Clone, Copy, clap::ValueEnum)]
    enum ThenArg {
        Nothing,
        Poweroff,
    }
    let args = Args::parse();
    if args.answers {
        let options = answers::Options {
            run_dir: args
                .run_dir
                .clone()
                .unwrap_or_else(|| helper::Config::default().run_dir),
            file: args.answers_file.clone(),
            then: match args.then {
                ThenArg::Nothing => answers::Then::Nothing,
                ThenArg::Poweroff => answers::Then::PowerOff,
            },
        };
        return answers::run(&options);
    }
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
    if let Some(seconds) = args.plan_valid_seconds {
        config.plan_valid_seconds = seconds;
    }
    if args.self_check {
        println!("ILEX-INSTALLERD-CHECK OK: {}", selfcheck::run(&config)?);
        return Ok(());
    }
    // The helper holds secrets (an administrator's password) once SetSecret
    // has run: it never dumps core and cannot be traced.
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
