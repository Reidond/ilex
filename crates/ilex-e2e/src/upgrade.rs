//! `upgrade::` tier: v1 installed, then upgraded to v2; signature enforcement
//! on v2; rollback returns to v1.
//!
//! One scenario on one libvirt VM (a real installed system that can reboot),
//! so the slow part, installing v1 to a disk, happens once:
//!
//! 1. install v1 and boot it; leave a file in `/var` and edit a file in `/etc`;
//! 2. build v2 (the image under test plus a marker layer) under the tag v1 was
//!    installed from, and `bootc upgrade` in the guest (stages, no reboot);
//! 3. reboot: v2 is booted, its marker is present, the `/var` file survived,
//!    the `/etc` edit was kept and the `/etc` file v2 added was merged in;
//! 4. signature enforcement, by v2's own `ilex-sigpolicy.service`: it left
//!    the containers-storage origin alone at boot; the origin is then pointed
//!    at ghcr unverified, the way an ISO install leaves it, and after a reboot
//!    the unit must have added the verification (a second run is a no-op);
//! 5. `bootc rollback` and reboot: v1 is booted again, the marker is gone.
//!
//! Inputs, all optional:
//!
//! * `ILEX_IMAGE`: the candidate, v2's base (xtask sets it);
//! * `ILEX_UPGRADE_FROM`: v1, e.g. the published `ghcr.io/<owner>/ilex:latest`
//!   in CI. Without it v1 is the candidate itself, so the test covers
//!   "upgrade from what you just built". When `CI` is set and this is empty
//!   (the very first run has no `:latest`), the tier is skipped;
//! * `ILEX_UPGRADE_PREPARE=1`: only install v1 and boot it, then stop. bcvk
//!   caches the installed base disk per image, so the real run that follows
//!   boots v1 in seconds. CI does this in the background while it builds the
//!   candidate, which takes the install (most of this tier's time) off its
//!   critical path.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ilex_devkit::bcvk::{self, LibvirtOpts, LibvirtVm};
use ilex_devkit::{Workspace, cmd};
use libtest_mimic::{Completion, Trial};
use serde_json::Value;

use crate::common::{self, ensure, trial};

/// Written to `/usr` by v2's marker layer.
const MARKER_USR: &str = "/usr/share/ilex/upgrade-marker";
/// Shipped in `/etc` by v2's marker layer; must appear through the 3-way merge.
const MARKER_ETC: &str = "/etc/ilex-upgrade-marker.conf";
/// Written to `/var` on v1; must survive upgrade and rollback.
const VAR_FILE: &str = "/var/lib/ilex-e2e/survivor";
/// A file the image ships in `/etc`, edited locally on v1.
const ETC_EDITED: &str = "/etc/motd";
const ETC_EDIT: &str = "ilex-e2e local edit";

const REBOOT_TIMEOUT: Duration = Duration::from_secs(600);

pub fn trials() -> Vec<Trial> {
    const NAME: &str = "upgrade::upgrade_and_rollback";
    if std::env::var_os("CI").is_some_and(|v| !v.is_empty()) && upgrade_from().is_none() {
        let why = "no ILEX_UPGRADE_FROM (nothing published to upgrade from yet)";
        return vec![Trial::ignorable_test(NAME, move || {
            Ok(Completion::ignored_with(why))
        })];
    }
    vec![trial(NAME, upgrade_and_rollback)]
}

/// `ILEX_UPGRADE_PREPARE=1`: install v1 (filling bcvk's base-disk cache) and stop.
fn prepare_only() -> bool {
    std::env::var("ILEX_UPGRADE_PREPARE").is_ok_and(|v| v == "1")
}

fn upgrade_from() -> Option<String> {
    std::env::var("ILEX_UPGRADE_FROM")
        .ok()
        .filter(|s| !s.is_empty())
}

fn step(what: &str) {
    eprintln!("ilex: upgrade: {what}");
}

/// What `bootc status` says about a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Deployment {
    image: String,
    digest: String,
}

#[derive(Debug)]
struct Status {
    booted: Deployment,
    staged: Option<Deployment>,
    rollback: Option<Deployment>,
}

