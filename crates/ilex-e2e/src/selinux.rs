//! The live installer boots SELinux enforcing, and no trial of it may end with
//! a denial.
//!
//! Every `iso::next…` trial boots the real ISO, and the disk-booted
//! `installer::` trials boot the live image written to a disk; each is
//! checked here, at the end of the trial, with the image's own report
//! (`/usr/libexec/ilex-installer/selinux-report`, which a person can run on
//! tty2 too): the mode is `Enforcing`, the kernel command line does not say
//! otherwise (`enforcing=0`, `selinux=0`), PID 1 runs in `init_t` (labels
//! exist and the policy is loaded before the first transition) and the audit
//! log of this boot holds no AVC, USER_AVC or SELINUX_ERR record. A failure
//! prints every denial, and the report is kept as `<trial>-selinux.txt`
//! beside the other artifacts.
//!
//! There is nothing to allow-list: a denial is a bug of the image (a wrong
//! label, a missing rule in `ilex-installer.cil`) or of the test, and the fix
//! is in one of them. `dontaudit` rules would hide the records this reads and
//! are not used.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_vmctl::Vmctl;

/// The image's report. Run as root in the guest.
pub(crate) const REPORT: &str = "/usr/libexec/ilex-installer/selinux-report";
/// First word of the summary line and of each denial's line.
pub(crate) const SUMMARY: &str = "ILEX-INSTALLER-SELINUX";
const RECORD: &str = "ILEX-INSTALLER-SELINUX-AVC";

/// What the report printed.
#[derive(Debug)]
pub(crate) struct Report {
    mode: String,
    policy: String,
    pid1: String,
    cmdline: String,
    denials: String,
    records: Vec<String>,
}

impl Report {
    /// The report in `text` (the last summary line wins: a guest that was
    /// asked twice printed it twice).
    pub(crate) fn parse(text: &str) -> Result<Self> {
        let summary = text
            .lines()
            .rev()
            .find(|line| line.split(' ').next() == Some(SUMMARY))
            .with_context(|| format!("the SELinux report has no {SUMMARY} line:\n{text}"))?;
        let value = |key: &str| -> Result<String> {
            summary
                .split(' ')
                .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
                .map(str::to_string)
                .with_context(|| format!("the SELinux summary has no {key}: {summary}"))
        };
        let records = text
            .lines()
            .filter_map(|line| line.strip_prefix(RECORD)?.strip_prefix(' '))
            .map(str::to_string)
            .collect();
        Ok(Self {
            mode: value("mode")?,
            policy: value("policy")?,
            pid1: value("pid1")?,
            cmdline: value("cmdline")?,
            denials: value("denials")?,
            records,
        })
    }

    /// How many denials the guest counted.
    pub(crate) fn denial_count(&self) -> Option<usize> {
        self.denials.parse().ok()
    }

    /// Everything that is not what an enforcing, labeled live system shows.
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.mode != "Enforcing" {
            problems.push(format!("getenforce says {:?}, not Enforcing", self.mode));
        }
        if self.cmdline.contains("enforcing=0") || self.cmdline.contains("selinux=0") {
            problems.push(format!(
                "the kernel command line turns SELinux down: {}",
                self.cmdline
            ));
        }
        if self.pid1 != "init_t" {
            problems.push(format!(
                "PID 1 runs as {:?}, not init_t (no labels, or no policy at boot)",
                self.pid1
            ));
        }
        if self.policy != "targeted" {
            problems.push(format!("the policy is {:?}, not targeted", self.policy));
        }
        match self.denial_count() {
            Some(0) => {}
            Some(n) => problems.push(format!("{n} SELinux denial(s)")),
            None => problems.push(format!(
                "the guest could not count its denials ({})",
                self.denials
            )),
        }
        problems
    }
}

/// Parse `text`, keep it as `<artifacts>/<name>-selinux.txt` and fail with the
/// denials unless the guest was enforcing and clean.
pub(crate) fn assert_report(text: &str, artifacts: &Path, name: &str) -> Result<()> {
    let _ = std::fs::write(artifacts.join(format!("{name}-selinux.txt")), text);
    let report = Report::parse(text)?;
    let problems = report.problems();
    if problems.is_empty() {
        eprintln!(
            "{name}: SELinux Enforcing ({}), PID 1 {}, 0 denials",
            report.policy, report.pid1
        );
        return Ok(());
    }
    bail!(
        "{name}: {} (report: {name}-selinux.txt)\n{}",
        problems.join("; "),
        report.records.join("\n")
    )
}

