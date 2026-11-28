//! The security chip and the recovery key, on the live ISO: KVM with enrolled
//! OVMF (Secure Boot on, the Microsoft-enrolled variable store) and a
//! persistent swtpm (T6's `VmSpec`), the way `security::firmware` boots. A
//! backend that cannot (no KVM SMM, no swtpm) leaves the trial **unqualified**
//! (`next-security-result.json`, stage `platform`) and the trial failed: Secure
//! Boot and SMM are never turned off to get a pass.
//!
//! * `iso::next_security`: an encrypted installation by a person at the
//!   interface (`Plan` with a recovery key and `tpm=auto`, the passphrase and
//!   the administrator's password as sealed memfds, `Apply`), as
//!   `ilex-installer` makes it, except that the interface is a script. Its
//!   second step is the one the page asks of a person: the recovery key is
//!   taken from the helper (once, through the descriptor it returns: a second
//!   request is refused), two groups of it that are wrong are refused, the
//!   right two are confirmed. The final state says the security chip was
//!   enrolled. The installed disk is then started cold with **nobody at the
//!   keyboard**: it reaches SSH, the unlock record says the security chip
//!   opened it, `cryptsetup luksDump` shows exactly one TPM token bound to PCR 7
//!   of the SHA-256 bank with no PIN and no signed or pcrlock policy, the
//!   recovery token, three keyslots and no receipt; `ilexctl security verify`
//!   says so too. A second boot of the same disk on a fresh TPM (PCR 7 is the
//!   same, the sealed secret is not) shows the passphrase prompt, and the
//!   **recovery key typed at it** opens the disk.
//! * `iso::next_security_off`: the same installation with Secure Boot **off**:
//!   the plan says the chip will not be used, and why; the installation ends
//!   `passphrase-only` with the reason `secure-boot-off`, `setup.json` and the
//!   enrolment record say so, and the disk opens with the recovery key typed
//!   at its prompt.
//!
//! In both, nothing the helper writes or prints holds the passphrase or the
//! recovery key: the helper's run directory, its journal, the kernel log, the
//! process list, the log saved to a stick (a tarball, scanned extracted and
//! raw), the installed system's logs, `/etc` and `/var/lib`, and every file
//! this trial saves on the host. The key and the passphrase are in the trial's
//! memory and in the guest only where a person would type them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::core::{
    Capability, Requirement, SecurityLaunch, TpmLifecycle, VmProvider, VmSpec,
};
use ilex_devkit::installer_model::recovery;
use ilex_devkit::provider::{LinuxProvider, LinuxVm};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::Vmctl;
use serde_json::{Value, json};

use super::{iso_path, wait_firmware_output};
use crate::common;
use crate::installer::apply::encrypted::{self, US, fat_stick};
use crate::installer::apply::{ADMIN, Expected, PASSWORD, busctl, check_installed, document, ssh};
use crate::selinux;

/// The target's virtual size, as the other `iso::next` trials make it.
const TARGET_BYTES: u64 = 41 << 30;
const INSTALL_TIMEOUT: Duration = Duration::from_secs(40 * 60);
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);
const HOST: &str = "ilexsec";
const TARGET_REF: &str = "ghcr.io/reidond/ilex:generic.gnome";
/// The raw D-Bus client the guest runs (see the file's own comment).
const RAWBUS: &str = include_str!("rawbus.py");

pub(super) fn run() -> Result<()> {
    scenario(true)
}

pub(super) fn run_off() -> Result<()> {
    scenario(false)
}

/// A VM of the trial: the provider that made it, its spec (the identity of its
/// disk and TPM bundle) and the directory of its files.
struct Machine {
    provider: LinuxProvider,
    vm: LinuxVm,
    spec: VmSpec,
    root: PathBuf,
}

impl Drop for Machine {
    fn drop(&mut self) {
        match self.provider.destroy_owned(&mut self.vm) {
            Ok(()) => {
                let _ = fs::remove_dir_all(&self.root);
            }
            Err(e) => eprintln!(
                "iso::next_security fixture cleanup: {e:#}; retained {}",
                self.root.display()
            ),
        }
    }
}