fn status(vm: &LibvirtVm) -> Result<Status> {
    let out = vm.ssh("bootc status --json")?.ok("bootc status --json")?;
    let v: Value = serde_json::from_str(&out.stdout).context("parsing `bootc status --json`")?;
    let deployment = |v: &Value| -> Option<Deployment> {
        let image = v.get("image").filter(|i| !i.is_null())?;
        Some(Deployment {
            image: image["image"]["image"].as_str()?.to_string(),
            digest: image["imageDigest"].as_str()?.to_string(),
        })
    };
    let s = &v["status"];
    Ok(Status {
        booted: deployment(&s["booted"])
            .with_context(|| format!("no booted image in:\n{}", out.stdout))?,
        staged: deployment(&s["staged"]),
        rollback: deployment(&s["rollback"]),
    })
}

fn upgrade_and_rollback() -> Result<()> {
    let ws = common::workspace()?;
    let candidate = common::image()?;
    let (v1_source, installed) = prepare_v1(&ws, &candidate)?;
    let token = unique_token();

    step(&format!("installing v1 ({v1_source}) as {installed}"));
    let name = ws.vm_name("upgrade");
    let vm = LibvirtVm::boot(&name, &installed, &LibvirtOpts::default())?;
    step(&format!("{name} is up (VM slot {})", vm.slot_index()));
    if prepare_only() {
        step("v1 is installed and its base disk cached (ILEX_UPGRADE_PREPARE); stopping here");
        return Ok(());
    }
    // Nothing here may reach for the network or stage an update of its own,
    // least of all once the origin points at ghcr (step 4). Masked in /etc,
    // so it stays masked across the upgrade.
    vm.ssh("systemctl mask --now bootc-fetch-apply-updates.timer")?
        .ok("masking the update timer")?;

    // --- v1 ---------------------------------------------------------------
    let v1 = status(&vm)?;
    step(&format!(
        "v1 booted: {} {}",
        v1.booted.image, v1.booted.digest
    ));
    ensure(!path_exists(&vm, MARKER_USR)?, || {
        format!("{MARKER_USR} already exists on v1")
    })?;
    ensure(v1.staged.is_none(), || {
        format!("v1 has a staged deployment: {:?}", v1.staged)
    })?;
    vm.ssh(&format!(
        "mkdir -p $(dirname {VAR_FILE}) && echo {token} > {VAR_FILE} && echo '{ETC_EDIT} {token}' >> {ETC_EDITED}"
    ))?
    .ok("writing the /var file and the /etc edit")?;

    // --- upgrade to v2 ----------------------------------------------------
    step("building v2 (candidate + marker layer)");
    build_v2(&candidate, &installed, &token)?;
    step("bootc upgrade (stage only)");
    let upgrade = vm
        .ssh_timeout("bootc upgrade", Duration::from_secs(900))?
        .ok("bootc upgrade")?;
    eprintln!("{}", upgrade.stdout.trim_end());
    let staged = status(&vm)?;
    let v2 = staged
        .staged
        .clone()
        .context("`bootc upgrade` staged nothing")?;
    ensure(v2.digest != v1.booted.digest, || {
        format!("staged digest equals v1's: {}", v2.digest)
    })?;
    ensure(staged.booted == v1.booted, || {
        format!(
            "booted deployment changed before the reboot: {:?}",
            staged.booted
        )
    })?;
    ensure(!path_exists(&vm, MARKER_USR)?, || {
        "the marker appeared before the reboot".into()
    })?;

    step("rebooting into v2");
    vm.reboot(REBOOT_TIMEOUT)?;

    // --- v2 ---------------------------------------------------------------
    let after = status(&vm)?;
    ensure(after.booted == v2, || {
        format!("booted {:?}, expected the staged {v2:?}", after.booted)
    })?;
    ensure(after.rollback.as_ref() == Some(&v1.booted), || {
        format!(
            "rollback deployment is {:?}, expected v1 {:?}",
            after.rollback, v1.booted
        )
    })?;
    check_v2(&vm, &token)?;

    // --- signature enforcement, on v2 -----------------------------------
    check_sigpolicy(&vm, &installed)?;

    // --- rollback ---------------------------------------------------------
    step("bootc rollback");
    vm.ssh("bootc rollback")?.ok("bootc rollback")?;
    step("rebooting into v1");
    vm.reboot(REBOOT_TIMEOUT)?;

    let back = status(&vm)?;
    ensure(back.booted == v1.booted, || {
        format!("booted {:?}, expected v1 {:?}", back.booted, v1.booted)
    })?;
    ensure(!path_exists(&vm, MARKER_USR)?, || {
        format!("{MARKER_USR} still exists after rollback")
    })?;
    let var = vm
        .ssh(&format!("cat {VAR_FILE}"))?
        .ok("reading the /var file after rollback")?;
    ensure(var.out() == token, || {
        format!("/var file changed across rollback: {}", var.out())
    })?;
    step("upgrade and rollback verified");
    Ok(())
}

