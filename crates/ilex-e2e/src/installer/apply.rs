//! `installer::policy`, `installer::install` and `installer::interrupted`: the
//! helper writing a disk, for real.
//!
//! `installer::probe` and `installer::plan` run in a bcvk ephemeral VM, which
//! boots a kernel directly and so has no UEFI: `bootupctl --auto` there
//! installs GRUB for BIOS, which a GPT disk without a BIOS boot partition
//! refuses, and no installation can finish. These trials boot the live image
//! the way the ISO does (UEFI, OVMF) from a disk `bcvk to-disk` writes of it,
//! with the target and a sentinel disk beside it, and call the helper over the
//! bus as `ilex-installer`, as the interface will. The installed disk is then
//! booted in a VM of its own, and what the helper says it did is checked
//! against what boots.
//!
//! * `installer::policy`: every way `Apply` refuses, each with the disks
//!   byte for byte as they were before it: no secret, a wrong confirmation, an
//!   unknown or stale plan, a disk that changed or is in use since the plan,
//!   a caller the bus refuses, an image whose signature does not verify (the
//!   key is not ilex's; no signature is found).
//! * `installer::install`: a whole-disk installation of `generic.gnome` with
//!   non-default answers; the progress and signals on the way; what is on the
//!   disk; then the disk booted: the digest the plan pinned, the origin that
//!   enforces the container signature policy, the administrator logging in
//!   with the password and `sudo`, the answers applied, the system running
//!   with nothing failed and GDM up.
//! * `installer::interrupted`: the helper killed in the middle of the pull
//!   leaves a state that says so and no mount; a new plan and a new `Apply`
//!   on the same disk then install a system that boots.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::Workspace;
use ilex_devkit::installer_model::{
    plan::{ErrorCode, Plan, PlanError},
    probe::{Disk, Probe},
    state::{Outcome, Stage, StageStatus, State},
};
use ilex_devkit::qemu::{self, DiskSpec, Vm, VmConfig, base64};
use ilex_vmctl::{Vmctl, WaitCond};
use libtest_mimic::{Failed, Trial};
use serde_json::{Value, json};

use crate::common;
use crate::selinux;

pub(crate) mod encrypted;

const GIB: u64 = 1 << 30;
/// The disk that is installed to: sparse, larger than the least a plan wants.
const TARGET_BYTES: u64 = 41 * GIB;
/// A disk that must come out of every trial exactly as it went in.
const SENTINEL_BYTES: u64 = 3 * GIB;
/// A disk with no serial number, so no `/dev/disk/by-id` name (the policy
/// trial's only), large enough to be planned for.
const BARE_BYTES: u64 = 36 * GIB;
/// The live VM: a pull streams into the target, and the helper, bootc and the
/// page cache share the memory (the ISO's trials use as much).
const MEMORY: &str = "6G";
const BUS: &str = "org.ilex.Installer1";
pub(crate) const ADMIN: &str = "ilexmaint";
/// Digests of answers that were refused in the boot, for the stamp: not any
/// request's, only two lines the helper must carry.
const SEEN_ONE: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const SEEN_TWO: &str = "2222222222222222222222222222222222222222222222222222222222222222";
/// Digests of the stamp a disk held before an installation erased it
/// (`installer::interrupted`): not any request's either.
const OLD_ONE: &str = "3333333333333333333333333333333333333333333333333333333333333333";
const OLD_TWO: &str = "4444444444444444444444444444444444444444444444444444444444444444";

/// What a person types: spaces, a colon, quotes. It must survive the memfd,
/// the bus, chpasswd's one line and PAM.
pub(crate) const PASSWORD: &str = "Correct horse: 'battery' \"staple\"";
/// The pull and everything after it, on a contended runner.
const INSTALL_TIMEOUT: Duration = Duration::from_secs(40 * 60);

pub fn trials() -> Vec<Trial> {
    let run = |name: &'static str, body: fn() -> Result<()>| {
        Trial::test(name, move || {
            body().map_err(|error| Failed::from(format!("{error:#}")))
        })
    };
    vec![
        run("installer::policy", policy),
        run("installer::install", install),
        run("installer::encrypted", || encrypted::run(&encrypted::US)),
        run("installer::encrypted_german", || {
            encrypted::run(&encrypted::GERMAN)
        }),
        run("installer::interrupted", interrupted),
    ]
}

// --- the VMs ----------------------------------------------------------------

/// A disk file the VM gets, sparse and fresh.
fn raw_disk(path: &Path, bytes: u64) -> Result<()> {
    let _ = fs::remove_file(path);
    fs::File::create(path)
        .and_then(|file| file.set_len(bytes))
        .with_context(|| format!("creating {}", path.display()))
}

/// What a trial works with: its private directory, the live image as a disk
/// and the two disks it gets.
struct Bench {
    ws: Workspace,
    /// The trial's private directory.
    dir: PathBuf,
    live: PathBuf,
    target: PathBuf,
    sentinel: PathBuf,
    bare: PathBuf,
    /// Images attached to the live VM as removable USB sticks (where a log is
    /// saved), when the trial has any.
    sticks: Vec<PathBuf>,
    artifacts: PathBuf,
    name: &'static str,
}

impl Bench {
    fn new(name: &'static str) -> Result<Self> {
        let ws = common::workspace()?;
        let image = super::with_ssh(&ws, &super::live_image()?)?;
        let mut spec = DiskSpec::new(&image);
        spec.format = qemu::DiskFormat::Qcow2;
        let live = qemu::ensure_disk(&ws, &spec)
            .with_context(|| format!("writing a disk of the live image {image}"))?;
        let dir = ws.state_dir().join("installer-apply").join(name);
        fs::create_dir_all(&dir)?;
        let target = dir.join("target.img");
        let sentinel = dir.join("sentinel.img");
        raw_disk(&target, TARGET_BYTES)?;
        raw_disk(&sentinel, SENTINEL_BYTES)?;
        let bare = dir.join("bare.img");
        raw_disk(&bare, BARE_BYTES)?;
        let artifacts = qemu::artifact_dir(&ws, "installer-apply")?;
        Ok(Self {
            ws,
            dir,
            live,
            target,
            sentinel,
            bare,
            sticks: Vec::new(),
            artifacts,
            name,
        })
    }

    /// Boot the live image from its disk (UEFI, a throwaway overlay) with the
    /// target and the sentinel attached, and wait for the helper.
    fn boot_live(&self) -> Result<Vm> {
        let mut config = VmConfig::new(format!("inst-{}", self.name), &self.live);
        config.memory = MEMORY.into();
        config.extra_disks = vec![
            self.target.clone(),
            self.sentinel.clone(),
            self.bare.clone(),
        ];
        // By-id names for the first two: an installation writes its disk by the
        // stable name once it has let go of its exclusive claim. The third has
        // none.
        config.disk_serials = true;
        config.bare_disks = 1;
        config.extra_usb_disks = self.sticks.clone();
        let mut vm = Vm::boot(&self.ws, &config)?;
        vm.wait_ssh(Duration::from_secs(420))
            .context("the live image did not accept the harness's SSH key")?;
        let ready = ssh(
            &vm,
            &format!(
                "for i in $(seq 1 120); do busctl --system status {BUS} >/dev/null 2>&1 && exit 0; sleep 1; done; systemctl status ilex-installerd --no-pager; journalctl -u ilex-installerd --no-pager | tail -30; exit 1"
            ),
        )?;
        ensure!(
            ready.code == Some(0),
            "the helper never took {BUS}: {}",
            ready.text
        );
        // The live image written to a disk is an ostree host: `bootc install`,
        // run from one, asks the host's own repository for its commit
        // (`ostree --repo=/ostree/repo rev-parse --single`) and stops at
        // "Multiple commit objects found" (the image's layers are refs of it).
        // On the ISO, whose root is an overlay on a squashfs, there is no
        // repository to ask. Hide this one, so that the helper meets what it
        // meets there (the unattended ISO trial is the proof it does).
        ok(&vm, "mount -t tmpfs tmpfs /sysroot/ostree")?;
        // The ISO boots SELinux enforcing and so does this: the helper's
        // `bootc install` enters `install_t` under the module the live image
        // ships (`ilex-installer.cil`), and nothing is made permissive here.
        let mode = ok(&vm, "getenforce")?;
        ensure!(
            mode == "Enforcing",
            "the live image booted {mode}, not Enforcing"
        );
        Ok(vm)
    }

    /// Boot the installed target disk (UEFI, a throwaway overlay).
    fn boot_target(&self) -> Result<Vm> {
        let mut config = VmConfig::new(format!("inst-{}-t", self.name), &self.target);
        config.memory = "4G".into();
        let mut vm = Vm::boot(&self.ws, &config)?;
        if let Err(error) = vm.wait_ssh(Duration::from_secs(600)) {
            // What a person would see: the console and the screen.
            if let Ok(serial) = fs::read(vm.info.dir.join("serial.log")) {
                self.save("target-serial.txt", &String::from_utf8_lossy(&serial));
            }
            let _ = Vmctl::from_info(vm.info.clone()).save_screenshot(
                &self
                    .artifacts
                    .join(format!("installer-{}-target-failure.png", self.name)),
            );
            return Err(error).context("the installed disk did not come up");
        }
        Ok(vm)
    }

    fn save(&self, name: &str, text: &str) {
        let _ = qemu::write_artifact(
            &self.artifacts,
            &format!("installer-{}-{name}", self.name),
            text,
        );
    }
}

// --- talking to the helper --------------------------------------------------

pub(crate) struct Ran {
    pub(crate) code: Option<i32>,
    pub(crate) text: String,
}

