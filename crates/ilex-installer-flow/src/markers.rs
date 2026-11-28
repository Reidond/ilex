//! The lines the installer prints for the test harness and for whoever watches
//! a serial console, in one place: the trials assert on these formats, so
//! every interface (GTK today, Kirigami later) must print the same ones. This
//! module formats; [`crate::flow::Flow`] decides when each line exists and
//! queues it (`Flow::drain_markers`), and an interface decides only how a line
//! is written (the GTK application's journal-and-serial worker, `serial.rs`).
//!
//! ```text
//! ILEX-INSTALLER page=<id> edition=<e> <the page's words> [renderer=… scale=… uptime=…]
//! ILEX-COMPOSITION <tag> (<reason>) installs <image>      (machine page, and a choice)
//! ILEX-INSTALLER event=probe result=ok disks=<n> | result=failed <title> | result=stale
//! ILEX-INSTALLER event=plan result=ok | refused code=<code> | failed | stale
//! ILEX-INSTALLER event=layout layout=<id> applied=true|false | result=stale
//! ILEX-INSTALLER event=language language=<locale>
//! ILEX-INSTALLER event=notice severity=info|warning|error text=<the machine page's notice>
//! ILEX-INSTALLER event=disk disk=<id>
//! ILEX-INSTALLER event=disk-gone disk=<id>      (the chosen disk is not offered any more)
//! ILEX-INSTALLER event=attach result=idle|installing|done|failed|unavailable
//! ILEX-INSTALLER event=confirm state=shown|cancelled|accepted
//! ILEX-INSTALLER event=install result=started | refused code=<code> | unknown | not-started
//! ILEX-INSTALLER event=stage stage=<name> status=running|done|failed
//! ILEX-INSTALLER event=outcome result=success | failed stage=<name>
//! ILEX-INSTALLER event=lost failures=<n>
//! ILEX-INSTALLER event=finish action=restart|poweroff|save-log result=ok|refused code=<code>|failed
//! ILEX-INSTALLER event=log state=looking|choosing|saving|saved|failed offered=<n> usable=<n>
//! ILEX-INSTALLER event=try layout=<id> text=<what was typed>      (test boots only)
//! ILEX-INSTALLER-STATS mem_total_kib=… mem_available_kib=… shmem_kib=… uptime=…
//! ILEX-INSTALLER-SELFCHECK ok
//! ILEX-INSTALLER-RENDER ok shots=<n> scheme=light|dark scale=<s> dir=<path>
//! ```
//!
//! Nothing a person types into a field ever appears in a marker, with one
//! exception that only a test can switch on: the layout test field's text,
//! which the keyboard trial reads back to prove that the session's layout
//! changed (typing `yz` gives `zy` in the German layout). People type their
//! intended passwords into that field to see how their keyboard behaves, so
//! on a normal boot the line is not printed at all. A fixed public test
//! credential, [`ECHO_CREDENTIAL`] (SMBIOS, `io.systemd.credential:`, imported
//! by the session unit), or a simulated session (the development container)
//! turns it on; the credential can only make the installer print what was
//! typed in that one field.

use crate::flow::PageId;
use crate::machine::{Notice, Severity};
use crate::messages::ClientError;
use crate::secret::Secret;
use ilex_installer_model::plan::Plan;
use ilex_installer_model::probe::Probe;
use std::path::Path;
use std::sync::OnceLock;

/// The test credential that lets the layout test field's text be printed.
pub const ECHO_CREDENTIAL: &str = "ilex.installer.echo-test";

/// Is the layout test field's text allowed in a marker? Only when the fixed
/// test credential says `1`.
pub fn echo_allowed() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| {
        let Some(directory) = std::env::var_os("CREDENTIALS_DIRECTORY") else {
            return false;
        };
        std::fs::read_to_string(Path::new(&directory).join(ECHO_CREDENTIAL))
            .is_ok_and(|text| text.trim() == "1")
    })
}

