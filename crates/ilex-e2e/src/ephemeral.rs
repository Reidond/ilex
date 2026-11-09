//! `ephemeral::` tier: The image booted directly as an ephemeral VM, no disk.
//!
//! One VM serves the whole tier. It comes from `bcvk ephemeral run`, holds a
//! machine-wide VM slot while it lives, and is removed when the last
//! `ephemeral::` test finishes (or, on a panic, when its guard drops).

use anyhow::{Context, Result};
use ilex_devkit::bcvk::{self, EphemeralOpts, EphemeralVm};
use libtest_mimic::Trial;
use serde_json::Value;

use crate::common::{self, Group, ensure, trial};

const AGENT: &str = "/usr/bin/ilex-agent";

static GROUP: Group<EphemeralVm> = Group::new("ephemeral", boot);

fn boot() -> Result<EphemeralVm> {
    let ws = common::workspace()?;
    let image = common::image()?;
    let name = ws.vm_name("ephemeral");
    let vm = EphemeralVm::boot(&name, &image, &EphemeralOpts::default())?;
    eprintln!(
        "ilex: {name} is up from {image} (VM slot {})",
        vm.slot_index()
    );
    Ok(vm)
}

/// How many `ephemeral::` tests will run, so the last one frees the VM.
pub fn expect(tests: usize) {
    GROUP.expect(tests);
}

/// Release the VM now (safety net after the run).
pub fn shutdown() {
    GROUP.finish();
}

pub fn trials() -> Vec<Trial> {
    vec![
        trial("ephemeral::system_running", || GROUP.run(system_running)),
        trial("ephemeral::no_failed_units", || GROUP.run(no_failed_units)),
        trial("ephemeral::sshd_active", || GROUP.run(sshd_active)),
        trial("ephemeral::agent_self_check", || {
            GROUP.run(agent_self_check)
        }),
        trial("ephemeral::image_under_test", || {
            GROUP.run(image_under_test)
        }),
        trial("ephemeral::bootc_status", || GROUP.run(bootc_status)),
        trial("ephemeral::graphical_default", || {
            GROUP.run(graphical_default)
        }),
        trial("ephemeral::sigpolicy_noop", || GROUP.run(sigpolicy_noop)),
    ]
}

/// systemd finished booting and nothing is degraded.
fn system_running(vm: &EphemeralVm) -> Result<()> {
    let state = vm
        .ssh_timeout(
            "systemctl is-system-running --wait",
            std::time::Duration::from_secs(300),
        )?
        .out()
        .to_string();
    if state == "running" {
        return Ok(());
    }
    let failed = vm.ssh("systemctl --failed --no-pager --plain")?.stdout;
    anyhow::bail!("system state is `{state}`, expected `running`\n{failed}")
}

/// No unit is in the failed state.
fn no_failed_units(vm: &EphemeralVm) -> Result<()> {
    vm.ssh_timeout(
        "systemctl is-system-running --wait",
        std::time::Duration::from_secs(300),
    )?;
    let failed = vm
        .ssh("systemctl --failed --no-legend --no-pager --plain")?
        .ok("systemctl --failed")?;
    ensure(failed.out().is_empty(), || {
        format!("failed units:\n{}", failed.out())
    })
}

/// sshd answers (it carried this very connection) and is the unit systemd
/// thinks it is.
fn sshd_active(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("systemctl is-active sshd.service")?
        .ok("systemctl is-active sshd.service")?;
    ensure(out.out() == "active", || {
        format!("sshd.service is `{}`", out.out())
    })
}

/// The binaries we ship check themselves from inside the guest.
fn agent_self_check(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("ilex-agent --self-check")?
        .ok("ilex-agent --self-check")?;
    ensure(out.stdout.contains("self-check: ok"), || {
        format!("unexpected output:\n{}", out.stdout)
    })
}

/// The VM runs the image under test.
///
/// An ephemeral VM boots the image's filesystem over virtiofs, with no ostree
/// deployment, so `bootc status` cannot name the image here (that assertion
/// lives in the `upgrade::` tier, on an installed system). Instead compare
/// the image bcvk started the VM from with the one under test, and a binary
/// the guest runs with the same file inside the image.
fn image_under_test(vm: &EphemeralVm) -> Result<()> {
    let want = bcvk::image_id(vm.image())?;
    let got = vm.booted_image_id()?;
    ensure(got == want, || {
        format!(
            "VM container runs image {got}, expected {want} ({})",
            vm.image()
        )
    })?;

    let in_image = bcvk::image_file_sha256(vm.image(), AGENT)?;
    let in_guest = vm
        .ssh(&format!("sha256sum {AGENT}"))?
        .ok("sha256sum in the guest")?;
    let in_guest = in_guest
        .out()
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    ensure(in_guest == in_image, || {
        format!(
            "{AGENT} is {in_guest} in the guest but {in_image} in {}",
            vm.image()
        )
    })
}

/// bootc works in the guest. With no deployment in an ephemeral VM there is
/// no booted image to compare, but if a future bcvk does report one it must be
/// the image under test.
fn bootc_status(vm: &EphemeralVm) -> Result<()> {
    let out = vm.ssh("bootc status --json")?.ok("bootc status --json")?;
    let status: Value =
        serde_json::from_str(&out.stdout).context("parsing `bootc status --json`")?;
    ensure(status["kind"] == "BootcHost", || {
        format!("not a BootcHost document:\n{}", out.stdout)
    })?;
    if let Some(booted) = status["status"]["booted"]["image"]["image"]["image"].as_str() {
        let want = vm.image();
        ensure(same_image(booted, want), || {
            format!("bootc status shows {booted}, expected {want}")
        })?;
    }
    Ok(())
}

/// The image's default target is the graphical one (the desktop is the point
/// of ilex), and it was reached without a display attached.
fn graphical_default(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("systemctl get-default")?
        .ok("systemctl get-default")?;
    ensure(out.out() == "graphical.target", || {
        format!("default target is {}", out.out())
    })
}

/// Compare image references, ignoring a leading `localhost/`.
fn same_image(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.strip_prefix("localhost/").unwrap_or(s).to_string();
    norm(a) == norm(b)
}

/// Signature enforcement does nothing here: an ephemeral VM has no booted
/// deployment, so `ilex-agent enforce-sigpolicy` (which `ilex-sigpolicy.service`
/// runs at every boot of an installed machine) must neither fail nor try to
/// switch anything. The unit itself must not be in the failed state, which
/// `no_failed_units` already requires; the real switch is tested on an
/// installed system, in `upgrade::sigpolicy_enforced`.
fn sigpolicy_noop(vm: &EphemeralVm) -> Result<()> {
    let out = vm
        .ssh("ilex-agent enforce-sigpolicy")?
        .ok("ilex-agent enforce-sigpolicy")?;
    ensure(out.stdout.contains("nothing to do"), || {
        format!("expected a no-op, got:\n{}{}", out.stdout, out.stderr)
    })?;
    let unit = vm.ssh("systemctl is-failed ilex-sigpolicy.service")?;
    ensure(unit.out() != "failed", || {
        "ilex-sigpolicy.service failed".to_string()
    })
}