pub(crate) fn ssh(vm: &Vm, script: &str) -> Result<Ran> {
    let out = vm.ssh(script)?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(Ran {
        code: out.status.code(),
        text,
    })
}

fn ok(vm: &Vm, script: &str) -> Result<String> {
    let ran = ssh(vm, script)?;
    ensure!(
        ran.code == Some(0),
        "`{script}` failed ({:?}):\n{}",
        ran.code,
        ran.text
    );
    Ok(ran.text.trim().to_owned())
}

/// A method call as `user`, in a script for the guest's root shell. The
/// request travels base64 so no quoting depends on its text.
pub(crate) fn busctl(user: &str, method: &str, arguments: &[&str]) -> String {
    let arguments: String = arguments
        .iter()
        .map(|a| format!(" \"$(printf %s {} | base64 -d)\"", base64(a.as_bytes())))
        .collect();
    let signature = "s".repeat(arguments.matches("base64").count());
    format!(
        "runuser -u {user} -- env SYSTEMD_COLORS=0 SYSTEMD_PAGER= busctl --system --json=short --timeout=120 call {BUS} /org/ilex/Installer1 {BUS} {method}{}{arguments}",
        if signature.is_empty() {
            String::new()
        } else {
            format!(" {signature}")
        }
    )
}

pub(crate) fn document(ran: &Ran, what: &str) -> Result<Value> {
    ensure!(
        ran.code == Some(0),
        "{what} failed ({:?}): {}",
        ran.code,
        ran.text
    );
    let reply: Value = serde_json::from_str(ran.text.trim())
        .with_context(|| format!("busctl's JSON for {what}: {}", ran.text))?;
    serde_json::from_str(reply["data"][0].as_str().context("busctl's data")?)
        .with_context(|| format!("{what}'s document"))
}

/// The refusal a failed call carries.
pub(crate) fn refusal(ran: &Ran) -> Result<PlanError> {
    ensure!(
        ran.code != Some(0),
        "the call was not refused: {}",
        ran.text
    );
    let json = ran
        .text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Call failed: "))
        .with_context(|| format!("busctl's error: {}", ran.text))?;
    serde_json::from_str(json).with_context(|| format!("the refusal's document: {json}"))
}

pub(crate) fn denied(ran: &Ran) -> bool {
    ran.code != Some(0) && ran.text.contains("Access denied")
}

fn probe(vm: &Vm) -> Result<Probe> {
    Ok(serde_json::from_value(document(
        &ssh(vm, &busctl("ilex-installer", "Probe", &[]))?,
        "Probe",
    )?)?)
}

fn state(vm: &Vm) -> Result<State> {
    Ok(serde_json::from_value(document(
        &ssh(vm, &busctl("ilex-installer", "State", &[]))?,
        "State",
    )?)?)
}

/// The two disks a trial works on, found by size: ids are whatever the
/// hardware gives virtio disks with no serial.
fn disks_of(probe: &Probe) -> Result<(Disk, Disk)> {
    let find = |bytes: u64| {
        probe
            .disks
            .iter()
            .find(|d| d.size_bytes == bytes)
            .cloned()
            .with_context(|| {
                format!(
                    "no disk of {bytes} bytes in the probe: {:?}",
                    probe
                        .disks
                        .iter()
                        .map(|d| (&d.id, d.size_bytes))
                        .collect::<Vec<_>>()
                )
            })
    };
    Ok((find(TARGET_BYTES)?, find(SENTINEL_BYTES)?))
}

fn request(disk: &str, edit: impl FnOnce(&mut Value)) -> String {
    let mut request = json!({
        "composition": "generic.gnome",
        "disk": disk,
        "mode": "whole-disk",
        "encrypt": false,
        "administrator": ADMIN,
    });
    edit(&mut request);
    request.to_string()
}