impl Machine {
    /// The ISO over a blank target (and a stick for the log), UEFI, with
    /// Secure Boot as asked and a persistent TPM.
    fn boot_iso(root: &Path, secure: bool, artifacts: &Path) -> Result<Self> {
        let ws = common::workspace()?;
        let target = root.join("target.raw");
        let stick = root.join("stick.img");
        let _ = fs::remove_file(&target);
        fs::File::create(&target)
            .and_then(|file| file.set_len(TARGET_BYTES))
            .with_context(|| format!("creating {}", target.display()))?;
        fat_stick(&stick)?;
        let provider = LinuxProvider::new(ws.clone())?
            .with_disk_serials(true)
            .with_usb_sticks(vec![stick]);
        let mut config = VmConfig::new("iso-next-sec", &target);
        config.cdrom = Some(iso_path()?);
        config.snapshot = false;
        config.memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| "6G".into());
        let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
        spec.security_launch = SecurityLaunch::QualificationFixture;
        spec.secure_boot = if secure {
            Requirement::Required
        } else {
            Requirement::Off
        };
        spec.tpm = TpmLifecycle::Persistent {
            bundle_id: spec.run.clone(),
        };
        // The platform: what cannot be provisioned leaves the run unqualified
        // (nothing is turned off to continue).
        let platform = (|| -> Result<()> {
            let caps = provider.probe(&spec)?;
            fs::write(
                artifacts.join("next-security-provisioning.json"),
                serde_json::to_vec_pretty(&caps)?,
            )?;
            if secure {
                caps.require_provisioning(Capability::FirmwareEnforcement)?;
            }
            caps.require_provisioning(Capability::PersistentTpm)
        })();
        if let Err(error) = platform {
            unqualified(artifacts, "platform", &format!("{error:#}"));
            return Err(error.context("the platform cannot provision Secure Boot and a TPM"));
        }
        let vm = provider.create(spec.clone())?;
        let mut machine = Self {
            provider,
            vm,
            spec,
            root: root.to_path_buf(),
        };
        if let Err(error) = machine.provider.start(&mut machine.vm) {
            unqualified(artifacts, "platform", &format!("{error:#}"));
            return Err(error.context("starting the secure VM"));
        }
        let running = machine.vm.running_mut()?;
        ensure!(
            running.info.secure_boot == secure,
            "the VM's Secure Boot is {}, not {secure}",
            running.info.secure_boot
        );
        wait_firmware_output(running)?;
        Ok(machine)
    }

    fn vm(&mut self) -> Result<&mut Vm> {
        self.vm.running_mut()
    }

    /// Power the guest off, wait for QEMU, and stop (the TPM bundle and the
    /// disk stay).
    fn power_off(&mut self) -> Result<()> {
        let vm = self.vm.running_mut()?;
        let _ = vm.ssh("systemctl poweroff");
        let started = Instant::now();
        while vm.exited()?.is_none() {
            ensure!(
                started.elapsed() < Duration::from_secs(300),
                "the guest did not power off"
            );
            std::thread::sleep(Duration::from_secs(2));
        }
        self.provider.stop(&mut self.vm)
    }

    /// The installed disk started again, without the ISO: with the same TPM
    /// (`fresh_tpm` false) or a new one, on an overlay of the disk the first
    /// run left (so that the first run's own files stay as they are).
    fn boot_disk(&mut self, fresh_tpm: bool, name: &str) -> Result<()> {
        let source = match &self.spec.boot {
            ilex_devkit::core::BootSource::Disk { path, .. } => path.clone(),
            other => bail!("the VM boots from {other:?}"),
        };
        self.spec.iso = None;
        self.spec.additional_disks.clear();
        if fresh_tpm {
            let overlay = self.root.join(format!("{name}.qcow2"));
            let _ = fs::remove_file(&overlay);
            ilex_devkit::cmd::run(
                Command::new("qemu-img")
                    .args(["create", "-q", "-f", "qcow2", "-F", "raw", "-b"])
                    .arg(&source)
                    .arg(&overlay),
            )?;
            self.spec.run = format!("{}-{name}", self.spec.run);
            self.spec.purpose = format!(
                "{}-{name}",
                &self.spec.purpose[..self.spec.purpose.len().min(12)]
            );
            self.spec.tpm = TpmLifecycle::Persistent {
                bundle_id: self.spec.run.clone(),
            };
            self.spec.boot = ilex_devkit::core::BootSource::Disk {
                path: overlay,
                format: ilex_devkit::core::DiskFormat::Qcow2,
                writable: true,
            };
        }
        // No stick, no serials: the disk alone, as a computer has it.
        self.provider = LinuxProvider::new(common::workspace()?)?;
        self.vm = self.provider.create(self.spec.clone())?;
        self.provider.start(&mut self.vm)?;
        Ok(())
    }
}

fn unqualified(artifacts: &Path, stage: &str, error: &str) {
    let _ = fs::write(
        artifacts.join("next-security-result.json"),
        serde_json::to_vec_pretty(&json!({
            "qualification": "unqualified",
            "stage": stage,
            "error": error,
        }))
        .unwrap_or_default(),
    );
}

// --- what runs in the guest -------------------------------------------------------

/// A person at the interface, part one: `Plan` (an encrypted installation, a
/// recovery key, the chip), `SetSecret` twice and `Apply`, then `State` until it
/// ends. Prints the plan's encryption (never a secret) and, last, the state.
const INSTALL_SCRIPT: &str = r#"import base64, fcntl, json, os, subprocess, sys, time
BUS = "org.ilex.Installer1"
disk, admin = sys.argv[1], sys.argv[2]
password, passphrase = base64.b64decode(sys.argv[3]), base64.b64decode(sys.argv[4])

def call(*args, fds=()):
    return subprocess.run(
        ["busctl", "--system", "--json=short", "--timeout=120", "call", BUS, "/org/ilex/Installer1", BUS, *args],
        capture_output=True, text=True, pass_fds=fds)