/// [`check`] for a guest whose login may not be up yet.
pub(crate) fn check_waiting(
    vm: &mut ilex_devkit::qemu::Vm,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    vm.wait_ssh(Duration::from_secs(180)).with_context(|| {
        format!("{name}: the harness's SSH key did not give a login to the live ISO")
    })?;
    check(vm, artifacts, name)
}

/// The check from a console log: the report is what the guest printed of its
/// own accord (`ilex-installerd --answers` ends with it) or what was typed at
/// tty2's shell with its output on the second UART.
pub(crate) fn assert_console(console: &str, artifacts: &Path, name: &str) -> Result<()> {
    let text: Vec<&str> = console
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .filter(|line| matches!(line.split(' ').next(), Some(SUMMARY | RECORD)))
        .collect();
    assert_report(&text.join("\n"), artifacts, name)
}

/// The check at the end of a trial with an SSH login to the guest.
pub(crate) fn check(vm: &ilex_devkit::qemu::Vm, artifacts: &Path, name: &str) -> Result<()> {
    let text = vm
        .ssh_ok(REPORT)
        .with_context(|| format!("{name}: running {REPORT}"))?;
    assert_report(&text, artifacts, name)
}

/// The check for a guest that runs no SSH server (the serial trials that boot
/// without the harness's key): a person's way to the report. Ctrl+Alt+F2 from
/// the page reaches tty2's root shell, the command is typed there with its
/// output on the second UART (`/dev/ttyS1`, which the serial faults do not
/// touch) and read from that port's log. The shell starts when the VT is first
/// visited, so characters typed too early are lost: the command is typed again
/// until the report comes.
pub(crate) fn check_tty2(
    ctl: &Vmctl,
    secondary: &Path,
    artifacts: &Path,
    name: &str,
) -> Result<()> {
    ctl.key("ctrl+alt+F2")?;
    std::thread::sleep(Duration::from_secs(5));
    // A line still being written is not a line yet.
    let summary = |text: &str| {
        text.split_inclusive('\n')
            .any(|l| l.ends_with('\n') && l.split(' ').next() == Some(SUMMARY))
    };
    for attempt in 1..=4 {
        ctl.type_text(&format!("{REPORT} > /dev/ttyS1\n"))?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(25) {
            let text = std::fs::read(secondary)
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default();
            if summary(&text) {
                // The denials follow the summary in one burst.
                std::thread::sleep(Duration::from_secs(3));
                let text = std::fs::read(secondary)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
                return assert_console(&text, artifacts, name);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        eprintln!(
            "{name}: no SELinux report on the second UART after typing it (attempt {attempt})"
        );
    }
    bail!("{name}: tty2's shell never printed the SELinux report on the second UART")
}

/// A control for the check itself: provoke one denial and require the report
/// to count it, so that a report that always says 0 cannot pass. `runcon` into
/// a domain whose entrypoint `/usr/bin/true` is not (httpd_t) is refused by
/// the policy, which logs one AVC (the process is the shell's own, not a
/// service's). It is the last thing the trial does: the denial stays in the
/// guest's log.
pub(crate) fn control(vm: &ilex_devkit::qemu::Vm, artifacts: &Path, name: &str) -> Result<()> {
    let before = Report::parse(&vm.ssh_ok(REPORT)?)?
        .denial_count()
        .context("the report cannot count denials")?;
    let ran = vm.ssh("runcon system_u:system_r:httpd_t:s0 /usr/bin/true");
    ensure!(
        ran.is_ok_and(|out| !out.status.success()),
        "the control's transition into httpd_t was not refused"
    );
    // The audit record reaches the journal a moment after the denial.
    let started = Instant::now();
    loop {
        let text = vm.ssh_ok(REPORT)?;
        let _ = std::fs::write(artifacts.join(format!("{name}-selinux.txt")), &text);
        let after = Report::parse(&text)?
            .denial_count()
            .context("the report cannot count denials")?;
        if after > before {
            eprintln!("{name}: the report counted the control's denial ({before} -> {after})");
            return Ok(());
        }
        ensure!(
            started.elapsed() < Duration::from_secs(30),
            "the report still says {after} denial(s) after one was provoked: the check cannot see denials"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
}