/// A plan, asked for again for a minute and a half while the live system has
/// no route to the registry yet.
fn plan(vm: &Vm, request: &str) -> Result<Plan> {
    let started = Instant::now();
    loop {
        let ran = ssh(vm, &busctl("ilex-installer", "Plan", &[request]))?;
        if ran.code == Some(0) {
            return Ok(serde_json::from_value(document(&ran, "Plan")?)?);
        }
        let retry = matches!(refusal(&ran), Ok(e) if e.code == ErrorCode::Registry)
            && started.elapsed() < Duration::from_secs(90);
        ensure!(retry, "Plan: {}", ran.text);
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// `SetSecret` as `user`: a memfd made by the caller, as the interface makes
/// it (`sealed` or not), handed over as a descriptor.
fn set_secret(
    vm: &Vm,
    user: &str,
    plan: &str,
    kind: &str,
    content: &str,
    sealed: bool,
) -> Result<Ran> {
    let python = format!(
        r#"import os, sys, base64, fcntl
fd = os.memfd_create("secret", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
os.write(fd, base64.b64decode(sys.argv[3] if len(sys.argv) > 3 else ""))
if {seal}:
    fcntl.fcntl(fd, 1033, 1 | 2 | 4 | 8)  # F_ADD_SEALS: seal, shrink, grow, write
os.set_inheritable(fd, True)
os.execvp("busctl", ["busctl", "--system", "--json=short", "--timeout=120", "call", "{BUS}", "/org/ilex/Installer1", "{BUS}", "SetSecret", "ssh", sys.argv[1], sys.argv[2], str(fd)])
"#,
        seal = if sealed { "True" } else { "False" }
    );
    ssh(
        vm,
        &format!(
            "runuser -u {user} -- python3 -c \"$(printf %s {} | base64 -d)\" {} {} {}",
            base64(python.as_bytes()),
            shell(plan),
            shell(kind),
            base64(content.as_bytes())
        ),
    )
}

pub(crate) fn shell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn apply(vm: &Vm, user: &str, plan: &str, erase: &str) -> Result<Ran> {
    ssh(vm, &busctl(user, "Apply", &[plan, erase]))
}

/// A refusal with the code and field wanted.
fn refused(ran: &Ran, code: ErrorCode, field: Option<&str>) -> Result<PlanError> {
    let error = refusal(ran)?;
    ensure!(
        error.code == code && field.is_none_or(|f| error.field.as_deref() == Some(f)),
        "wanted {code:?} ({field:?}), got {error:?}"
    );
    Ok(error)
}

// --- the disks, as they are ---------------------------------------------------

/// What is on the disks, as bytes: the first and last 16 MiB of each (where a
/// partition table, its backup and every first superblock live) and what the
/// kernel lists on each. A write to either changes it.
fn fingerprint(vm: &Vm, disks: &[&Disk]) -> Result<String> {
    let mut script = String::from("set -e\n");
    for disk in disks {
        let mib = disk.size_bytes >> 20;
        script.push_str(&format!(
            "echo {path}; dd if={path} bs=1M count=16 status=none | sha256sum; dd if={path} bs=1M skip={tail} count=16 status=none | sha256sum; lsblk -n -o NAME,TYPE,SIZE,FSTYPE,PARTLABEL {path}\n",
            path = disk.path,
            tail = mib - 16
        ));
    }
    ok(vm, &script)
}

fn no_logs(vm: &Vm, plan: &str) -> Result<()> {
    let ran = ssh(vm, &format!("test -e /run/ilex-installer/log/{plan}"))?;
    ensure!(ran.code != Some(0), "a refused Apply made logs for {plan}");
    Ok(())
}

/// After a refusal: nothing began, nothing was written, nothing is mounted.
fn nothing_happened(vm: &Vm, before: &str, disks: &[&Disk], plan: &str) -> Result<()> {
    let s = state(vm)?;
    ensure!(
        !s.active && s.outcome.is_none() && s.stage == Stage::Idle && !s.disk_changed,
        "a refusal left the helper in {s:?}"
    );
    ensure!(
        fingerprint(vm, disks)? == before,
        "a disk changed during a refused Apply"
    );
    no_logs(vm, plan)
}

// --- installer::policy ---------------------------------------------------------

fn policy() -> Result<()> {
    let bench = Bench::new("policy")?;
    let vm = bench.boot_live()?;
    let probe = probe(&vm)?;
    let (target, sentinel) = disks_of(&probe)?;
    let both = [&target, &sentinel];
    let before = fingerprint(&vm, &both)?;
    println!("     disks before: {}", before.replace('\n', " | "));

    let plan1 = plan(&vm, &request(&target.id, |_| {}))?;
    let id = plan1.id.clone();
    println!(
        "     planned {} from {} ({} bytes to download)",
        plan1.image.digest, plan1.image.source, plan1.image.download_bytes
    );
    let refuse =
        |what: &str, ran: Ran, code: ErrorCode, field: Option<&str>| -> Result<PlanError> {
            let error = refused(&ran, code, field).with_context(|| what.to_owned())?;
            println!("     refused ({what}): {code:?}: {}", error.message);
            nothing_happened(&vm, &before, &both, &id)?;
            Ok(error)
        };

    // No password yet.
    refuse(
        "no secret",
        apply(&vm, "ilex-installer", &id, &target.id)?,
        ErrorCode::MissingSecret,
        None,
    )?;
    // Secrets: the right kind, a sealed memfd, from a caller the bus lets in.
    refuse(
        "an unsealed memfd",
        set_secret(
            &vm,
            "ilex-installer",
            &id,
            "admin-password",
            PASSWORD,
            false,
        )?,
        ErrorCode::InvalidSecret,
        Some("fd"),
    )?;
    refuse(
        "an empty secret",
        set_secret(&vm, "ilex-installer", &id, "admin-password", "", true)?,
        ErrorCode::InvalidSecret,
        Some("fd"),
    )?;
    refuse(
        "a password with a line break",
        set_secret(&vm, "ilex-installer", &id, "admin-password", "a\nb", true)?,
        ErrorCode::InvalidSecret,
        Some("fd"),
    )?;
    refuse(
        "an unknown kind",
        set_secret(&vm, "ilex-installer", &id, "root-password", PASSWORD, true)?,
        ErrorCode::InvalidSecret,
        Some("kind"),
    )?;
    refuse(
        "a passphrase for a plan without encryption",
        set_secret(
            &vm,
            "ilex-installer",
            &id,
            "disk-passphrase",
            PASSWORD,
            true,
        )?,
        ErrorCode::InvalidSecret,
        Some("fd"),
    )?;
    refuse(
        "a secret for an unknown plan",
        set_secret(
            &vm,
            "ilex-installer",
            &"0".repeat(32),
            "admin-password",
            PASSWORD,
            true,
        )?,
        ErrorCode::UnknownPlan,
        Some("plan"),
    )?;
    for user in ["nobody", "daemon"] {
        ensure!(
            denied(&set_secret(
                &vm,
                user,
                &id,
                "admin-password",
                PASSWORD,
                true
            )?),
            "{user} may call SetSecret"
        );
        ensure!(
            denied(&apply(&vm, user, &id, &target.id)?),
            "{user} may call Apply"
        );
    }
    // Still no secret after all that.
    refuse(
        "no secret after refused ones",
        apply(&vm, "ilex-installer", &id, &target.id)?,
        ErrorCode::MissingSecret,
        None,
    )?;
    // The password.
    let ran = set_secret(&vm, "ilex-installer", &id, "admin-password", PASSWORD, true)?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);

    // The confirmation: Apply repeats the disk's id, and the right one.
    refuse(
        "no confirmation",
        apply(&vm, "ilex-installer", &id, "")?,
        ErrorCode::InvalidField,
        Some("erase_disk"),
    )?;
    refuse(
        "another disk's id",
        apply(&vm, "ilex-installer", &id, &sentinel.id)?,
        ErrorCode::InvalidField,
        Some("erase_disk"),
    )?;
    refuse(
        "a path for an id",
        apply(&vm, "ilex-installer", &id, &target.path)?,
        ErrorCode::InvalidField,
        Some("erase_disk"),
    )?;
    refuse(
        "an unknown plan",
        apply(&vm, "ilex-installer", &"1".repeat(32), &target.id)?,
        ErrorCode::UnknownPlan,
        Some("plan"),
    )?;

    // The signature. The image really is ilex's and really is signed: the
    // policy is what is wrong.
    let key = "/etc/pki/containers/ilex.pub";
    let registries = "/etc/containers/registries.d/ilex.yaml";
    ok(
        &vm,
        &format!(
            "cp -a {key} /run/ilex.pub.orig && cp -a {registries} /run/ilex.yaml.orig && openssl ecparam -name prime256v1 -genkey -noout -out /run/other.pem && openssl ec -in /run/other.pem -pubout -out {key} 2>/dev/null"
        ),
    )?;
    let wrong_key = refuse(
        "a key that is not ilex's",
        apply(&vm, "ilex-installer", &id, &target.id)?,
        ErrorCode::Signature,
        Some("composition"),
    )?;
    ensure!(
        wrong_key.message.contains("nothing was written")
            && wrong_key.message.contains(&plan1.image.source),
        "the refusal does not say what it refused: {}",
        wrong_key.message
    );
    ok(
        &vm,
        &format!("cp -a /run/ilex.pub.orig {key}; rm -f {registries}"),
    )?;
    let unsigned = refuse(
        "no signature is found",
        apply(&vm, "ilex-installer", &id, &target.id)?,
        ErrorCode::Signature,
        Some("composition"),
    )?;
    ensure!(
        unsigned.message.to_lowercase().contains("signature"),
        "the refusal for an image with no signature: {}",
        unsigned.message
    );
    ok(&vm, &format!("cp -a /run/ilex.yaml.orig {registries}"))?;

    // A plan is valid for as long as it says: the helper is restarted with
    // five seconds, and the plan is let to grow old.
    ok(
        &vm,
        "mkdir -p /run/systemd/system/ilex-installerd.service.d; printf '[Service]\\nExecStart=\\nExecStart=/usr/libexec/ilex-installer/ilex-installerd --plan-valid-seconds 5\\n' > /run/systemd/system/ilex-installerd.service.d/short.conf; systemctl daemon-reload; systemctl reset-failed ilex-installerd 2>/dev/null; systemctl restart ilex-installerd",
    )?;
    let short = plan(&vm, &request(&target.id, |_| {}))?;
    ensure!(
        short.valid_seconds == 5,
        "the plan's validity: {}",
        short.valid_seconds
    );
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &short.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    // The wall clock is stepped back two hours (an NTP step, a person setting
    // the date): a plan's age is the helper's own monotonic one, so it is still
    // seven seconds old, and expired.
    ok(&vm, "date -s \"@$(( $(date +%s) - 7200 ))\" >/dev/null")?;
    std::thread::sleep(Duration::from_secs(7));
    let id_short = short.id.clone();
    let ran = apply(&vm, "ilex-installer", &short.id, &target.id)?;
    ok(&vm, "date -s \"@$(( $(date +%s) + 7200 ))\" >/dev/null")?;
    let error = refused(&ran, ErrorCode::PlanExpired, Some("plan"))?;
    println!(
        "     refused (a stale plan, the clock stepped back): {}",
        error.message
    );
    nothing_happened(&vm, &before, &both, &id_short)?;
    ok(
        &vm,
        "rm -rf /run/systemd/system/ilex-installerd.service.d; systemctl daemon-reload; systemctl reset-failed ilex-installerd 2>/dev/null; systemctl restart ilex-installerd",
    )?;
    // A restarted helper has forgotten its plans.
    let ran = apply(&vm, "ilex-installer", &id, &target.id)?;
    refused(&ran, ErrorCode::UnknownPlan, Some("plan"))?;

    // The disk changes after the plan was made. The test makes the change,
    // so the disks differ from `before` by that and nothing else.
    let plan2 = plan(&vm, &request(&target.id, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan2.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    ok(
        &vm,
        &format!(
            "printf 'label: gpt\\nsize=64MiB, name=\"someone-elses\"\\n' | sfdisk --quiet {}; udevadm settle",
            target.path
        ),
    )?;
    let changed = fingerprint(&vm, &both)?;
    ensure!(
        changed != before,
        "the test's own change did not change the disk"
    );
    let ran = apply(&vm, "ilex-installer", &plan2.id, &target.id)?;
    let error = refused(&ran, ErrorCode::DiskChanged, Some("disk"))?;
    println!("     refused (the disk changed): {}", error.message);
    nothing_happened(&vm, &changed, &both, &plan2.id)?;

    // The disk is in use after the plan was made: a plan, then a mount.
    let plan3 = plan(&vm, &request(&target.id, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan3.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    ok(
        &vm,
        &format!(
            "mkfs.ext4 -q -F {p}1 && mkdir -p /run/inuse && mount {p}1 /run/inuse",
            p = target.path
        ),
    )?;
    let mounted = fingerprint(&vm, &both)?;
    let ran = apply(&vm, "ilex-installer", &plan3.id, &target.id)?;
    let error = refused(&ran, ErrorCode::InUse, Some("disk"))?;
    println!("     refused (the disk is in use): {}", error.message);
    nothing_happened(&vm, &mounted, &both, &plan3.id)?;
    ok(&vm, "umount /run/inuse")?;
    // A claim that is made in the moment between the helper's last look at the
    // disk and its own claim (the kernel's, with nothing the helper could
    // trace): the helper is restarted with a pause there (a hidden flag that
    // does nothing unless the file it names exists), `Apply` is let to reach it,
    // and the disk is then opened exclusively by someone else, or a partition is
    // mounted. The probe has already said "free", so the refusal can only be
    // the helper's own hold: a message that says the claim failed.
    ok(
        &vm,
        "mkdir -p /run/systemd/system/ilex-installerd.service.d; printf '[Service]\nExecStart=\nExecStart=/usr/libexec/ilex-installer/ilex-installerd --pause-before-hold /run/hold-pause\n' > /run/systemd/system/ilex-installerd.service.d/pause.conf; systemctl daemon-reload; systemctl reset-failed ilex-installerd 2>/dev/null; systemctl restart ilex-installerd",
    )?;
    let raced = |vm: &Vm, what: &str, race: &str, plan_for: &Plan| -> Result<PlanError> {
        ok(
            vm,
            &format!(
                "rm -f /run/hold-pause.reached /run/apply.out /run/held; touch /run/hold-pause; ( runuser -u ilex-installer -- env SYSTEMD_COLORS=0 SYSTEMD_PAGER= busctl --system --json=short --timeout=120 call {BUS} /org/ilex/Installer1 {BUS} Apply ss {} {} > /run/apply.out 2>&1; echo $? >> /run/apply.out ) >/dev/null 2>&1 &\nfor i in $(seq 1 300); do [ -e /run/hold-pause.reached ] && break; sleep 0.1; done; test -e /run/hold-pause.reached",
                shell(&plan_for.id),
                shell(&target.id)
            ),
        )
        .with_context(|| format!("{what}: Apply never reached the pause"))?;
        // Apply has probed the disk (free) and waits: now the race. What the
        // race itself writes (a mount touches a superblock) is not Apply's.
        ok(vm, race).with_context(|| format!("{what}: the race"))?;
        let before = fingerprint(vm, &both)?;
        ok(
            vm,
            "rm -f /run/hold-pause; for i in $(seq 1 600); do tail -n1 /run/apply.out 2>/dev/null | grep -qE '^[0-9]+$' && break; sleep 0.2; done; tail -n1 /run/apply.out | grep -qE '^[0-9]+$'",
        )
        .with_context(|| format!("{what}: Apply did not end"))?;
        let text = ok(vm, "cat /run/apply.out")?;
        let mut lines: Vec<&str> = text.lines().collect();
        let code: i32 = lines.pop().unwrap_or("").trim().parse().unwrap_or(-1);
        let ran = Ran {
            code: Some(code),
            text: lines.join("\n"),
        };
        let error = refusal(&ran).with_context(|| format!("{what}: {text}"))?;
        ensure!(
            error.code == ErrorCode::InUse
                && error.field.as_deref() == Some("disk")
                && error.message.contains("cannot be claimed"),
            "{what}: wanted the helper's own claim to refuse (InUse, \"cannot be claimed\"), got {error:?}"
        );
        println!("     refused ({what}): {}", error.message);
        nothing_happened(vm, &before, &both, &plan_for.id)?;
        Ok(error)
    };
    let plan4 = plan(&vm, &request(&target.id, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan4.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    raced(
        &vm,
        "the whole disk opened exclusively after the probe",
        &format!(
            "python3 -c 'import os,time; fd=os.open(\"{}\", os.O_RDONLY|os.O_EXCL); open(\"/run/held\", \"w\").write(str(os.getpid())); time.sleep(120)' >/dev/null 2>&1 & for i in $(seq 1 50); do [ -e /run/held ] && break; sleep 0.2; done; test -e /run/held",
            target.path
        ),
        &plan4,
    )?;
    ok(&vm, "kill $(cat /run/held); rm -f /run/held")?;
    // The plan was refused, not spent: the same one, the claim gone, is applied
    // as far as the pause (it is not let through: the file is made again, the
    // helper told to wait, and the plan is then abandoned by the restart below).
    let plan5 = plan(&vm, &request(&target.id, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan5.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    raced(
        &vm,
        "a partition mounted after the probe",
        &format!("mkdir -p /run/raced && mount {}1 /run/raced", target.path),
        &plan5,
    )?;
    ok(&vm, "umount /run/raced")?;
    ok(
        &vm,
        "rm -rf /run/systemd/system/ilex-installerd.service.d; systemctl daemon-reload; systemctl reset-failed ilex-installerd 2>/dev/null; systemctl restart ilex-installerd",
    )?;

    // A disk with no /dev/disk/by-id name is not written to: once the helper has
    // let go of its claim it addresses the disk by that name. This one (no
    // serial number: a bus path is all it has) can be probed and planned, and
    // `Apply` refuses it before anything is written.
    let bare = probe
        .disks
        .iter()
        .find(|d| d.size_bytes == BARE_BYTES)
        .cloned()
        .context("the disk without a serial number is not in the probe")?;
    ensure!(
        ssh(&vm, &format!("test -e /dev/disk/by-id/{}", shell(&bare.id)))?.code != Some(0),
        "the bare disk {} has a by-id name",
        bare.id
    );
    let all = [&target, &sentinel, &bare];
    let bare_plan = plan(&vm, &request(&bare.id, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &bare_plan.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    let bare_before = fingerprint(&vm, &all)?;
    let ran = apply(&vm, "ilex-installer", &bare_plan.id, &bare.id)?;
    let error = refused(&ran, ErrorCode::UnknownDisk, Some("disk"))?;
    ensure!(
        error.message.contains("by-id"),
        "the refusal of a disk with no stable name does not say why: {}",
        error.message
    );
    println!("     refused (no by-id name): {}", error.message);
    nothing_happened(&vm, &bare_before, &all, &bare_plan.id)?;

    // Nothing of the secrets is anywhere: the helper's files and the journal,
    // counted exactly (no hit is not "the output ends in a 0").
    let leaks = ok(
        &vm,
        &format!(
            "set -eu\nfiles=$(grep -rlF -e {p} /run/ilex-installer 2>/dev/null || true)\necho \"leaked_files=$(printf %s \"$files\" | grep -c . || true)\"\necho \"journal_hits=$(journalctl --no-pager | grep -cF -e {p} || true)\"",
            p = shell(PASSWORD)
        ),
    )?;
    ensure!(
        field(&leaks, "leaked_files")? == "0" && field(&leaks, "journal_hits")? == "0",
        "the password is in the helper's files or journal: {leaks}"
    );
    println!("     every refusal left the disks and the helper untouched");
    selinux::check(&vm, &bench.artifacts, "installer-policy")?;
    Ok(())
}

// --- installer::install ---------------------------------------------------------

/// The state of an installation as it ran: what the harness saw each time it
/// asked.
struct Trace {
    seen: Vec<(Stage, f64)>,
    last_print: Instant,
}

impl Trace {
    fn new() -> Self {
        Self {
            seen: Vec::new(),
            last_print: Instant::now() - Duration::from_secs(60),
        }
    }

    fn note(&mut self, s: &State) {
        self.seen.push((s.stage, s.progress));
        if self.last_print.elapsed() > Duration::from_secs(20) {
            println!(
                "     {} {:.0}% {}",
                s.stage.name(),
                s.progress * 100.0,
                s.message
            );
            self.last_print = Instant::now();
        }
    }
}

/// Ask for the state until the installation has ended.
fn wait_for_end(vm: &Vm, trace: &mut Trace, timeout: Duration) -> Result<State> {
    let started = Instant::now();
    loop {
        let s = state(vm)?;
        trace.note(&s);
        if !s.active && s.outcome.is_some() {
            return Ok(s);
        }
        ensure!(
            started.elapsed() < timeout,
            "the installation did not end in {timeout:?}: {s:?}"
        );
        std::thread::sleep(Duration::from_secs(3));
    }
}

fn install() -> Result<()> {
    let bench = Bench::new("install")?;
    let vm = bench.boot_live()?;
    let probe = probe(&vm)?;
    let (target, sentinel) = disks_of(&probe)?;
    let sentinel_before = fingerprint(&vm, &[&sentinel])?;
    // Answers that are not the defaults, so that applying them is visible.
    let plan = plan(
        &vm,
        &request(&target.id, |r| {
            r["keyboard"] = json!("de-latin1");
            r["language"] = json!("de_DE.UTF-8");
            r["time_zone"] = json!("Europe/Berlin");
            r["host_name"] = json!("ilextest");
        }),
    )?;
    bench.save("plan.json", &serde_json::to_string_pretty(&plan)?);
    println!(
        "     plan {}: {} ({} bytes) onto {} ({} partitions)",
        plan.id,
        plan.image.digest,
        plan.image.download_bytes,
        plan.disk.id,
        plan.partitions.len()
    );

    // The bus's signals, from before Apply.
    ok(
        &vm,
        "nohup busctl --system --json=short monitor org.ilex.Installer1 > /run/signals.json 2>/dev/null &\necho $! > /run/monitor.pid; sleep 1; echo monitoring",
    )?;
    // What answers mode leaves when it saw a request and handed over (two
    // digests, root's file, mode 600): the installation keeps them in the stamp.
    ok(
        &vm,
        &format!(
            "umask 077; printf '%s\\n' {SEEN_ONE} {SEEN_TWO} > /run/ilex-answers-seen; stat -c 'mode=%a owner=%U' /run/ilex-answers-seen"
        ),
    )?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    let started = document(
        &apply(&vm, "ilex-installer", &plan.id, &target.id)?,
        "Apply",
    )?;
    let started: State = serde_json::from_value(started)?;
    ensure!(
        started.active && started.applied.as_deref() == Some(&plan.id) && started.stages.len() == 6,
        "Apply's state: {started:?}"
    );

    // While it runs: nothing else starts, and the state is what an interface
    // that restarts re-attaches with.
    let mut trace = Trace::new();
    let mut tried = false;
    let started_at = Instant::now();
    let finished = loop {
        let s = state(&vm)?;
        trace.note(&s);
        if !s.active && s.outcome.is_some() {
            break s;
        }
        if !tried && s.stage == Stage::Image && s.progress > 0.0 {
            tried = true;
            let again = apply(&vm, "ilex-installer", &plan.id, &target.id)?;
            refused(&again, ErrorCode::InstallationActive, None)?;
            let replan = ssh(
                &vm,
                &busctl("ilex-installer", "Plan", &[&request(&target.id, |_| {})]),
            )?;
            refused(&replan, ErrorCode::InstallationActive, None)?;
            let reprobe = ssh(&vm, &busctl("ilex-installer", "Probe", &[]))?;
            refused(&reprobe, ErrorCode::InstallationActive, None)?;
            // A secret is not taken while an installation runs, for its own
            // plan or any other.
            let again = set_secret(
                &vm,
                "ilex-installer",
                &plan.id,
                "admin-password",
                "another password",
                true,
            )?;
            refused(&again, ErrorCode::InstallationActive, None)?;
            let on_disk: State =
                serde_json::from_str(&ok(&vm, "cat /run/ilex-installer/state.json")?)?;
            ensure!(
                on_disk.active && on_disk.stage == Stage::Image,
                "state.json while installing: {on_disk:?}"
            );
            println!(
                "     during the pull: Apply, Plan, Probe and SetSecret are refused; state.json says {}",
                on_disk.stage.name()
            );
        }
        ensure!(
            started_at.elapsed() < INSTALL_TIMEOUT,
            "the installation did not end: {s:?}"
        );
        std::thread::sleep(Duration::from_secs(3));
    };
    println!("     installed in {:?}", started_at.elapsed());
    bench.save("state.json", &serde_json::to_string_pretty(&finished)?);
    ensure!(
        finished.outcome == Some(Outcome::Success) && finished.stage == Stage::Done,
        "the installation ended: {:?}\n{}",
        finished.outcome,
        finished.log.join("\n")
    );
    ensure!(
        finished
            .stages
            .iter()
            .all(|r| r.status == StageStatus::Done && r.started.is_some() && r.finished.is_some())
            && finished.disk_changed,
        "the stage records: {:?}",
        finished.stages
    );
    // Progress is honest: stages in order, the pull's bar rising and never full
    // before the end.
    let order: Vec<Stage> = {
        let mut stages: Vec<Stage> = trace.seen.iter().map(|(s, _)| *s).collect();
        stages.dedup();
        stages
    };
    ensure!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "stages went backwards: {order:?}"
    );
    let image_bar: Vec<f64> = trace
        .seen
        .iter()
        .filter(|(s, _)| *s == Stage::Image)
        .map(|(_, p)| *p)
        .collect();
    ensure!(
        image_bar.windows(2).all(|w| w[0] <= w[1] + 1e-9)
            && image_bar.iter().filter(|p| **p > 0.0).count() >= 3
            && image_bar.iter().all(|p| *p <= 1.0),
        "the pull's progress: {image_bar:?}"
    );
    println!(
        "     stages seen: {order:?}; the pull's bar: {} readings up to {:.2}",
        image_bar.len(),
        image_bar.iter().copied().fold(0.0, f64::max)
    );

    // The signals the interface would have heard: busctl's JSON, one message
    // per line, parsed. Each of the six stages was announced by a `Progress`
    // (with a fraction in 0..=1, never going backwards across stages), and
    // exactly one `Finished` reported the success.
    let signals = ok(
        &vm,
        "kill $(cat /run/monitor.pid) || true; sleep 1; cat /run/signals.json",
    )?;
    bench.save("signals.json", &signals);
    let mut progress: Vec<(String, f64)> = Vec::new();
    let mut finished_signals: Vec<Outcome> = Vec::new();
    for line in signals.lines().filter(|l| l.trim_start().starts_with('{')) {
        let message: Value = serde_json::from_str(line)
            .with_context(|| format!("a line of busctl monitor: {line}"))?;
        if message["type"] != "signal" || message["interface"] != BUS {
            continue;
        }
        let data = &message["payload"]["data"];
        match message["member"].as_str() {
            Some("Progress") => {
                let stage = data[0].as_str().context("Progress's stage")?.to_owned();
                let fraction = data[1].as_f64().context("Progress's fraction")?;
                ensure!(
                    (0.0..=1.0).contains(&fraction) && data[2].is_string(),
                    "a Progress signal: {data}"
                );
                progress.push((stage, fraction));
            }
            Some("Finished") => finished_signals.push(serde_json::from_str(
                data[0].as_str().context("Finished's outcome")?,
            )?),
            _ => {}
        }
    }
    for stage in [
        "partition",
        "mount",
        "image",
        "configure",
        "account",
        "finalise",
    ] {
        ensure!(
            progress.iter().any(|(s, _)| s == stage),
            "no Progress signal for {stage}: {progress:?}"
        );
    }
    let order: Vec<&str> = {
        let mut stages: Vec<&str> = progress.iter().map(|(s, _)| s.as_str()).collect();
        stages.dedup();
        stages
    };
    ensure!(
        order
            == [
                "partition",
                "mount",
                "image",
                "configure",
                "account",
                "finalise"
            ],
        "the stages the signals announced, in order: {order:?}"
    );
    ensure!(
        finished_signals == [Outcome::Success],
        "exactly one Finished (success) was wanted: {finished_signals:?}"
    );

    // What is on the disk, from the live system.
    let inspect = ok(&vm, INSPECT)?;
    bench.save("inspect.txt", &inspect);
    println!(
        "{}",
        inspect
            .lines()
            .map(|l| format!("     | {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    inspected(&inspect, &plan)?;
    // The password is nowhere: not in the helper's files or journal, not on the
    // target (where the logs went), not in the state.
    // A mount that fails fails the script: no leak count is read from a target
    // that was not looked at.
    let leaks = ok(
        &vm,
        &format!(
            "set -eu\nmkdir -p /run/inspect\nmount -o ro /dev/disk/by-partlabel/ilex-root /run/inspect\ntrap 'umount /run/inspect' EXIT\nfiles=$(grep -rlF -e {p} /run/ilex-installer /run/inspect/ostree/deploy/default/var/log 2>/dev/null || true)\necho \"leaked_files=$(printf %s \"$files\" | grep -c . || true)\"\necho \"journal_hits=$(journalctl --no-pager | grep -cF -e {p} || true)\"",
            p = shell(PASSWORD)
        ),
    )?;
    ensure!(
        field(&leaks, "leaked_files")? == "0" && field(&leaks, "journal_hits")? == "0",
        "the password is in the logs, the state or the journal: {leaks}"
    );
    // The plan was spent when the installation began: the same one is unknown now.
    refused(
        &apply(&vm, "ilex-installer", &plan.id, &target.id)?,
        ErrorCode::UnknownPlan,
        Some("plan"),
    )?;
    ensure!(
        fingerprint(&vm, &[&sentinel])? == sentinel_before,
        "the sentinel disk changed"
    );
    ok(&vm, "sync")?;
    // The whole installation ran enforcing: nothing was denied.
    selinux::check(&vm, &bench.artifacts, "installer-install")?;
    drop(vm);

    // The disk, booted.
    let mut installed = bench.boot_target()?;
    check_installed(
        &|name, text| bench.save(name, text),
        &installed,
        &Expected {
            digest: &plan.image.digest,
            target: &plan.image.target,
            host: "ilextest",
            zone: "Europe/Berlin",
            locale: "de_DE.UTF-8",
            keymap: "de-latin1",
            layout: "de",
            admin: ADMIN,
            password: PASSWORD,
            stamp: &[SEEN_ONE, SEEN_TWO],
        },
    )?;
    gdm_screenshot(
        &mut installed,
        &bench.artifacts.join("installer-install-gdm.png"),
    )?;
    Ok(())
}

/// The target's file systems and the installed system's files, read-only from
/// the live system. One `key=value` per line.
const INSPECT: &str = r#"set -u
t=/run/inspect
mkdir -p $t
mount -o ro /dev/disk/by-partlabel/ilex-root $t
D=$(ls -d $t/ostree/deploy/default/deploy/*/ | head -1); D=${D%/}
echo "deployments=$(ls -d $t/ostree/deploy/default/deploy/*/ | wc -l)"
echo "origin=$(grep container-image-reference= $D.origin)"
echo "digest=$(ostree --repo=$t/ostree/repo show --print-metadata-key=ostree.manifest-digest $(basename $D | cut -d. -f1))"
echo "hostname=$(cat $D/etc/hostname)"
echo "locale=$(grep ^LANG= $D/etc/locale.conf)"
echo "keymap=$(grep ^KEYMAP= $D/etc/vconsole.conf)"
echo "xkblayout=$(grep ^XKBLAYOUT= $D/etc/vconsole.conf)"
echo "localtime=$(readlink $D/etc/localtime)"
echo "fstab_root_lines=$(grep -cE '^[^#[:space:]]+[[:space:]]+/[[:space:]]' $D/etc/fstab 2>/dev/null)"
echo "fstab_boot_lines=$(grep -cE '[[:space:]]/boot[[:space:]]' $D/etc/fstab 2>/dev/null)"
echo "default_target=$(readlink $D/etc/systemd/system/default.target || readlink $D/usr/lib/systemd/system/default.target)"
echo "setup=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])), sort_keys=True))' $D/etc/ilex/setup.json)"
echo "setup_mode=$(stat -c %a $D/etc/ilex/setup.json)"
echo "passwd=$(grep '^ilexmaint:' $D/etc/passwd)"
echo "shadow_hash_kind=$(grep '^ilexmaint:' $D/etc/shadow | cut -d: -f2 | cut -c1-3)"
echo "wheel=$(grep '^wheel:' $D/etc/group)"
echo "root_locked=$(grep '^root:' $D/etc/shadow | cut -d: -f2 | cut -c1-1)"
echo "home=$(stat -c '%u:%g %a' $t/ostree/deploy/default/var/home/ilexmaint)"
echo "home_label=$(ls -dZ $t/ostree/deploy/default/var/home/ilexmaint | cut -d' ' -f1)"
echo "shadow_label=$(ls -Z $D/etc/shadow | cut -d' ' -f1)"
echo "passwd_label=$(ls -Z $D/etc/passwd | cut -d' ' -f1)"
echo "setup_label=$(ls -Z $D/etc/ilex/setup.json | cut -d' ' -f1)"
echo "hostname_label=$(ls -Z $D/etc/hostname | cut -d' ' -f1)"
echo "logs=$(ls $t/ostree/deploy/default/var/log/ilex-installer | tr '\n' ' ')"
echo "plan_in_logs=$(python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print(d["id"])' $t/ostree/deploy/default/var/log/ilex-installer/plan.json)"
L=$t/ostree/deploy/default/var/log/ilex-installer
echo "provisional_written=$(grep -h 'recorded .* in the provisional /boot/ilex-install-request.sha256' $L/*-mount.log | sed 's/^.*recorded \([0-9]*\) digest.*$/\1/')"
echo "provisional_after_bootc=$(grep -h 'the provisional stamp on the new /boot is as it was written after bootc' $L/*-image.log | sed 's/^.*bootc (\([0-9]*\) digest.*$/\1/')"
P=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' $L/plan.json)
echo "checkpointed_provisional=$(grep -h 'checkpointed the journal of the new /boot' $L/*-mount.log | wc -l)"
echo "checkpointed_finished=$(grep -h 'checkpointed the journal of the new /boot' /run/ilex-installer/log/$P/*-finalise.log | wc -l)"
umount $t
echo "partitions=$(lsblk -n -o PARTLABEL,FSTYPE,SIZE -r /dev/disk/by-partlabel/ilex-root /dev/disk/by-partlabel/ilex-boot /dev/disk/by-partlabel/ilex-esp | tr '\n' ';')"
# Nothing of the installation is mounted: in the host's table and in the
# helper's own (it runs in a private mount namespace). The probe after a
# failed or finished installation (installer::interrupted) is the other proof.
helper=$(systemctl show -p MainPID --value ilex-installerd)
echo "mounted_after=$(cat /proc/1/mountinfo /proc/$helper/mountinfo | grep -c ilex-installer/target || true)"
echo "helper_namespace_read=$(grep -q . /proc/$helper/mountinfo 2>/dev/null && echo yes || echo no)"
"#;

fn field<'a>(text: &'a str, key: &str) -> Result<&'a str> {
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .with_context(|| format!("no {key} in the inspection"))
}

fn inspected(text: &str, plan: &Plan) -> Result<()> {
    let want = |key: &str, expected: &str| -> Result<()> {
        let got = field(text, key)?;
        ensure!(got == expected, "{key}: {got:?}, wanted {expected:?}");
        Ok(())
    };
    want("deployments", "1")?;
    want(
        "origin",
        &format!(
            "container-image-reference=ostree-image-signed:docker://{}",
            plan.image.target
        ),
    )?;
    want("digest", &format!("'{}'", plan.image.digest))?;
    want("hostname", "ilextest")?;
    want("locale", "LANG=de_DE.UTF-8")?;
    want("keymap", "KEYMAP=de-latin1")?;
    want("xkblayout", "XKBLAYOUT=de")?;
    want("localtime", "../usr/share/zoneinfo/Europe/Berlin")?;
    want("fstab_root_lines", "0")?;
    want("fstab_boot_lines", "1")?;
    ensure!(
        field(text, "default_target")?.ends_with("/graphical.target")
            || field(text, "default_target")? == "graphical.target",
        "default target: {}",
        field(text, "default_target")?
    );
    want("setup_mode", "644")?;
    let setup: Value = serde_json::from_str(field(text, "setup")?)?;
    ensure!(
        setup
            == json!({
                "schema": 1,
                "composition": "generic.gnome",
                "image": plan.image.target,
                "profile": {"id": null, "schema": null},
                "security": {"capabilities": {}, "tpm": {"requested": "unspecified", "capability": "unknown", "status": "not-configured"}},
            }),
        "setup.json: {setup}"
    );
    ensure!(
        field(text, "passwd")?.starts_with("ilexmaint:x:1000:1000:")
            && field(text, "passwd")?.ends_with(":/var/home/ilexmaint:/bin/bash"),
        "passwd: {}",
        field(text, "passwd")?
    );
    want("shadow_hash_kind", "$y$")?;
    ensure!(
        field(text, "wheel")?.split(':').next_back() == Some(ADMIN),
        "wheel: {}",
        field(text, "wheel")?
    );
    ensure!(
        field(text, "root_locked")? != "$",
        "root has a password hash"
    );
    want("home", "1000:1000 700")?;
    // Labeled after bootc: a file the live system wrote is `shadow_t`, not
    // unlabeled.
    ensure!(
        field(text, "shadow_label")?.contains("shadow_t"),
        "shadow: {}",
        field(text, "shadow_label")?
    );
    ensure!(
        field(text, "passwd_label")?.contains("passwd_file_t"),
        "passwd: {}",
        field(text, "passwd_label")?
    );
    ensure!(
        field(text, "home_label")?.contains("user_home_dir_t"),
        "home: {}",
        field(text, "home_label")?
    );
    ensure!(
        field(text, "hostname_label")?.contains("hostname_etc_t"),
        "hostname: {}",
        field(text, "hostname_label")?
    );
    ensure!(
        !field(text, "setup_label")?.contains("unlabeled"),
        "setup.json: {}",
        field(text, "setup_label")?
    );
    ensure!(
        field(text, "logs")?.contains("plan.json") && field(text, "logs")?.contains("-image.log"),
        "the logs copied to the target: {}",
        field(text, "logs")?
    );
    want("plan_in_logs", &plan.id)?;
    ensure!(
        ["ilex-esp;", "ilex-boot;", "ilex-root;"]
            .iter()
            .all(|p| field(text, "partitions")
                .is_ok_and(|v| v.contains(p) || v.contains(&p[..p.len() - 1]))),
        "partitions: {}",
        field(text, "partitions")?
    );
    // The provisional stamp (the two digests the test put in the record of
    // answers seen, and not the request's own) was written on the new `/boot`
    // before the pull, and `bootc` left it as it was.
    want("provisional_written", "2")?;
    want("provisional_after_bootc", "2")?;
    // The journal of the new `/boot` was checkpointed after each stamp (the
    // provisional one, and the finished one over it, labelled), so that a reader
    // that does not replay the journal finds them after a power cut.
    want("checkpointed_provisional", "1")?;
    want("checkpointed_finished", "1")?;
    want("mounted_after", "0")?;
    want("helper_namespace_read", "yes")?;
    Ok(())
}

/// What an installed system must show once it has booted.
pub(crate) struct Expected<'a> {
    /// The manifest digest the plan pinned.
    pub digest: &'a str,
    /// What the machine tracks: `ghcr.io/reidond/ilex:<tag>`.
    pub target: &'a str,
    pub host: &'a str,
    pub zone: &'a str,
    pub locale: &'a str,
    pub keymap: &'a str,
    pub layout: &'a str,
    pub admin: &'a str,
    pub password: &'a str,
    /// The digests the stamp on `/boot` holds before this installation's own
    /// (the last line): what the disk's earlier stamp held and what answers
    /// mode had seen in the boot, in that order, each once.
    pub stamp: &'a [&'a str],
}

/// The installed disk, booted. `save` keeps evidence under a name.
pub(crate) fn check_installed(save: &dyn Fn(&str, &str), vm: &Vm, e: &Expected<'_>) -> Result<()> {
    // Everything below is asserted against this one boot.
    let mut failures: Vec<String> = Vec::new();
    let mut check = |name: &str, result: Result<()>| match result {
        Ok(()) => println!("ok   {name}"),
        Err(error) => {
            println!("FAIL {name}: {error:#}");
            failures.push(format!("{name}: {error:#}"));
        }
    };
    check(
        "the system reaches running",
        (|| {
            let out = vm.ssh("timeout 300 systemctl is-system-running --wait")?;
            let state = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            ensure!(state == "running", "the system is `{state}`");
            Ok(())
        })(),
    );
    check(
        "no failed units",
        (|| {
            let failed = vm.ssh_ok("systemctl --failed --no-legend --plain")?;
            if !failed.is_empty() {
                // What a person would ask next: why. The journal of each, and the
                // denials this boot made.
                let why = vm.ssh(
                "for u in $(systemctl --failed --no-legend --plain | cut -d' ' -f1); do systemctl status \"$u\" --no-pager; journalctl -b -u \"$u\" --no-pager | tail -25; done; journalctl -b --no-pager -g 'avc: +denied' | tail -20",
            )?;
                save("failed-units.txt", &String::from_utf8_lossy(&why.stdout));
                bail!(
                    "failed units:\n{failed}\n{}",
                    String::from_utf8_lossy(&why.stdout)
                );
            }
            Ok(())
        })(),
    );
    check(
        "the display manager is up",
        (|| {
            // The GNOME edition installs a GNOME composition: its greeter is GDM,
            // whatever the harness's own checkout builds; the KDE edition's is
            // Plasma Login Manager.
            let greeter = if e.target.ends_with(".kde") {
                "plasmalogin.service"
            } else {
                "gdm.service"
            };
            let out = vm.ssh_ok("systemctl show -p Id --value display-manager.service; systemctl is-active display-manager.service; systemctl get-default")?;
            ensure!(
                out == format!("{greeter}\nactive\ngraphical.target"),
                "the display manager: {out}"
            );
            Ok(())
        })(),
    );
    check(
        "the layout is recorded",
        (|| {
            save("layout.txt", &vm.ssh_ok(common::LAYOUT_RECORD)?);
            Ok(())
        })(),
    );
    check(
        "the digests are stamped on /boot (root's, mode 600, one 64-digit digest a line: the earlier ones, then this request's)",
        (|| {
            let out = vm.ssh_ok(
                "f=/boot/ilex-install-request.sha256; stat -c 'mode=%a owner=%U links=%h' $f; ls -Z $f; cat $f",
            )?;
            save("request-stamp.txt", &out);
            let digests: Vec<&str> = out.lines().skip(2).collect();
            ensure!(
                out.contains("mode=600 owner=root links=1")
                    && digests.iter().all(|d| d.len() == 64
                        && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
                    && digests.len() == e.stamp.len() + 1
                    && digests[..e.stamp.len()] == *e.stamp,
                "the stamp on /boot (wanted {:?} and the request's own): {out}",
                e.stamp
            );
            Ok(())
        })(),
    );
    check(
        "the booted digest is the one the plan pinned, enforcing the container policy",
        (|| {
            let status: Value = serde_json::from_str(&vm.ssh_ok("bootc status --json")?)?;
            save("bootc-status.json", &serde_json::to_string_pretty(&status)?);
            let booted = &status["status"]["booted"];
            ensure!(
                booted["image"]["imageDigest"] == e.digest,
                "booted {}, planned {}",
                booted["image"]["imageDigest"],
                e.digest
            );
            // The release gate names the digest it verified in the registry
            // (`ILEX_ISO_DIGEST`, the KDE edition's `ILEX_ISO_DIGEST_KDE`): the
            // plan has to have pinned exactly that one.
            let variable = if e.target.ends_with(".kde") {
                "ILEX_ISO_DIGEST_KDE"
            } else {
                "ILEX_ISO_DIGEST"
            };
            if let Some(verified) = std::env::var(variable).ok().filter(|v| !v.is_empty()) {
                ensure!(
                    e.digest == verified,
                    "the installer pinned {}, not the verified published digest {verified}",
                    e.digest
                );
            }
            ensure!(
                booted["image"]["image"]["image"] == e.target,
                "tracking {}",
                booted["image"]["image"]["image"]
            );
            let text = status.to_string();
            ensure!(
                text.contains("\"signature\":\"containerPolicy\""),
                "the origin does not enforce the container signature policy: {text}"
            );
            Ok(())
        })(),
    );
    check(
        "the answers are applied",
        (|| {
            let out = vm.ssh_ok("printf 'host=%s\\n' \"$(hostnamectl --static)\"; timedatectl show -p Timezone; localectl status | grep -E 'System Locale|VC Keymap|X11 Layout' | sed 's/^ *//'")?;
            println!("     {}", out.replace('\n', " | "));
            ensure!(
                out.contains(&format!("host={}", e.host))
                    && out.contains(&format!("Timezone={}", e.zone))
                    && out.contains(&format!("LANG={}", e.locale))
                    && out.contains(&format!("VC Keymap: {}", e.keymap))
                    && out.contains(&format!("X11 Layout: {}", e.layout)),
                "{out}"
            );
            Ok(())
        })(),
    );
    check(
        "the administrator logs in over SSH with the password and sudo works",
        (|| {
            let out = password_ssh(
                vm,
                e.admin,
                e.password,
                &format!(
                    "id -un; id -nG; printf %s {} | sudo -S -p '' id -u",
                    shell(e.password)
                ),
            )?;
            println!("     {}", out.replace('\n', " | "));
            let mut lines = out.lines();
            ensure!(lines.next() == Some(e.admin), "logged in as: {out}");
            ensure!(
                lines
                    .next()
                    .is_some_and(|g| g.split(' ').any(|g| g == "wheel")),
                "groups: {out}"
            );
            ensure!(lines.next() == Some("0"), "sudo: {out}");
            ensure!(
                password_ssh(vm, e.admin, "not the password", "true").is_err(),
                "a wrong password logged in"
            );
            Ok(())
        })(),
    );
    check(
        "what the installer wrote is labeled: no denial on an unlabeled file, none on its lock file, and restorecon would change nothing",
        (|| {
            // The OS image has denials of its own on every boot (`chcon` by tuned,
            // a unit looking at a unit file); the installer's are the ones about a
            // file it wrote and did not label (`unlabeled_t`) or labeled wrongly
            // (`/etc/.pwd.lock`, which useradd leaves and DynamicUser needs).
            let out = vm.ssh("journalctl -b --no-pager -g 'avc: +denied' > /run/avc.txt; echo total=$(wc -l < /run/avc.txt); echo ours=$(grep -cE 'unlabeled_t|\\.pwd\\.lock' /run/avc.txt); grep -E 'unlabeled_t|\\.pwd\\.lock' /run/avc.txt | head -5; echo ---; cat /run/avc.txt | sed 's/^.*avc: *denied/denied/' | cut -c1-200 | sort | uniq -c | sort -rn | head -12; echo ---; restorecon -n -v -r /etc /var/home /var/log/ilex-installer > /run/restorecon.txt 2>&1; echo restorecon_exit=$?; restorecon -n -v /boot /boot/efi >> /run/restorecon.txt 2>&1; echo restorecon_boot_exit=$?; head -20 /run/restorecon.txt")?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            save("avc.txt", &text);
            ensure!(
                text.contains("ours=0")
                    && text.contains("restorecon_exit=0")
                    && text.contains("restorecon_boot_exit=0")
                    && !text.contains("Would relabel"),
                "a file the installer wrote is not labeled (or restorecon failed):\n{text}"
            );
            Ok(())
        })(),
    );
    check(
        "bootc upgrade --check reaches the registry and verifies",
        (|| {
            let out = vm.ssh("timeout 300 bootc upgrade --check 2>&1")?;
            let text = String::from_utf8_lossy(&out.stdout).into_owned();
            ensure!(out.status.success(), "{text}");
            Ok(())
        })(),
    );
    ensure!(
        failures.is_empty(),
        "{} check(s) failed:\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
    Ok(())
}

/// The installed system's login screen, once GDM has drawn it: the screenshot
/// the pull request shows.
pub(crate) fn gdm_screenshot(vm: &mut Vm, path: &Path) -> Result<()> {
    const TIMEOUT: Duration = Duration::from_secs(300);
    vm.wait_for(
        "a GDM greeter session",
        "loginctl list-sessions --no-legend | grep -w greeter",
        TIMEOUT,
    )?;
    let ctl = Vmctl::from_info(vm.info.clone());
    ctl.wait_screen(
        &WaitCond {
            nonblank: true,
            stable: Some(Duration::from_secs(5)),
            ..WaitCond::default()
        },
        TIMEOUT,
    )
    .context("waiting for GDM's login screen")?;
    let frame = ctl.save_screenshot(path)?;
    println!(
        "     screenshot {} ({}x{})",
        path.display(),
        frame.width,
        frame.height
    );
    Ok(())
}

/// One command over SSH logging in with a password instead of the key.
pub(crate) fn password_ssh(vm: &Vm, user: &str, password: &str, script: &str) -> Result<String> {
    let dir = tempdir()?;
    let askpass = dir.join("askpass");
    fs::write(
        &askpass,
        "#!/bin/sh\nprintf '%s\\n' \"$ILEX_TEST_PASSWORD\"\n",
    )?;
    cmd_chmod(&askpass)?;
    let out = Command::new("ssh")
        .env("SSH_ASKPASS", &askpass)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("ILEX_TEST_PASSWORD", password)
        .env_remove("DISPLAY")
        .args([
            "-o",
            "PreferredAuthentications=password",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "NumberOfPasswordPrompts=1",
            "-o",
            "ConnectTimeout=10",
            "-p",
            &vm.info.ssh_port.to_string(),
        ])
        .arg(format!("{user}@127.0.0.1"))
        .arg(script)
        .stdin(std::process::Stdio::null())
        .output()
        .context("running ssh")?;
    let _ = fs::remove_dir_all(&dir);
    ensure!(
        out.status.success(),
        "ssh as {user} failed ({}): {}{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn tempdir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!(
        "ilex-askpass-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn cmd_chmod(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

// --- installer::interrupted -------------------------------------------------------

fn interrupted() -> Result<()> {
    let bench = Bench::new("interrupted")?;
    let vm = bench.boot_live()?;
    let probe = probe(&vm)?;
    let (target, sentinel) = disks_of(&probe)?;
    let sentinel_before = fingerprint(&vm, &[&sentinel])?;
    // The target already holds an installation's `/boot`, with the stamp it
    // left (two digests of answers that installed it before). The first
    // installation here erases it; the retry that follows reads the empty
    // `ilex-boot` the first one made, so only what the first one kept in /run
    // can carry the digests into the retry's own stamp.
    ok(&vm, &stamped_boot(&target.path))?;
    let probe = probe_after(&vm)?;
    let (target, sentinel) = disks_of(&probe)?;
    ensure!(
        target.partitions.iter().any(|p| p.label == "ilex-boot"),
        "the stamped /boot is not on the target: {:?}",
        target.partitions
    );
    let first = plan(&vm, &request(&target.id, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &first.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    document(
        &apply(&vm, "ilex-installer", &first.id, &target.id)?,
        "Apply",
    )?;

    // In the middle of the pull: some of it is on the disk.
    let mut trace = Trace::new();
    let started = Instant::now();
    loop {
        let s = state(&vm)?;
        trace.note(&s);
        ensure!(
            s.active,
            "the installation ended before it could be interrupted: {s:?}"
        );
        if s.stage == Stage::Image && s.progress > 0.05 {
            break;
        }
        ensure!(
            started.elapsed() < INSTALL_TIMEOUT,
            "the pull never got going: {s:?}"
        );
        std::thread::sleep(Duration::from_secs(2));
    }
    // The disk was wiped, and the stamp's digests were kept in /run first
    // (root's, mode 600).
    let kept = ok(
        &vm,
        "stat -c 'mode=%a owner=%U' /run/ilex-installer-earlier; cat /run/ilex-installer-earlier",
    )?;
    bench.save("earlier-kept.txt", &kept);
    ensure!(
        kept == format!("mode=600 owner=root\n{OLD_ONE}\n{OLD_TWO}"),
        "what the installation kept of the disk's stamp before it wiped it: {kept}"
    );
    // The helper is killed as a crash would: no chance to say anything. The
    // tools it started go with it.
    let main_pid = ok(&vm, "systemctl show -p MainPID --value ilex-installerd")?;
    ok(
        &vm,
        "systemctl kill --signal=SIGKILL --kill-whom=main ilex-installerd",
    )?;
    println!(
        "     killed the helper (pid {main_pid}) at the pull's {:.0}%",
        trace.seen.last().map_or(0.0, |s| s.1 * 100.0)
    );
    // systemd starts it again; it reads what the dead one left.
    let after = {
        let started = Instant::now();
        loop {
            let ran = ssh(&vm, &busctl("ilex-installer", "State", &[]))?;
            if ran.code == Some(0) {
                let s: State = serde_json::from_value(document(&ran, "State")?)?;
                let new_pid = ok(&vm, "systemctl show -p MainPID --value ilex-installerd")?;
                if new_pid != main_pid && new_pid != "0" {
                    break s;
                }
            }
            ensure!(
                started.elapsed() < Duration::from_secs(60),
                "the helper did not come back: {}",
                ran.text
            );
            std::thread::sleep(Duration::from_secs(1));
        }
    };
    bench.save(
        "state-after-kill.json",
        &serde_json::to_string_pretty(&after)?,
    );
    ensure!(
        !after.active
            && after.disk_changed
            && matches!(&after.outcome, Some(Outcome::Failed { stage: Stage::Image, reason })
                if reason.contains("stopped") && reason.contains("disk was already changed")),
        "the state a killed helper leaves: {after:?}"
    );
    ensure!(
        after
            .stages
            .iter()
            .any(|r| r.stage == Stage::Image && r.status == StageStatus::Failed)
            && after
                .stages
                .iter()
                .any(|r| r.stage == Stage::Partition && r.status == StageStatus::Done),
        "the stage records: {:?}",
        after.stages
    );
    println!(
        "     State after the restart: {}",
        match &after.outcome {
            Some(Outcome::Failed { reason, .. }) => reason.as_str(),
            _ => "?",
        }
    );
    // Nothing resumed by itself, no tool is left writing, nothing is mounted.
    std::thread::sleep(Duration::from_secs(3));
    // (What the killed helper had mounted lived in its own mount namespace,
    // which died with it: no table of this system can show it. That it holds
    // nothing is what the probe below says of the disk: not in use, by the
    // trace and by the exclusive open.)
    let left = ssh(
        &vm,
        "pgrep -a 'bootc|systemd-repart|skopeo|ostree' || echo none; findmnt -rn -S /dev/disk/by-partlabel/ilex-root || echo unmounted",
    )?;
    println!("     after the kill: {}", left.text.replace('\n', " | "));
    ensure!(
        left.text.contains("none") && left.text.contains("unmounted"),
        "a tool of the dead installation is still running, or its mounts are: {}",
        left.text
    );
    ensure!(
        document(&ssh(&vm, &busctl("ilex-installer", "State", &[]))?, "State")
            .is_ok_and(|v| v["active"] == false),
        "a restarted helper began again by itself"
    );
    // The old plan and its password are gone with the process.
    let ran = apply(&vm, "ilex-installer", &first.id, &target.id)?;
    refused(&ran, ErrorCode::UnknownPlan, Some("plan"))?;
    // The helper had mounted the new `/boot` when it died: the stamp the wipe
    // took is back on it, provisional (the two old digests, written before the
    // pull, and not the request being installed), root's, mode 600; and the
    // record in /run, which outlives the helper, still lists them.
    let left = ok(
        &vm,
        &format!("{LOOK_AT_STAMP}; echo kept=$(tr '\\n' ' ' < /run/ilex-installer-earlier)"),
    )?;
    bench.save("stamp-after-kill.txt", &left);
    ensure!(
        left == format!("mode=600 owner=root\n{OLD_ONE}\n{OLD_TWO}\nkept={OLD_ONE} {OLD_TWO}"),
        "the provisional stamp on the retry's disk, or the record that outlives the helper: {left}"
    );
    // A reboot before any retry: the machine is stopped as a power cut would
    // (the guest is not shut down) and the live system starts again. `/run` is
    // new, so only the disk can still name the old digests.
    selinux::check(&vm, &bench.artifacts, "installer-interrupted-killed")?;
    drop(vm);
    let vm = bench.boot_live()?;
    ensure!(
        ok(
            &vm,
            "test -e /run/ilex-installer-earlier && echo present || echo absent"
        )? == "absent",
        "the record of kept digests survived a reboot"
    );
    let after_reboot = ok(&vm, LOOK_AT_STAMP)?;
    bench.save("stamp-after-reboot.txt", &after_reboot);
    ensure!(
        after_reboot == format!("mode=600 owner=root\n{OLD_ONE}\n{OLD_TWO}"),
        "the disk's stamp after the reboot does not name the old digests: {after_reboot}"
    );
    println!("     after a reboot the new /boot's stamp still names the two old digests");

    // A retry: a new plan sees what the dead installation left on the disk
    // (ilex's own partitions, nothing in use), says so and installs over it.
    let probe = probe_after(&vm)?;
    let (target, _) = disks_of(&probe)?;
    ensure!(
        target.in_use.is_empty() && !target.live_medium,
        "the disk is not free after the kill: {target:?}"
    );
    ensure!(
        target.partitions.iter().any(|p| p.label == "ilex-root"),
        "the interrupted installation left no ilex partitions: {:?}",
        target.partitions
    );
    let second = plan(&vm, &request(&target.id, |_| {}))?;
    ensure!(
        !second.erases.systems.is_empty(),
        "the second plan does not say what it erases: {:?}",
        second.erases
    );
    println!(
        "     the second plan erases: {:?}",
        second
            .erases
            .systems
            .iter()
            .map(|s| &s.description)
            .collect::<Vec<_>>()
    );
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &second.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    document(
        &apply(&vm, "ilex-installer", &second.id, &target.id)?,
        "Apply",
    )?;
    let finished = wait_for_end(&vm, &mut trace, INSTALL_TIMEOUT)?;
    bench.save(
        "state-retry.json",
        &serde_json::to_string_pretty(&finished)?,
    );
    ensure!(
        finished.outcome == Some(Outcome::Success),
        "the retry ended: {:?}\n{}",
        finished.outcome,
        finished.log.join("\n")
    );
    ensure!(
        fingerprint(&vm, &[&sentinel])? == sentinel_before,
        "the sentinel disk changed"
    );
    ok(&vm, "sync")?;
    selinux::check(&vm, &bench.artifacts, "installer-interrupted-retry")?;
    drop(vm);
    let installed = bench.boot_target()?;
    let status: Value = serde_json::from_str(&installed.ssh_ok("bootc status --json")?)?;
    ensure!(
        status["status"]["booted"]["image"]["imageDigest"] == second.image.digest.as_str(),
        "the retried system's digest: {}",
        status["status"]["booted"]["image"]["imageDigest"]
    );
    let out = installed.ssh("timeout 300 systemctl is-system-running --wait")?;
    ensure!(
        String::from_utf8_lossy(&out.stdout).trim() == "running",
        "the retried system is not running: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let login = password_ssh(&installed, ADMIN, PASSWORD, "id -un")?;
    ensure!(
        login == ADMIN,
        "the retried system's administrator: {login}"
    );
    // The retry, in a boot of its own, read the provisional stamp from its disk
    // (the record in /run was gone), and its own stamp holds what the erased one
    // did: both old digests, then the request's own.
    let stamp = installed.ssh_ok(
        "f=/boot/ilex-install-request.sha256; stat -c 'mode=%a owner=%U links=%h' $f; cat $f",
    )?;
    bench.save("request-stamp.txt", &stamp);
    let lines: Vec<&str> = stamp.lines().collect();
    ensure!(
        lines.len() == 4
            && lines[0] == "mode=600 owner=root links=1"
            && lines[1] == OLD_ONE
            && lines[2] == OLD_TWO
            && lines[3].len() == 64
            && lines[3]
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            && lines[3] != OLD_ONE
            && lines[3] != OLD_TWO,
        "the stamp of the retried installation does not hold the digests the erased stamp held, then its own: {stamp}"
    );
    println!(
        "     the retried installation's stamp holds the erased stamp's two digests and its own"
    );
    Ok(())
}

/// Read the stamp of the new `/boot` the way answers mode does (read-only, no
/// journal replay): its mode and owner, then its lines. Fails when there is no
/// stamp.
const LOOK_AT_STAMP: &str = "set -e; d=/dev/disk/by-partlabel/ilex-boot; mkdir -p /run/stamp-look; mount -t ext4 -o ro,noload,nodev,nosuid,noexec $d /run/stamp-look; f=/run/stamp-look/ilex-install-request.sha256; stat -c 'mode=%a owner=%U' $f; cat $f; umount /run/stamp-look";

/// A script for the live system: the disk `path` gets a GPT with one
/// `ilex-boot` partition (an XBOOTLDR partition with an ext4 file system) that
/// holds a stamp of [`OLD_ONE`] and [`OLD_TWO`], as an earlier installation
/// leaves it (root's, mode 600).
fn stamped_boot(path: &str) -> String {
    format!(
        r#"set -eu
disk={disk}
printf 'label: gpt\nsize=1GiB, type=BC13C2FF-59E6-4262-A352-B275FD6F7172, name="ilex-boot"\n' | sfdisk --quiet "$disk"
udevadm settle --timeout=30
part=$(lsblk -n -r -o PATH,PARTLABEL "$disk" | awk '$2=="ilex-boot" {{print $1; exit}}')
test -n "$part"
mkfs.ext4 -q -L ilex-boot "$part"
mkdir -p /run/stamp-seed
mount -t ext4 "$part" /run/stamp-seed
umask 077
printf '%s\n' {OLD_ONE} {OLD_TWO} > /run/stamp-seed/ilex-install-request.sha256
sync
umount /run/stamp-seed
udevadm settle --timeout=30
echo stamped"#,
        disk = shell(path)
    )
}

/// A probe, asked again for a few seconds while the restarted helper settles.
fn probe_after(vm: &Vm) -> Result<Probe> {
    let started = Instant::now();
    loop {
        match probe(vm) {
            Ok(p) => return Ok(p),
            Err(e) if started.elapsed() < Duration::from_secs(60) => {
                let _ = e;
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => return Err(e),
        }
    }
}