/// What must hold on the freshly booted v2.
fn check_v2(vm: &LibvirtVm, token: &str) -> Result<()> {
    let marker = vm
        .ssh(&format!("cat {MARKER_USR}"))?
        .ok("reading the v2 marker")?;
    ensure(marker.out() == token, || {
        format!("v2 marker is `{}`, expected `{token}`", marker.out())
    })?;

    let var = vm
        .ssh(&format!("cat {VAR_FILE}"))?
        .ok("reading the /var file")?;
    ensure(var.out() == token, || {
        format!("/var file is `{}`, expected `{token}`", var.out())
    })?;

    let issue = vm
        .ssh(&format!("cat {ETC_EDITED}"))?
        .ok("reading the edited /etc file")?;
    ensure(
        issue.stdout.contains(&format!("{ETC_EDIT} {token}")),
        || format!("the local edit of {ETC_EDITED} was lost:\n{}", issue.stdout),
    )?;

    let merged = vm
        .ssh(&format!("cat {MARKER_ETC}"))?
        .ok("reading the /etc file v2 added")?;
    ensure(merged.out() == token, || {
        format!("{MARKER_ETC} is `{}`, expected `{token}`", merged.out())
    })
}

fn path_exists(vm: &LibvirtVm, path: &str) -> Result<bool> {
    Ok(vm.ssh(&format!("test -e {path}"))?.success())
}

fn unique_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("v2-{nanos:x}")
}

/// Make v1 available under the tag the VM is installed from; returns what v1
/// is and that tag. A tag of our own keeps the checkout's real image tag from
/// being replaced by v2.
fn prepare_v1(ws: &Workspace, candidate: &str) -> Result<(String, String)> {
    let source = match upgrade_from() {
        Some(from) => {
            if !bcvk::image_exists(&from) {
                cmd::run(Command::new("podman").args(["pull", "--quiet", &from]))?;
            }
            from
        }
        None => candidate.to_string(),
    };
    let installed = format!("{}-upgrade", ws.image_ref());
    cmd::run(Command::new("podman").args(["tag", &source, &installed]))?;
    Ok((source, installed))
}

/// v2 = the candidate plus a layer that adds a `/usr` marker and an `/etc`
/// file, tagged as `installed` so the guest finds it where it installed v1
/// from.
fn build_v2(candidate: &str, installed: &str, token: &str) -> Result<()> {
    let containerfile = format!(
        "FROM {candidate}\n\
         RUN mkdir -p /usr/share/ilex && echo {token} > {MARKER_USR} && echo {token} > {MARKER_ETC}\n"
    );
    let ctx = std::env::temp_dir().join(format!("ilex-upgrade-ctx-{}", std::process::id()));
    std::fs::create_dir_all(&ctx)?;
    let mut build = Command::new("podman");
    build
        .args(["build", "--layers", "--file", "-", "--tag", installed])
        .arg(&ctx)
        .stdin(Stdio::piped());
    eprintln!("$ {}", cmd::display(&build));
    let mut child = build.spawn().context("spawning podman build")?;
    child
        .stdin
        .take()
        .context("podman stdin")?
        .write_all(containerfile.as_bytes())?;
    let status = child.wait()?;
    let _ = std::fs::remove_dir_all(&ctx);
    if !status.success() {
        bail!("building v2 failed with {status}");
    }
    Ok(())
}

// --- signature enforcement ------------------------------------------------

