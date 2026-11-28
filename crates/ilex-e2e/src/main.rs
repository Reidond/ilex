//! ilex end-to-end tests. There are no unit tests: every test boots the
//! image in QEMU and asserts over SSH.
//!
//! Tiers are selected with the standard libtest filter, e.g.
//! `cargo ilexctl test ephemeral::`; several filters select their union
//! (`cargo ilexctl test ephemeral:: upgrade::`). The image under test comes in
//! through `ILEX_IMAGE` (default: this checkout's `localhost/ilex:<tag>`).

#[cfg(target_os = "linux")]
use std::panic::{self, AssertUnwindSafe};

#[cfg(any(target_os = "linux", target_os = "windows"))]
use libtest_mimic::{Arguments, Trial};

#[cfg(target_os = "linux")]
mod apps;
#[cfg(target_os = "linux")]
mod common;
// Fixtures preserve Unix modes and symlinks; Windows runs these tiers on the builder.
#[cfg(target_os = "linux")]
mod config;
#[cfg(target_os = "linux")]
mod disk;
#[cfg(target_os = "linux")]
mod ephemeral;
#[cfg(target_os = "linux")]
mod graphical;
#[cfg(target_os = "linux")]
mod homed;
#[cfg(target_os = "linux")]
mod installer;
#[cfg(target_os = "linux")]
mod iso;
#[cfg(target_os = "linux")]
mod iso_next;
#[cfg(target_os = "linux")]
mod security;
#[cfg(target_os = "windows")]
mod security_windows;
#[cfg(target_os = "linux")]
mod encrypted;
#[cfg(target_os = "linux")]
mod selinux;
#[cfg(target_os = "linux")]
mod tool;
#[cfg(target_os = "linux")]
mod upgrade;

#[cfg(target_os = "windows")]
fn main() {
    let (filters, rest) = split_filters(std::env::args().skip(1));
    let args = Arguments::from_iter(std::iter::once("e2e".to_string()).chain(rest));
    let trials = [
        security_windows::trial(),
        security_windows::transition_trial(),
    ]
    .into_iter()
    .filter(|trial| filters.iter().any(|f| matches(&args, trial, f)))
    .collect();
    libtest_mimic::run(&args, trials).exit();
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn main() {
    eprintln!("ilex e2e requires Linux or Windows");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    let (filters, rest) = split_filters(std::env::args().skip(1));
    let args = Arguments::from_iter(std::iter::once("e2e".to_string()).chain(rest));

    let mut trials = Vec::new();
    trials.extend(ephemeral::trials());
    trials.extend(disk::trials());
    trials.extend(upgrade::trials());
    trials.extend(graphical::trials());
    if filters.iter().any(|f| f.contains("apps::")) {
        trials.extend(apps::trials());
    }
    if filters.iter().any(|f| f.contains("graphical::apps")) {
        trials.push(graphical::apps_trial());
    }
    // One image with two shells, one home: needs `--shell gnome --shell kde`.
    if filters.iter().any(|f| f.contains("graphical::shells")) {
        trials.push(graphical::shells_trial());
    }
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
    // The live installer's ISOs have trials of their own, named explicitly:
    // `iso::` alone must not boot an ISO it was not given.
    if filters.iter().any(|f| f.contains("iso::next")) {
        trials.extend(iso_next::trials());
    }
    // The KDE edition's live ISO (a second ISO, `ILEX_ISO_NEXT_KDE`): only a
    // filter that names `iso::next_kde` boots it.
    if filters.iter().any(|f| f.contains("iso::next_kde")) {
        trials.extend(iso_next::kde_trials());
    }
    // The ISO as a file on another disk needs a host tool or two more and a
    // second copy of the ISO: only a filter that names it asks for it.
    if filters.iter().any(|f| f.contains("next_helper_loop")) {
        trials.push(iso_next::loop_trial());
    }
    if filters.iter().any(|f| f.contains("next_unattended_loop")) {
        trials.push(iso_next::unattended_loop_trial());
    }
    // The next installer's root helper: a VM of its live image, no ISO.
    if filters.iter().any(|f| f.contains("installer::")) {
        trials.extend(installer::trials());
    }
    // The KDE edition's interface checks boot the KDE live image in a VM of
    // their own: only a filter that names them.
    if filters.iter().any(|f| f.contains("ui_kde")) {
        trials.extend(installer::kde_trials());
    }
    // Security is opt-in, but an explicitly selected unqualified host fails.
    if filters.iter().any(|f| f.contains("security")) {
        trials.extend(security::trials());
        #[cfg(target_os = "linux")]
        trials.extend(encrypted::trials());
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
    ephemeral::expect(will_run("ephemeral::"));
    apps::expect(will_run("apps::"));
    // `tool::installed_system` boots a disk of its own, not the tier's VM.
    tool::expect(will_run("tool::") - will_run("tool::installed_system"));
    // `installer::policy`, `::install`, `::encrypted`, `::encrypted_german` and
    // `::interrupted` write disks in VMs of their own.
    installer::expect(
        will_run("installer::")
            - will_run("installer::policy")
            - will_run("installer::install")
            - will_run("installer::encrypted")
            - will_run("installer::interrupted"),
    );

    let outcome = panic::catch_unwind(AssertUnwindSafe(|| libtest_mimic::run(&args, trials)));
    // Safety net: nothing may outlive the run.
    ephemeral::shutdown();
    apps::shutdown();
    tool::shutdown();
    installer::shutdown();
    match outcome {
        Ok(conclusion) => conclusion.exit(),
        Err(panic) => panic::resume_unwind(panic),
    }
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn matches(args: &Arguments, trial: &Trial, filter: &str) -> bool {
    if args.exact {
        trial.name() == filter
    } else {
        trial.name().contains(filter)
    }
}

/// Split the command line into positional name filters and everything else
/// (flags and their values), which goes to libtest-mimic.
#[cfg(any(target_os = "linux", target_os = "windows"))]
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