def document(ran, what):
    if ran.returncode != 0:
        sys.exit(f"{what} failed ({ran.returncode}): {ran.stdout}{ran.stderr}")
    return json.loads(json.loads(ran.stdout)["data"][0])

def secret(plan, kind, value):
    fd = os.memfd_create("secret", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
    os.write(fd, value)
    fcntl.fcntl(fd, 1033, 1 | 2 | 4 | 8)
    os.set_inheritable(fd, True)
    ran = call("SetSecret", "ssh", plan, kind, str(fd), fds=(fd,))
    if ran.returncode != 0:
        sys.exit(f"SetSecret failed: {ran.stdout}{ran.stderr}")

request = json.dumps({"composition": "generic.gnome", "disk": disk, "mode": "whole-disk", "encrypt": True,
                      "tpm": "auto", "recovery_key": True, "host_name": "ilexsec", "administrator": admin})
deadline = time.time() + 120
while True:
    ran = call("Plan", "s", request)
    if ran.returncode == 0 or time.time() > deadline or "Registry" not in ran.stdout + ran.stderr:
        break
    time.sleep(5)
plan = document(ran, "Plan")
print("PLAN " + json.dumps({"id": plan["id"], "encryption": plan["encryption"], "warnings": plan["warnings"],
                            "digest": plan["image"]["digest"]}), flush=True)
secret(plan["id"], "admin-password", password)
secret(plan["id"], "disk-passphrase", passphrase)
document(call("Apply", "ss", plan["id"], disk), "Apply")
print("applied", flush=True)
while True:
    state = document(call("State"), "State")
    if not state["active"] and state["outcome"] is not None:
        break
    time.sleep(3)
print("STATE " + json.dumps(state), flush=True)
sys.exit(0 if state["outcome"]["result"] == "success" else 1)
"#;

/// Part two, a person at the page: wait until the helper says a key waits,
/// take it, print it (to the caller's terminal and nothing else). With `again`
/// the key is asked for a second time after it was taken, which the helper
/// refuses.
const TAKE_SCRIPT: &str = r#"import json, os, sys, time
exec(open("/run/rawbus.py").read())
plan, again = sys.argv[1], sys.argv[2] == "again"
IF = "org.ilex.Installer1"
bus = Bus()
def state():
    return json.loads(bus.call(IF, "/org/ilex/Installer1", IF, "State")["value"])
deadline = time.time() + 600
while True:
    status = state()
    key = status["security"]["recovery_key"]
    if key == "waiting":
        break
    if status["outcome"] is not None or time.time() > deadline:
        sys.exit("no key waits: " + json.dumps({"stage": status["stage"], "security": status["security"]}))
    time.sleep(0.2)
if plan == "-":
    plan = status["applied"]
fd = bus.call(IF, "/org/ilex/Installer1", IF, "TakeRecoveryKey", plan)["value"]
key = os.read(fd, 1024).decode()
os.close(fd)
if again:
    try:
        bus.call(IF, "/org/ilex/Installer1", IF, "TakeRecoveryKey", plan)
        sys.exit("the key was handed over twice")
    except RuntimeError as refused:
        print("AGAIN " + str(refused), flush=True)
print("KEY " + key, flush=True)
"#;

/// Part three: two groups, typed back, as a sealed memfd (`AcknowledgeRecoveryKey`).
const ACK_SCRIPT: &str = r#"import base64, fcntl, os, subprocess, sys
BUS = "org.ilex.Installer1"
plan, text = sys.argv[1], base64.b64decode(sys.argv[2])
fd = os.memfd_create("groups", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
os.write(fd, text)
fcntl.fcntl(fd, 1033, 1 | 2 | 4 | 8)
os.set_inheritable(fd, True)
ran = subprocess.run(["busctl", "--system", "--json=short", "--timeout=120", "call", BUS, "/org/ilex/Installer1", BUS,
                      "AcknowledgeRecoveryKey", "sh", plan, str(fd)], capture_output=True, text=True, pass_fds=(fd,))
print("ACK-OK" if ran.returncode == 0 else "ACK-REFUSED " + (ran.stdout + ran.stderr).replace("\n", " "))
"#;

/// A person's window that took the key before the interface could (the
/// keyboard-install trial stops the interface first): the key of the
/// installation that runs, once, from the helper. It goes nowhere but the
/// caller's memory.
pub(super) fn take_key(vm: &Vm) -> Result<String> {
    vm.ssh_ok(&format!(
        "printf %s {} | base64 -d > /run/rawbus.py; printf %s {} | base64 -d > /run/sec-take.py",
        qemu::base64(RAWBUS.as_bytes()),
        qemu::base64(TAKE_SCRIPT.as_bytes()),
    ))?;
    let taken = vm.ssh_ok("python3 /run/sec-take.py - once");
    let _ = vm.ssh("rm -f /run/rawbus.py /run/sec-take.py");
    let taken = taken.context("taking the recovery key")?;
    let key = taken
        .lines()
        .find_map(|l| l.strip_prefix("KEY "))
        .context("the helper handed over no key")?
        .trim()
        .to_owned();
    ensure!(
        recovery::is_key(&key),
        "the key is not in the form systemd makes"
    );
    Ok(key)
}

fn field<'a>(text: &'a str, key: &str) -> Result<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .with_context(|| format!("no {key}= in:\n{text}"))
}