/// A machine tracking `ghcr.io/reidond/ilex` without signature verification
/// (an ISO install, a plain `bootc switch`) is pinned to signed images by
/// `ilex-sigpolicy.service` at its next boot, and the unit is a no-op on
/// anything else and on machines that already verify.
///
/// Runs on the freshly booted v2, the image under test. Its origin is the
/// containers-storage tag it was upgraded from, which already is "some other
/// image": the unit must have left it alone at boot. Then the origin is
/// rewritten to the ghcr reference the way an ISO install leaves it (`bootc
/// switch --mutate-in-place`, which changes the origin only and downloads
/// nothing), the VM reboots, and the boot-time run must have added the
/// verification. The deployment itself never changes, so the rollback that
/// follows is unaffected.
fn check_sigpolicy(vm: &LibvirtVm, installed: &str) -> Result<()> {
    const GHCR: &str = "ghcr.io/reidond/ilex:latest";
    const UNIT: &str = "ilex-sigpolicy.service";

    // --- another image: untouched ---------------------------------------
    let before = booted(vm)?;
    ensure(
        before.image == installed && before.transport == "containers-storage",
        || format!("unexpected starting point: {before:?}"),
    )?;
    ensure(before.signature.is_none(), || {
        format!("a locally installed image already has a signature: {before:?}")
    })?;
    let ran = unit_log(vm, UNIT)?;
    ensure(ran.contains("nothing to do"), || {
        format!("{UNIT} should have been a no-op at boot, journal:\n{ran}")
    })?;

    // --- tracking ghcr, unverified, as after an ISO install ---------------
    step(&format!(
        "sigpolicy: pointing the origin at {GHCR} without verification"
    ));
    // /sysroot is read-only on a running system; remount it in a private
    // mount namespace, as the unit does.
    vm.ssh(&format!(
        "unshare --mount --propagation private sh -c \
         'mount -o remount,rw /sysroot && \
         bootc switch --mutate-in-place --transport registry {GHCR}'"
    ))?
    .ok("bootc switch --mutate-in-place")?;
    let unverified = booted(vm)?;
    ensure(
        unverified.image == GHCR
            && unverified.transport == "registry"
            && unverified.signature.is_none(),
        || format!("expected {GHCR} tracked unverified, got {unverified:?}"),
    )?;
    ensure(unverified.digest == before.digest, || {
        "the in-place switch changed the booted digest".to_string()
    })?;

    step("sigpolicy: rebooting, the unit must enforce at boot");
    vm.reboot(REBOOT_TIMEOUT)?;
    let enforced = booted(vm)?;
    ensure(
        enforced.image == GHCR
            && enforced.transport == "registry"
            && enforced.signature.as_deref() == Some("containerPolicy"),
        || format!("expected {GHCR} with container policy enforcement, got {enforced:?}"),
    )?;
    ensure(enforced.digest == before.digest && !enforced.staged, || {
        format!("enforcement changed the deployment: {enforced:?}")
    })?;
    let ran = unit_log(vm, UNIT)?;
    ensure(
        ran.contains("enforcing") && ran.contains("now verifies"),
        || format!("{UNIT} did not report enforcing at boot, journal:\n{ran}"),
    )?;

    // --- again: nothing left to do ---------------------------------------
    step("sigpolicy: running the unit again, it must be a no-op");
    vm.ssh(&format!("systemctl restart {UNIT}"))?
        .ok("restarting the unit")?;
    let again = booted(vm)?;
    ensure(again == enforced, || {
        format!("a second run changed {enforced:?} into {again:?}")
    })?;
    let ran = unit_log(vm, UNIT)?;
    ensure(ran.contains("already verified"), || {
        format!("expected `already verified` after the restart, journal:\n{ran}")
    })?;
    step("signature enforcement verified");
    Ok(())
}

/// The booted deployment's image reference, from `bootc status --json`.
#[derive(Debug, PartialEq, Eq)]
struct BootedRef {
    image: String,
    transport: String,
    /// `containerPolicy`, `insecure`, ...; `None` when bootc records none.
    signature: Option<String>,
    digest: String,
    staged: bool,
}

fn booted(vm: &LibvirtVm) -> Result<BootedRef> {
    let out = vm.ssh("bootc status --json")?.ok("bootc status --json")?;
    let v: Value = serde_json::from_str(&out.stdout).context("parsing `bootc status --json`")?;
    let booted = &v["status"]["booted"];
    let reference = &booted["image"]["image"];
    let field = |name: &str| {
        reference[name]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("no `{name}` in the booted image:\n{}", out.stdout))
    };
    Ok(BootedRef {
        image: field("image")?,
        transport: field("transport")?,
        signature: reference["signature"].as_str().map(str::to_string),
        digest: booted["image"]["imageDigest"]
            .as_str()
            .with_context(|| format!("no digest in:\n{}", out.stdout))?
            .to_string(),
        staged: !v["status"]["staged"].is_null(),
    })
}

/// What `unit` printed since the current boot, plus any restart in it.
fn unit_log(vm: &LibvirtVm, unit: &str) -> Result<String> {
    Ok(vm
        .ssh(&format!("journalctl -u {unit} -b -o cat --no-pager"))?
        .ok("reading the unit's journal")?
        .stdout)
}