/// Words in a marker are separated by spaces: a sentence becomes one word.
pub(crate) fn one_word(text: &str) -> String {
    text.chars()
        .take(400)
        .map(|c| {
            if c.is_whitespace() || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// `ILEX-INSTALLER page=<id> edition=<e> <words>`: printed when a page is
/// shown (`fields` is `Flow::marker_fields`).
pub fn page(edition: &str, page: PageId, fields: &str) -> String {
    let mut line = format!("ILEX-INSTALLER page={} edition={edition}", page.id());
    if !fields.is_empty() {
        line.push(' ');
        line.push_str(fields);
    }
    line
}

pub fn probe(result: &Result<Probe, ClientError>) -> String {
    match result {
        Ok(probe) => format!(
            "ILEX-INSTALLER event=probe result=ok disks={}",
            probe.disks.len()
        ),
        Err(error) => format!(
            "ILEX-INSTALLER event=probe result=failed {}",
            one_word(&error.message().title)
        ),
    }
}

pub fn plan(result: &Result<Plan, ClientError>) -> String {
    match result {
        Ok(_) => "ILEX-INSTALLER event=plan result=ok".to_owned(),
        Err(ClientError::Refused(error)) => {
            format!(
                "ILEX-INSTALLER event=plan result=refused code={:?}",
                error.code
            )
        }
        Err(_) => "ILEX-INSTALLER event=plan result=failed".to_owned(),
    }
}

/// An answer to a request that was replaced meanwhile (`event` is `probe`,
/// `plan` or `layout`): it changed nothing.
pub fn stale(event: &str) -> String {
    format!("ILEX-INSTALLER event={event} result=stale")
}

pub fn layout(layout: &str, applied: bool) -> String {
    format!("ILEX-INSTALLER event=layout layout={layout} applied={applied}")
}

pub fn language(locale: &str) -> String {
    format!("ILEX-INSTALLER event=language language={locale}")
}

/// The machine page's notice, with the model's own sentence (an unknown boot
/// argument's carries the list of offers).
pub fn notice(notice: &Notice) -> String {
    let severity = match notice.severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Error => "error",
    };
    format!(
        "ILEX-INSTALLER event=notice severity={severity} text={}",
        one_word(&notice.message.one_line())
    )
}

/// What was typed in the layout test field, when a test allows it (see the
/// module's text); `None` on a normal boot, when nothing is printed.
pub fn tried(layout: &str, text: &Secret, echo: bool) -> Option<String> {
    echo.then(|| {
        let shown: String = text.expose().chars().take(32).collect();
        format!(
            "ILEX-INSTALLER event=try layout={layout} text={}",
            one_word(&shown)
        )
    })
}

/// The person chose a disk (its id, one word).
pub fn disk(id: &str) -> String {
    format!("ILEX-INSTALLER event=disk disk={}", one_word(id))
}

/// The person turned encryption on or off. (Never the passphrase, nor its
/// length.)
pub fn encryption(on: bool) -> String {
    format!(
        "ILEX-INSTALLER event=encryption encryption={}",
        if on { "on" } else { "off" }
    )
}

/// The person turned the security chip's switch on or off.
pub fn chip(on: bool) -> String {
    format!(
        "ILEX-INSTALLER event=chip chip={}",
        if on { "on" } else { "off" }
    )
}

/// The recovery key's page: `taking` (asking the helper for the key), `shown`
/// (this interface holds it and draws it), `gone` (it was shown to an
/// interface that no longer exists), `confirmed`, `refused` (the helper said
/// the groups are not the key's) or `failed`. Never a letter of the key.
pub fn recovery(state: &str) -> String {
    format!("ILEX-INSTALLER event=recovery state={state}")
}

/// The disk the person had chosen is not offered any more.
pub fn disk_gone(id: &str) -> String {
    format!("ILEX-INSTALLER event=disk-gone disk={}", one_word(id))
}

/// Where the interface found the installation when it started.
pub fn attach(result: &str) -> String {
    format!("ILEX-INSTALLER event=attach result={result}")
}

/// The erase confirmation: `shown`, `cancelled` or `accepted`.
pub fn confirm(state: &str) -> String {
    format!("ILEX-INSTALLER event=confirm state={state}")
}

/// What `Apply` answered (`started`, `refused code=…`, `unknown` when the
/// answer was lost, `not-started` when the helper then said none began).
pub fn install(result: &str) -> String {
    format!("ILEX-INSTALLER event=install result={result}")
}

pub fn install_refused(error: &ilex_installer_model::plan::PlanError) -> String {
    format!(
        "ILEX-INSTALLER event=install result=refused code={:?}",
        error.code
    )
}

/// A stage changed status in the helper's record.
pub fn stage(stage: ilex_installer_model::state::Stage, status: &str) -> String {
    format!(
        "ILEX-INSTALLER event=stage stage={} status={status}",
        stage.name()
    )
}

/// The installation ended.
pub fn outcome(outcome: &ilex_installer_model::state::Outcome) -> String {
    use ilex_installer_model::state::Outcome;
    match outcome {
        Outcome::Success => "ILEX-INSTALLER event=outcome result=success".to_owned(),
        Outcome::Failed { stage, .. } => format!(
            "ILEX-INSTALLER event=outcome result=failed stage={}",
            stage.name()
        ),
    }
}

/// The helper has not answered `State` for this many requests running.
pub fn lost(failures: u32) -> String {
    format!("ILEX-INSTALLER event=lost failures={failures}")
}

/// The answer to `Finish`: its action's word, never its device.
pub fn finish(action: &str, result: &Result<(), ClientError>) -> String {
    match result {
        Ok(()) => format!("ILEX-INSTALLER event=finish action={action} result=ok"),
        Err(ClientError::Refused(error)) => format!(
            "ILEX-INSTALLER event=finish action={action} result=refused code={:?}",
            error.code
        ),
        Err(_) => format!("ILEX-INSTALLER event=finish action={action} result=failed"),
    }
}

/// The log dialog: `looking` (asking the helper which sticks there are),
/// `choosing`, `saving`, `saved` or `failed`.
pub fn log(state: &str, offered: usize, usable: usize) -> String {
    format!("ILEX-INSTALLER event=log state={state} offered={offered} usable={usable}")
}

/// Memory and uptime, reported once the session has settled.
pub fn stats() -> String {
    format!(
        "ILEX-INSTALLER-STATS mem_total_kib={} mem_available_kib={} shmem_kib={} uptime={}",
        meminfo("MemTotal"),
        meminfo("MemAvailable"),
        meminfo("Shmem"),
        uptime()
    )
}

pub const SELF_CHECK_OK: &str = "ILEX-INSTALLER-SELFCHECK ok";

pub fn render_ok(shots: usize, dark: bool, scale: f64, directory: &Path) -> String {
    format!(
        "ILEX-INSTALLER-RENDER ok shots={shots} scheme={} scale={scale} dir={}",
        if dark { "dark" } else { "light" },
        directory.display()
    )
}

pub fn meminfo(key: &str) -> String {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|text| {
            text.lines()
                .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))
                .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Seconds since boot, from the kernel.
pub fn uptime() -> String {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}
