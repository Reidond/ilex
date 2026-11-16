//! ilex end-to-end tests. There are no unit tests: every test boots the
//! image in QEMU and asserts over SSH.
//!
//! Tiers are selected with the standard libtest filter, e.g.
//! `cargo xtask test ephemeral::`; several filters select their union
//! (`cargo xtask test ephemeral:: upgrade::`). The image under test comes in
//! through `ILEX_IMAGE` (default: this checkout's `localhost/ilex:<tag>`).

use std::panic::{self, AssertUnwindSafe};

use libtest_mimic::{Arguments, Trial};

mod common;
// Fixtures preserve Unix modes and symlinks; these tiers run in WSL on Windows.
#[cfg(target_os = "linux")]
mod config;
mod disk;
mod ephemeral;
mod graphical;
mod homed;
mod iso;
mod security;
mod tool;
mod upgrade;

fn main() {
    let (filters, rest) = split_filters(std::env::args().skip(1));
    let args = Arguments::from_iter(std::iter::once("e2e".to_string()).chain(rest));

    let mut trials = Vec::new();
    trials.extend(ephemeral::trials());
    trials.extend(disk::trials());
    trials.extend(upgrade::trials());
    trials.extend(graphical::trials());
    trials.extend(tool::trials());
    // Installed, enforcing SELinux homes are an explicit, slow qualification.
    if filters.iter().any(|f| f.contains("homed")) {
        trials.extend(homed::trials());
    }
    #[cfg(target_os = "linux")]
    if !std::env::var_os("CI").is_some_and(|v| !v.is_empty())
        && filters.iter().any(|f| f.contains("config"))
    {
        trials.extend(config::trials());
    }
    // Opt-in: builds an installer ISO and runs an install (tens of minutes),
    // so it only exists when a filter asks for it (`cargo xtask test iso::`).
    if filters.iter().any(|f| f.contains("iso")) {
        trials.extend(iso::trials());
    }
    // Security is opt-in, but an explicitly selected unqualified host fails.
    if filters.iter().any(|f| f.contains("security")) {
        trials.extend(security::trials());
    }

    // libtest-mimic takes a single filter; accept several and run the union.
    trials.retain(|t| filters.is_empty() || filters.iter().any(|f| matches(&args, t, f)));

    // Tell each group how many of its tests will actually run, so the last
    // one to finish can release the group's VM (and its VM slot).
    let will_run = |tier: &str| {
        trials
            .iter()
            .filter(|t| {
                t.name().starts_with(tier) && !args.is_filtered_out(t) && !args.is_ignored(t)
            })
            .count()
    };
    if !args.list
        && will_run("iso::") > 0
        && let Err(error) = iso::preflight()
    {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
    ephemeral::expect(will_run("ephemeral::"));
    // `tool::installed_system` boots a disk of its own, not the tier's VM.
    tool::expect(will_run("tool::") - will_run("tool::installed_system"));

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| libtest_mimic::run(&args, trials)));
    // Safety net: nothing may outlive the run.
    ephemeral::shutdown();
    tool::shutdown();
    match outcome {
        Ok(conclusion) => conclusion.exit(),
        Err(panic) => panic::resume_unwind(panic),
    }
}

fn matches(args: &Arguments, trial: &Trial, filter: &str) -> bool {
    if args.exact {
        trial.name() == filter
    } else {
        trial.name().contains(filter)
    }
}

/// Split the command line into positional name filters and everything else
/// (flags and their values), which goes to libtest-mimic.
fn split_filters(args: impl Iterator<Item = String>) -> (Vec<String>, Vec<String>) {
    const TAKES_VALUE: &[&str] = &[
        "--test-threads",
        "--logfile",
        "--skip",
        "--color",
        "--format",
        "-Z",
    ];
    let (mut filters, mut rest) = (Vec::new(), Vec::new());
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg.starts_with('-') {
            let takes_value = TAKES_VALUE.contains(&arg.as_str());
            rest.push(arg);
            if takes_value {
                rest.extend(args.next());
            }
        } else {
            filters.push(arg);
        }
    }
    (filters, rest)
}