fn run_guest(vm: &Vm, script: &str) -> Result<String> {
    vm.ssh_ok(script)
}

/// A script of the guest with values in `$PASS` and `$KEY` (base64 on the
/// command line: no quoting depends on their text, and the command line holds
/// neither as such).
fn with_secrets(passphrase: &str, key: &str, body: &str) -> String {
    format!(
        "PASS=$(printf %s {} | base64 -d)\nKEY=$(printf %s {} | base64 -d)\n{body}",
        qemu::base64(passphrase.as_bytes()),
        qemu::base64(key.as_bytes())
    )
}

// --- the trial ----------------------------------------------------------------------

fn scenario(secure: bool) -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let name = if secure { "security" } else { "security-off" };
    let root = ws.state_dir().join("iso-next-fixtures").join(name);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let started = Instant::now();
    fs::write(
        artifacts.join(format!("next-{name}-result.json")),
        serde_json::to_vec_pretty(
            &json!({"qualification": "unqualified", "stage": "run has not completed"}),
        )?,
    )?;
    let mut machine = Machine::boot_iso(&root, secure, &artifacts)?;
    let mut secrets: Vec<String> = vec![US.passphrase.to_owned(), PASSWORD.to_owned()];
    let result = (|| -> Result<()> {
        let key = install(&mut machine, secure, &artifacts, name, &mut secrets)?;
        boots(&mut machine, secure, &artifacts, name, &key)
    })();
    // Whatever was saved here holds no secret: the passphrase, the password
    // and the key (when there is one) are looked for in every file.
    let leak = scan_host(&artifacts, name, &secrets);
    let report = json!({
        "qualification": if result.is_ok() && leak.is_ok() { "passed" } else { "failed" },
        "secure_boot": secure,
        "elapsed_seconds": started.elapsed().as_secs(),
        "error": result.as_ref().err().map(|e| format!("{e:#}")),
        "secret_scan": leak.as_ref().err().map(|e| format!("{e:#}")).unwrap_or_else(|| "clean".into()),
    });
    fs::write(
        artifacts.join(format!("next-{name}-result.json")),
        serde_json::to_vec_pretty(&report)?,
    )?;
    result?;
    leak
}

