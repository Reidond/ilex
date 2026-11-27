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
mod fault;
#[cfg(target_os = "linux")]
mod finish;
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
mod stamp;
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
        /// While this file exists, Apply stops after its probe and before it
        /// claims the disk (the `installer::` tier's race fixture)
        #[arg(long, hide = true)]
        pause_before_hold: Option<PathBuf>,
        /// Install without an interface, from the answers on the volume
        /// labelled ILEX-ANSWERS, by calling this helper's own methods as an
        /// interface would
        #[arg(long)]
        answers: bool,
        /// With `--answers`: what to do when the installation succeeded
        #[arg(long, value_enum, default_value = "nothing", requires = "answers")]
        then: ThenArg,
        /// At boot, once udev has settled: leave a marker if a volume
        /// labelled ILEX-ANSWERS (or ILEXANSWERS) is here, which keeps the
        /// interface's session from starting
        #[arg(long, conflicts_with_all = ["answers", "self_check"])]
        answers_gate: bool,
        /// A volume with an answers label appeared (the device node): say that
        /// it is ignored unless it was here at boot
        #[arg(long, value_name = "DEVICE", conflicts_with_all = ["answers", "self_check"])]
        answers_late: Option<PathBuf>,
        /// Say that the systemd-level hand-over to the interface has run (from
        /// `ilex-installer-answers-handover.service`): one line on the
        /// console and the serial port
        #[arg(long, conflicts_with_all = ["answers", "self_check", "answers_gate"])]
        answers_handed_over: bool,
        /// With `--answers`: read the answers from this file instead of the
        /// volume (the `installer::` tier's fixtures)
        #[arg(long, hide = true, requires = "answers")]
        answers_file: Option<PathBuf>,
        /// Internal: mount an EFI system partition read-only at the directory,
        /// print its loader directories as JSON and unmount it. The helper
        /// starts this inside `unshare --mount`.
        #[arg(long, hide = true, num_args = 2, value_names = ["DEVICE", "DIR"])]
        scan_esp: Option<Vec<PathBuf>>,
        /// Internal: mount a stick's partition in a mount namespace of its own,
        /// write one log tarball to it and unmount (`Finish(save-log)`).
        #[arg(long, hide = true, num_args = 6, value_names = ["DEVICE", "FSTYPE", "DIR", "SOURCE", "NAME", "MAJOR:MINOR"])]
        write_log: Option<Vec<PathBuf>>,
    }
    #[derive(Clone, Copy, clap::ValueEnum)]
    enum ThenArg {
        Nothing,
        Poweroff,
    }
    let args = Args::parse();
    if args.answers_gate {
        return answers::gate();
    }
    if let Some(device) = &args.answers_late {
        return answers::late(device);
    }
    if args.answers_handed_over {
        answers::handed_over();
        return Ok(());
    }
    if args.answers {
        // (`answers::run` hardens the process itself, before it reads the
        // volume, so that a failure to do so hands the machine to the
        // interface like any other failure before `Apply`.)
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
    if let Some(write) = args.write_log {
        return finish::run_write_log(&write);
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
    config.pause_before_hold = args.pause_before_hold;
    if args.self_check {
        println!("ILEX-INSTALLERD-CHECK OK: {}", selfcheck::run(&config)?);
        return Ok(());
    }
    // The `installer::` tier's fault (the fixed credential
    // ilex.installer.fault=helper; it can only make the installer fail): the
    // helper never takes its bus name.
    if fault::requested("helper") {
        eprintln!("ilex-installerd: fault=helper (a test credential): not starting");
        std::process::exit(1);
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