/// The installation: Plan, the key, its confirmation, the end; then what the
/// helper and its logs hold. Returns the recovery key.
fn install(
    machine: &mut Machine,
    secure: bool,
    artifacts: &Path,
    name: &str,
    secrets: &mut Vec<String>,
) -> Result<String> {
    let started = Instant::now();
    let vm = machine.vm()?;
    vm.wait_ssh(Duration::from_secs(600))
        .context("the live system did not come up")?;
    // The computer, as `ilex-enroll` sees it: eligible with Secure Boot on and
    // a TPM whose PCR 7 is what its event log replays to; not with it off.
    let probe: Value = serde_json::from_str(&run_guest(vm, "ilex-enroll probe --json")?)
        .context("ilex-enroll's probe")?;
    fs::write(
        artifacts.join(format!("next-{name}-probe.json")),
        serde_json::to_vec_pretty(&probe)?,
    )?;
    if secure {
        ensure!(
            probe["verdict"] == "eligible"
                && probe["secure_boot"] == true
                && probe["setup_mode"] == false
                && probe["tpm2"] == true
                && probe["pcr7"] == probe["replay"]["sha256"],
            "the secure VM is not an eligible computer: {probe}"
        );
    } else {
        ensure!(
            probe["verdict"] == "unavailable" && probe["reason"] == "secure-boot-off",
            "the insecure VM: {probe}"
        );
    }
    // The disk the helper names by its serial number.
    let disk = "virtio-ilex-disk-0";
    vm.ssh_ok(&format!(
        "rm -f /run/sec-install.log; printf %s {} | base64 -d > /run/sec-install.py; printf %s {} | base64 -d > /run/rawbus.py; \
         printf %s {} | base64 -d > /run/sec-take.py; printf %s {} | base64 -d > /run/sec-ack.py",
        qemu::base64(INSTALL_SCRIPT.as_bytes()),
        qemu::base64(RAWBUS.as_bytes()),
        qemu::base64(TAKE_SCRIPT.as_bytes()),
        qemu::base64(ACK_SCRIPT.as_bytes()),
    ))?;
    vm.ssh_ok(&format!(
        "nohup sh -c 'python3 /run/sec-install.py {} {} {} {} > /run/sec-install.log 2>&1; echo EXIT=$? >> /run/sec-install.log' > /dev/null 2>&1 &",
        crate::installer::apply::shell(disk),
        crate::installer::apply::shell(ADMIN),
        qemu::base64(PASSWORD.as_bytes()),
        qemu::base64(US.passphrase.as_bytes()),
    ))?;
    vm.wait_for(
        "the plan",
        "grep -q '^PLAN ' /run/sec-install.log || grep -q '^EXIT=' /run/sec-install.log",
        Duration::from_secs(300),
    )?;
    let log = vm.ssh_ok("cat /run/sec-install.log")?;
    let plan_line = log
        .lines()
        .find_map(|l| l.strip_prefix("PLAN "))
        .with_context(|| format!("no plan: {log}"))?;
    let plan: Value = serde_json::from_str(plan_line)?;
    let plan_id = plan["id"].as_str().context("the plan's id")?.to_owned();
    let enrolls = plan["encryption"]["tpm"]["will_enroll"] == true;
    ensure!(
        plan["encryption"]["enabled"] == true
            && plan["encryption"]["recovery_key"] == true
            && enrolls == secure
            && plan["encryption"]["tpm"]["reason"]
                == if secure {
                    "pcr7-sha256"
                } else {
                    "secure-boot-off"
                },
        "the plan's encryption: {plan}"
    );
    let warnings = plan["warnings"].to_string();
    ensure!(
        secure == !warnings.contains("security chip"),
        "the plan's warnings about the chip: {warnings}"
    );
    println!("iso::next {name}: plan {plan_id}: {}", plan["encryption"]);

    // The key: taken once (a second request is refused)...
    let taken = vm.ssh_ok(&format!("python3 /run/sec-take.py {plan_id} again"))?;
    ensure!(
        taken.contains("AGAIN org.ilex.Installer1.Error.Refused")
            && taken.contains("recovery-key-taken"),
        "the key was not refused the second time: {}",
        taken.replace(
            taken.lines().find(|l| l.starts_with("KEY ")).unwrap_or("-"),
            "KEY <withheld>"
        )
    );
    let key = taken
        .lines()
        .find_map(|l| l.strip_prefix("KEY "))
        .context("the helper handed over no key")?
        .trim()
        .to_owned();
    ensure!(
        recovery::is_key(&key),
        "the key is not in the form systemd makes"
    );
    secrets.push(key.clone());
    // ...and `State` says so, without the key.
    let state = busctl_state(vm)?;
    ensure!(
        state["security"]["recovery_key"] == "shown",
        "the state after the key was taken: {}",
        state["security"]
    );
    // Two groups that are wrong (right shape, wrong letters) are refused; the
    // helper keeps waiting.
    let asked = recovery::asked(&plan_id);
    let groups = recovery::groups(&key);
    let answer = |a: [String; 2]| -> String {
        recovery::answer_text(&[(asked[0], a[0].clone()), (asked[1], a[1].clone())])
    };
    let reversed = |n: usize| groups[n - 1].chars().rev().collect::<String>();
    let ack = |text: &str| -> Result<String> {
        vm.ssh_ok(&format!(
            "python3 /run/sec-ack.py {plan_id} {}",
            qemu::base64(text.as_bytes())
        ))
    };
    let refused = ack(&answer([reversed(asked[0]), reversed(asked[1])]))?;
    ensure!(
        refused.contains("ACK-REFUSED") && refused.contains("recovery-key-mismatch"),
        "wrong groups were not refused: {refused}"
    );
    let state = busctl_state(vm)?;
    ensure!(
        state["security"]["recovery_key"] == "shown" && state["active"] == true,
        "the state after a refusal: {}",
        state["security"]
    );
    let accepted = ack(&answer([
        groups[asked[0] - 1].to_uppercase(),
        groups[asked[1] - 1].to_owned(),
    ]))?;
    ensure!(
        accepted.contains("ACK-OK"),
        "the right groups were not accepted: {accepted}"
    );

    // The end.
    let waited = vm.wait_for(
        "the installation to end",
        "grep -q '^EXIT=' /run/sec-install.log",
        INSTALL_TIMEOUT,
    );
    let log = vm.ssh_ok("cat /run/sec-install.log").unwrap_or_default();
    fs::write(artifacts.join(format!("next-{name}-install.txt")), &log)?;
    waited?;
    let state: Value = serde_json::from_str(
        log.lines()
            .find_map(|l| l.strip_prefix("STATE "))
            .with_context(|| format!("no final state: {log}"))?,
    )?;
    ensure!(
        state["outcome"]["result"] == "success" && log.contains("EXIT=0"),
        "the installation did not succeed: {}",
        state["outcome"]
    );
    let tpm = &state["security"]["tpm"];
    ensure!(
        state["security"]["recovery_key"] == "acknowledged"
            && tpm["status"]
                == if secure {
                    "enrolled-unverified"
                } else {
                    "passphrase-only"
                }
            && tpm["reason"]
                == if secure {
                    "pcr7-sha256"
                } else {
                    "secure-boot-off"
                },
        "the helper's record of the chip: {}",
        state["security"]
    );
    let stages: Vec<&str> = state["stages"]
        .as_array()
        .context("stages")?
        .iter()
        .filter_map(|s| s["stage"].as_str())
        .collect();
    let expected: &[&str] = if secure {
        &[
            "partition",
            "encrypt",
            "mount",
            "image",
            "configure",
            "account",
            "recovery",
            "tpm",
            "finalise",
        ]
    } else {
        &[
            "partition",
            "encrypt",
            "mount",
            "image",
            "configure",
            "account",
            "recovery",
            "finalise",
        ]
    };
    ensure!(stages == expected, "the stages: {stages:?}");
    println!(
        "iso::next {name}: installed in {:?}; chip {}",
        started.elapsed(),
        tpm
    );

    // Enforcing, and not one denial, through the whole installation.
    selinux::check(vm, artifacts, &format!("next-{name}"))?;

    // The helper and its logs hold neither secret.
    let scan = vm.ssh_ok(&with_secrets(
        US.passphrase,
        &key,
        r#"set -u
count() { grep -rlaF -e "$PASS" -e "$KEY" "$@" 2>/dev/null | wc -l; }
echo "run_dir=$(count /run/ilex-installer)"
echo "state_json=$(grep -cF -e "$PASS" -e "$KEY" /run/ilex-installer/state.json || true)"
echo "journal=$(journalctl --no-pager 2>/dev/null | grep -cF -e "$PASS" -e "$KEY" || true)"
echo "dmesg=$(dmesg 2>/dev/null | grep -cF -e "$PASS" -e "$KEY" || true)"
echo "processes=$(ps -eo args | grep -v grep | grep -cF -e "$PASS" -e "$KEY" || true)"
echo "install_log=$(grep -cF -e "$PASS" -e "$KEY" /run/sec-install.log || true)"
echo "key_files=$(ls /run/ilex-enroll 2>/dev/null | tr '\n' ' ')"
"#,
    ))?;
    fs::write(
        artifacts.join(format!("next-{name}-secret-scan.txt")),
        &scan,
    )?;
    for entry in [
        "run_dir",
        "state_json",
        "journal",
        "dmesg",
        "processes",
        "install_log",
    ] {
        ensure!(
            field(&scan, entry)? == "0",
            "a secret is in {entry}: {scan}"
        );
    }
    // The helper's own scratch holds no receipt of the key: the sealed receipt
    // of the library goes with the acknowledgement.
    ensure!(
        !field(&scan, "key_files")?.contains("pending"),
        "a receipt is left: {scan}"
    );
    // The log saved to a stick: a tarball, scanned as it is and extracted.
    save_log_and_scan(vm, &key)?;
    machine.power_off().context("powering the installer off")?;
    Ok(key)
}

fn busctl_state(vm: &Vm) -> Result<Value> {
    document(&ssh(vm, &busctl("root", "State", &[]))?, "State")
}

/// `Finish(save-log:…)` onto the stick, and the tarball looked at.
fn save_log_and_scan(vm: &Vm, key: &str) -> Result<()> {
    let probe = document(&ssh(vm, &busctl("root", "Probe", &[]))?, "Probe")?;
    let stick = probe["disks"]
        .as_array()
        .context("disks")?
        .iter()
        .find(|d| {
            d["removable"] == true && d["id"].as_str().is_some_and(|i| i.contains("ILEXSTICK0"))
        })
        .and_then(|d| d["id"].as_str())
        .context("no stick in the probe")?
        .to_owned();
    let saved = document(
        &ssh(
            vm,
            &busctl("root", "Finish", &[&format!("save-log:{stick}")]),
        )?,
        "Finish(save-log)",
    )?;
    let file = saved["file"].as_str().context("the saved file")?.to_owned();
    vm.ssh_ok("sync")?;
    let scan = vm.ssh_ok(&with_secrets(
        US.passphrase,
        key,
        &format!(
            r#"set -u
mkdir -p /run/stickscan /run/stick
for p in /dev/disk/by-id/*ILEXSTICK0*-part1; do mount -o ro "$p" /run/stick; done
tar -C /run/stickscan -xf /run/stick/{file}
echo "files=$(find /run/stickscan -type f | wc -l)"
echo "extracted=$(grep -rlaF -e "$PASS" -e "$KEY" /run/stickscan | wc -l)"
echo "raw=$(grep -caF -e "$PASS" -e "$KEY" /run/stick/{file} || true)"
echo "names=$(cd /run/stickscan && find . -type f | sed 's|^\./||' | tr '\n' ' ')"
umount /run/stick; rm -rf /run/stickscan
"#
        ),
    ))?;
    ensure!(
        field(&scan, "files")? != "0"
            && field(&scan, "extracted")? == "0"
            && field(&scan, "raw")? == "0",
        "the saved log: {scan}"
    );
    Ok(())
}

/// The installed disk, started cold; then on a new TPM.
fn boots(
    machine: &mut Machine,
    secure: bool,
    artifacts: &Path,
    name: &str,
    key: &str,
) -> Result<()> {
    let save = |file: &str, text: &str| {
        let _ = qemu::write_artifact(artifacts, &format!("next-{name}-{file}"), text);
    };
    if secure {
        // The first cold boot: nobody types. It reaches SSH on its own.
        machine.boot_disk(false, "cold")?;
        let vm = machine.vm()?;
        let started = Instant::now();
        let reached = vm.wait_ssh(BOOT_TIMEOUT);
        let ctl = Vmctl::from_info(vm.info.clone());
        let _ = ctl.save_screenshot(&artifacts.join(format!("next-{name}-cold-boot.png")));
        reached.context("the cold boot did not reach SSH with nobody at the keyboard")?;
        println!(
            "iso::next {name}: the cold boot reached SSH after {:?}, no input",
            started.elapsed()
        );
        check_enrolled(vm, true, key, &save, name)?;
        check_installed(
            &save,
            vm,
            &Expected {
                digest: &installed_digest(artifacts, name)?,
                target: TARGET_REF,
                host: HOST,
                zone: "Etc/UTC",
                locale: "en_US.UTF-8",
                keymap: "us",
                layout: "us",
                admin: ADMIN,
                password: PASSWORD,
                stamp: &[],
            },
        )?;
        machine.power_off()?;
        // Another TPM: PCR 7 is what it was, what the chip sealed is not
        // there. The prompt appears, and the recovery key opens the disk.
        machine.boot_disk(true, "swap")?;
        let vm = machine.vm()?;
        encrypted::unlock(vm, key, artifacts, &format!("next-{name}-swap"))
            .context("unlocking with the recovery key at the prompt, on another TPM")?;
        vm.wait_ssh(BOOT_TIMEOUT)
            .context("the disk did not come up after the recovery key")?;
        check_enrolled(vm, false, key, &save, name)?;
    } else {
        // Secure Boot off: no chip; the recovery key opens the disk.
        machine.boot_disk(false, "cold")?;
        let vm = machine.vm()?;
        encrypted::unlock(vm, key, artifacts, &format!("next-{name}-cold"))
            .context("unlocking with the recovery key at the prompt")?;
        vm.wait_ssh(BOOT_TIMEOUT)
            .context("the disk did not come up after the recovery key")?;
        check_enrolled(vm, false, key, &save, name)?;
    }
    Ok(())
}

fn installed_digest(artifacts: &Path, name: &str) -> Result<String> {
    let log = fs::read_to_string(artifacts.join(format!("next-{name}-install.txt")))?;
    let plan = log
        .lines()
        .find_map(|l| l.strip_prefix("PLAN "))
        .context("no plan in the install log")?;
    let plan: Value = serde_json::from_str(plan)?;
    Ok(plan["digest"].as_str().context("digest")?.to_owned())
}

/// What the installed, unlocked system says of its security chip and its
/// recovery key. `cold`: this boot was the first, with the chip (so the chip
/// opened the volume and `verify` says verified); otherwise the volume was
/// opened with the recovery key at the prompt.
fn check_enrolled(
    vm: &Vm,
    cold: bool,
    key: &str,
    save: &dyn Fn(&str, &str),
    name: &str,
) -> Result<()> {
    let secure = name == "security";
    let out = vm.ssh_ok(&with_secrets(
        US.passphrase,
        key,
        r#"set -u
uuid=$(cryptsetup luksUUID /dev/disk/by-partlabel/ilex-root)
echo "uuid=$uuid"
cryptsetup luksDump --dump-json-metadata /dev/disk/by-uuid/$uuid | python3 -c '
import json, sys
h = json.load(sys.stdin)
tokens = h["tokens"].values()
tpm = [t for t in tokens if t["type"] == "systemd-tpm2"]
print("slots=%d" % len(h["keyslots"]))
print("token_types=%s" % ",".join(sorted(t["type"] for t in tokens)))
print("tpm_tokens=%d" % len(tpm))
if tpm:
    t = tpm[0]
    print("tpm_pcrs=%s" % ",".join(str(p) for p in t.get("tpm2-pcrs", [])))
    print("tpm_bank=%s" % t.get("tpm2-pcr-bank"))
    print("tpm_pin=%s" % t.get("tpm2-pin"))
    print("tpm_extra=%s" % ",".join(sorted(k for k in t if "pubkey" in k or "pcrlock" in k or "signature" in k)))
'
echo "crypttab=$(tr '\t' ' ' < /etc/crypttab)"
echo "setup=$(python3 -c 'import json; print(json.dumps(json.load(open("/etc/ilex/setup.json"))["security"]["tpm"], sort_keys=True, separators=(",", ":")))')"
echo "record=$(python3 -c '
import json
r = json.load(open("/var/lib/ilex/security/enrollment.json"))
print(json.dumps({"state": r.get("state"), "uuid": r.get("uuid"), "recovery": r.get("recovery"), "tpm": r.get("tpm"), "reason": r.get("reason")}, sort_keys=True, separators=(",", ":")))')"
echo "record_mode=$(stat -c %a:%U /var/lib/ilex/security/enrollment.json)"
echo "security_dir=$(ls -A /var/lib/ilex/security | tr '\n' ' ')"
echo "unlock_record=$(tr -d '\n ' < /run/ilex/root-unlock.json)"
echo "verify=$(ilexctl security verify 2>&1 | tr -d '\n ')"
# The recovery key opens its slot (the passphrase's is not asked for).
printf %s "$KEY" | cryptsetup open --test-passphrase --key-file=- /dev/disk/by-uuid/$uuid; echo "key_rc=$?"
printf %s "$PASS" | cryptsetup open --test-passphrase --key-file=- /dev/disk/by-uuid/$uuid; echo "pass_rc=$?"
echo "leak_files=$(grep -rlaF -e "$PASS" -e "$KEY" /var/log /etc /var/lib/ilex /var/lib/systemd 2>/dev/null | wc -l)"
echo "leak_journal=$(journalctl -b --no-pager | grep -cF -e "$PASS" -e "$KEY" || true)"
echo "failed=$(systemctl --failed --no-legend --plain | wc -l)"
"#,
    ))?;
    let stage = if cold {
        "cold"
    } else if secure {
        "swap"
    } else {
        "off"
    };
    save(&format!("installed-{stage}.txt"), &out);
    println!(
        "{}",
        out.lines()
            .map(|l| format!("     | {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let uuid = field(&out, "uuid")?;
    let want = |k: &str, v: &str| -> Result<()> {
        ensure!(
            field(&out, k)? == v,
            "{k}: {:?}, wanted {v:?}\n{out}",
            field(&out, k)?
        );
        Ok(())
    };
    // The volume: the passphrase's slot, the recovery key's, and the chip's
    // when it was enrolled; nothing the installer did not make.
    if secure {
        want("slots", "3")?;
        want("token_types", "systemd-recovery,systemd-tpm2")?;
        want("tpm_tokens", "1")?;
        want("tpm_pcrs", "7")?;
        want("tpm_bank", "sha256")?;
        want("tpm_pin", "False")?;
        want("tpm_extra", "")?;
    } else {
        want("slots", "2")?;
        want("token_types", "systemd-recovery")?;
        want("tpm_tokens", "0")?;
    }
    // The line `ilex-enroll`'s completion writes (its columns are tabs).
    want(
        "crypttab",
        &format!("luks-{uuid} UUID={uuid} none discard,tpm2-device=auto"),
    )?;
    let setup: Value = serde_json::from_str(field(&out, "setup")?)?;
    ensure!(
        setup["requested"] == "auto"
            && setup["status"]
                == if secure {
                    "enrolled-unverified"
                } else {
                    "passphrase-only"
                }
            && setup["reason"]
                == if secure {
                    "pcr7-sha256"
                } else {
                    "secure-boot-off"
                }
            && setup["capability"] == if secure { "eligible" } else { "unavailable" },
        "setup.json's security.tpm: {setup}"
    );
    let record: Value = serde_json::from_str(field(&out, "record")?)?;
    ensure!(
        record["uuid"] == uuid
            && record["recovery"]["acknowledged"] == true
            && record["recovery"]["displayed_and_acknowledged"] == true
            && if secure {
                record["tpm"].is_object() && record["state"] != "unavailable"
            } else {
                record["tpm"].is_null() && record["state"] == "unavailable"
            },
        "the enrolment record: {record}"
    );
    want("record_mode", "600:root")?;
    // Only the record: no receipt of the key or of the chip is left.
    want("security_dir", "enrollment.json ")?;
    let unlock: Value = serde_json::from_str(field(&out, "unlock_record")?)?;
    ensure!(
        unlock["uuid"] == uuid && unlock["method"] == if cold { "tpm2" } else { "fallback" },
        "how this boot opened the volume: {unlock}"
    );
    if cold {
        ensure!(
            unlock["token"].is_u64() && unlock["slot"].is_u64(),
            "the chip's token and slot: {unlock}"
        );
        // The first boot that the chip unlocked: `verify` says verified.
        ensure!(
            field(&out, "verify")?.contains("\"state\":\"verified\""),
            "ilexctl security verify: {}",
            field(&out, "verify")?
        );
    }
    want("key_rc", "0")?;
    want("pass_rc", "0")?;
    want("leak_files", "0")?;
    want("leak_journal", "0")?;
    want("failed", "0")?;
    Ok(())
}

/// Nothing this trial saved holds a secret: every file under the artifacts
/// that belongs to this trial is read as bytes.
fn scan_host(artifacts: &Path, name: &str, secrets: &[String]) -> Result<()> {
    let prefix = format!("next-{name}");
    let mut found = Vec::new();
    for entry in fs::read_dir(artifacts)? {
        let entry = entry?;
        let file = entry.file_name().to_string_lossy().into_owned();
        if !file.starts_with(&prefix) || !entry.file_type()?.is_file() {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        for (index, secret) in secrets.iter().enumerate() {
            if bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
            {
                // Names, never the value.
                found.push(format!("{file} holds secret #{index}"));
            }
        }
    }
    ensure!(
        found.is_empty(),
        "secrets in saved files: {}",
        found.join("; ")
    );
    Ok(())
}
