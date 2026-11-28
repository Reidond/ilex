//! The encrypted release gates, `security::encrypted_install` and
//! `security::upgrade_rollback`: an encrypted installation by the live installer
//! of the GNOME edition (the real root helper, a recovery key and the security
//! chip, on a Secure Boot VM with a persistent TPM), then the installed system's
//! boots: automatic TPM unlock, PCR mismatch, recovery key, no TPM, and for the
//! lifecycle an upgrade and a rollback. A disposable signed local registry
//! delivers only the lifecycle's update; nothing publishes to ghcr or relaxes
//! the shipped reject policy.
use crate::common;
use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::{
    Workspace, cmd,
    core::{Requirement, TpmLifecycle, VmProvider, VmSpec},
    provider::{LinuxProvider, LinuxVm},
    qemu, security,
};
use ilex_enroll::Secret;
use ilex_vmctl::Vmctl;
use libtest_mimic::Trial;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fmt,
    fs::{self, File},
    io::{Read, Write},
    net::{Shutdown, TcpListener},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::UnixStream,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const GUEST: &str = include_str!("encrypted/guest.py");
const PRIVACY: &str = include_str!("encrypted/privacy.py");
const DIAGNOSTICS: &str = include_str!("encrypted/diagnostics.py");
const LOG_STREAM: &str = include_str!("encrypted/log_stream.py");
#[path = "encrypted/lifecycle.rs"]
mod lifecycle;

#[derive(Clone, Debug, Serialize)]
struct GuestFailure {
    check: &'static str,
    observed_unix_seconds: u64,
    unlock_method: &'static str,
    unit_states: BTreeMap<&'static str, &'static str>,
}

impl fmt::Display for GuestFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.check)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl GuestFailure {
    fn capture(fixture: Option<&mut Fixture>, check: &'static str) -> Self {
        let vm = fixture.and_then(|fixture| fixture.vm.running_mut().ok());
        Self::capture_vm(vm.as_deref(), check)
    }

    fn capture_vm(vm: Option<&qemu::Vm>, check: &'static str) -> Self {
        const UNITS: &[&str] = &[
            "multi-user.target",
            "ilex-security-verify.service",
            "ilex-fixture-wrong-repair.service",
            "ilex-fixture-full-repair.service",
            "ilex-fixture-repair.service",
            "ilex-encrypted-vt-input.service",
            "ilex-enroll-no-tpm-completion.service",
        ];
        const STATES: &[&str] = &[
            "active",
            "reloading",
            "inactive",
            "failed",
            "activating",
            "deactivating",
            "maintenance",
            "refreshing",
        ];
        let mut snapshot = Self {
            check,
            observed_unix_seconds: unix_seconds(),
            unlock_method: "unavailable",
            unit_states: UNITS.iter().map(|&unit| (unit, "unavailable")).collect(),
        };
        let Some(vm) = vm else {
            return snapshot;
        };
        let query = format!(
            r#"timeout 10s sh -s -- {} <<'ILEX_STATE'
for unit do
    state=$(systemctl show -p ActiveState --value "$unit" 2>/dev/null) || state=
    printf '%s\n' "$state"
done
python3 -I -S -c 'import json; method = json.loads(open("/run/ilex/root-unlock.json", "rb").read(4096))["method"]; print(method if method in ("fallback", "tpm2") else "unavailable")' 2>/dev/null || printf '\n'
ILEX_STATE"#,
            UNITS.join(" "),
        );
        let Ok(output) = vm.ssh(&query) else {
            return snapshot;
        };
        if output.status.success()
            && output.stdout.len() <= 1024
            && let Ok(output) = std::str::from_utf8(&output.stdout)
            && output.lines().count() == UNITS.len() + 1
        {
            // Emit only fixed unit names and allowlisted values. Failure
            // paths can report this even when the artifact scan cannot run.
            let mut values = output.lines();
            for (&unit, value) in UNITS.iter().zip(&mut values) {
                if let Some(&state) = STATES.iter().find(|&&state| state == value) {
                    snapshot.unit_states.insert(unit, state);
                }
            }
            snapshot.unlock_method = match values.next() {
                Some("tpm2") => "tpm2",
                Some("fallback") => "fallback",
                _ => "unavailable",
            };
        }
        snapshot
    }
}

pub fn trials() -> Vec<Trial> {
    vec![
        common::trial("security::encrypted_install", || run_fixture(false)),
        common::trial("security::upgrade_rollback", lifecycle::run),
    ]
}

pub(crate) struct Registry {
    id: String,
    owner: String,
    port: u16,
    work: PathBuf,
}
impl Drop for Registry {
    fn drop(&mut self) {
        let owner = cmd::probe(Command::new("podman").args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"ilex.checkout-owner\"}}",
            &self.id,
        ]));
        if owner.as_deref().map(str::trim) == Some(self.owner.as_str()) {
            let _ = Command::new("podman")
                .args(["rm", "--force", &self.id])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }
}
/// A disposable signed registry the guest reaches at 10.0.2.2: the update of
/// the lifecycle is pushed to it (the installer itself installs from ghcr).
pub(crate) fn registry(ws: &Workspace, work: &Path) -> Result<Registry> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let owner = ws.identity()?.owner;
    let run = work
        .file_name()
        .and_then(|s| s.to_str())
        .context("fixture run missing")?;
    let name = ws.vm_name(&format!("enroll-registry-{run}"));
    let id = cmd::output(Command::new("podman").args([
        "run",
        "--rm",
        "--detach",
        "--name",
        &name,
        "--label",
        &format!("ilex.checkout-owner={owner}"),
        "--publish",
        &format!("127.0.0.1:{port}:5000"),
        "docker.io/library/registry:2",
    ]))?;
    let registry = Registry {
        id: id.trim().into(),
        owner,
        port,
        work: work.to_owned(),
    };
    cmd::run(
        Command::new("cosign")
            .args(["generate-key-pair", "--output-key-prefix"])
            .arg(work.join("cosign"))
            .env("COSIGN_PASSWORD", ""),
    )?;
    Ok(registry)
}

impl Registry {
    fn push(&self, image: &str, tag: &str) -> Result<Value> {
        let port = self.port;
        let local = format!("localhost:{port}/ilex:{tag}");
        let repository = format!("10.0.2.2:{port}/ilex");
        let digestfile = self.work.join("digest");
        cmd::run(
            Command::new("podman")
                .args(["push", "--tls-verify=false", "--digestfile"])
                .arg(&digestfile)
                .arg(image)
                .arg(&local),
        )?;
        let digest = fs::read_to_string(digestfile)?.trim().to_string();
        ensure!(lifecycle::valid_digest(&digest), "invalid fixture digest");
        Ok(
            json!({"repository": repository, "source":format!("{repository}@{digest}"), "digest":digest, "fixture":"candidate-signed-local-registry"}),
        )
    }

    fn sign(&self, digest: &str) -> Result<()> {
        let port = self.port;
        cmd::run(
            Command::new("cosign")
                .args([
                    "sign",
                    "--yes",
                    "--tlog-upload=false",
                    "--allow-insecure-registry",
                    "--key",
                ])
                .arg(self.work.join("cosign.key"))
                .args([
                    "--sign-container-identity",
                    &format!("10.0.2.2:{port}/ilex"),
                ])
                .arg(format!("localhost:{port}/ilex@{digest}"))
                .env("COSIGN_PASSWORD", ""),
        )
    }
}

struct PrivateConsole {
    input: UnixStream,
    output: Option<thread::JoinHandle<std::io::Result<()>>>,
    path: PathBuf,
}
impl PrivateConsole {
    fn attach(info: &qemu::VmInfo) -> Result<Self> {
        let path = info.dir.join("input.sock");
        ensure!(!path.exists(), "private console endpoint already exists");
        let mut qmp = ilex_devkit::qmp::Qmp::connect(&info.qmp_sock)?;
        let devices = qmp.execute("query-chardev", json!({}))?;
        ensure!(
            devices
                .as_array()
                .context("invalid console inventory")?
                .iter()
                .any(|d| d["label"] == "serial0"
                    && d["filename"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("file"))),
            "expected the fixture's file-backed serial0"
        );
        // Keep the guest UART unchanged. This test-only backend has no QEMU
        // logfile: a separate reader records outgoing guest bytes exclusively.
        // Credential input never enters QMP, argv or that output writer.
        qmp.execute("chardev-change", json!({"id":"serial0", "backend":{
            "type":"socket", "data":{"addr":{"type":"unix","data":{"path":path}},"server":true,"wait":false}
        }}))?;
        let input = UnixStream::connect(&path)?;
        input.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = input.try_clone()?;
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(info.dir.join("serial.log"))?;
        let output = thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            let result = (|| {
                loop {
                    let length = match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(length) => length,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    };
                    log.write_all(&buffer[..length])?;
                }
                log.sync_all()
            })();
            buffer.fill(0);
            result
        });
        Ok(Self {
            input,
            output: Some(output),
            path,
        })
    }
    fn credential(&mut self, secret: &Secret) -> Result<()> {
        self.input.write_all(secret.bytes())?;
        self.input.write_all(b"\n")?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        // Called after QEMU exits: drain through EOF before copying logs.
        if let Some(output) = self.output.take() {
            output
                .join()
                .map_err(|_| anyhow::anyhow!("private console reader failed"))??;
        }
        let _ = fs::remove_file(&self.path);
        Ok(())
    }
}
impl Drop for PrivateConsole {
    fn drop(&mut self) {
        // Cancellation must also wake a reader when a failed VM is still up.
        let _ = self.input.shutdown(Shutdown::Both);
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
        let _ = fs::remove_file(&self.path);
    }
}

struct Fixture {
    provider: LinuxProvider,
    vm: LinuxVm,
    spec: VmSpec,
    console: Option<PrivateConsole>,
}
impl Fixture {
    fn private_console(&mut self) -> Result<()> {
        self.console = Some(PrivateConsole::attach(&self.vm.running_mut()?.info)?);
        Ok(())
    }
    fn logs(&mut self, artifacts: &Path, label: &str) -> Result<()> {
        if self.vm.running_mut()?.exited()?.is_some()
            && let Some(mut console) = self.console.take()
        {
            console.finish()?;
        }
        let info = &self.vm.running_mut()?.info;
        let private = raw_logs(artifacts)?;
        fs::create_dir_all(&private)?;
        for file in ["serial.log", "qemu.log", "swtpm.log"] {
            if info.dir.join(file).is_file() {
                fs::copy(info.dir.join(file), private.join(format!("{label}-{file}")))?;
            }
        }
        Ok(())
    }
    fn disk_boot(&mut self) -> Result<()> {
        self.provider.stop(&mut self.vm)?;
        self.console.take();
        self.spec.iso = None;
        self.spec.additional_disks.clear();
        self.vm = self.provider.create(self.spec.clone())?;
        self.provider.start(&mut self.vm)?;
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(e) = self.provider.destroy_owned(&mut self.vm) {
            eprintln!("encrypted fixture cleanup failed: {e:#}");
        }
    }
}

fn fingerprints(secrets: &[&Secret]) -> Value {
    json!(
        secrets
            .iter()
            .map(|s| json!({"length":s.bytes().len(), "sha256":security::hash(s.bytes())}))
            .collect::<Vec<_>>()
    )
}

fn private_fingerprint(secret: &Secret) -> Value {
    let bytes = secret.bytes();
    json!({
        "length":bytes.len(), "sha256":security::hash(bytes),
        "edges":[security::hash(&bytes[..8]), security::hash(&bytes[bytes.len() - 8..])]
    })
}

fn save_private_fingerprints(tier: &str, fingerprints: &[Value]) -> Result<()> {
    if let Some(directory) = std::env::var_os("ILEX_PRIVATE_FINGERPRINTS") {
        let path = PathBuf::from(directory).join(format!("{tier}.json"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        serde_json::to_writer(&mut file, fingerprints)?;
        file.sync_all()?;
    }
    Ok(())
}

fn failure_code(error: &anyhow::Error) -> Value {
    // Error chains can contain guest output. Export only fixed classifications
    // and an OS errno; never the original text, matching path or line.
    let mut detail = format!("{error:#}").into_bytes();
    let patterns: &[(&[u8], &str)] = &[
        (b"QMP chardev-change failed", "console-backend-change"),
        (b"QMP query-chardev failed", "console-inventory"),
        (
            b"expected the fixture's file-backed serial0",
            "console-backend-type",
        ),
        (
            b"private console endpoint already exists",
            "console-endpoint-exists",
        ),
        (
            b"did not request fallback authentication",
            "fallback-prompt",
        ),
        (
            b"encrypted ISO installation did not finish",
            "installer-exit",
        ),
        (
            b"private encrypted fixture failed",
            "installer-guest-failure",
        ),
        (
            b"encrypted installer deadline exceeded",
            "installer-timeout",
        ),
        (b"read installer recovery envelope", "recovery-envelope"),
        (b"finaliser-leak-scan.json", "installer-finaliser-evidence"),
        (
            b"private guest secret scan failed",
            "installer-privacy-scan",
        ),
        (b"no-TPM mode was not preserved", "no-tpm-probe"),
        (b"start no-TPM completion", "no-tpm-unit-start"),
        (
            b"wait for no-TPM passphrase prompt",
            "no-tpm-completion-prompt",
        ),
        (
            b"wait for no-TPM completion confirmation",
            "no-tpm-completion-confirmation",
        ),
        (b"verify no-TPM completion exit", "no-tpm-completion-exit"),
        (
            b"no-TPM completion did not preserve",
            "no-tpm-completion-state",
        ),
        (
            b"no-TPM completion changed LUKS",
            "no-tpm-completion-tokens",
        ),
        (b"did not accept SSH", "ssh-deadline"),
        (b"qemu exited", "guest-exited"),
        (b"QMP command", "qmp-deadline"),
        (b"QMP response ID", "qmp-response-id"),
        (b"installed system health", "installed-system-health"),
        (b"installed enrollment self-check", "enrollment-self-check"),
        (
            b"repair did not refuse for lack of a free LUKS2 slot",
            "full-header-refusal",
        ),
        (
            b"full-header repair changed credential data or enrollment state",
            "full-header-changed",
        ),
        (b"PCR7 lacks meaningful", "positive-authority-log"),
        (
            b"present TPM / changed Secure Boot PCR",
            "mismatch-measurement",
        ),
        (
            b"fallback boot changed slots/tokens",
            "fallback-metadata-changed",
        ),
        (b"post-finalisation private scan", "target-privacy-scan"),
        (b"credential found or unreadable", "artifact-privacy-scan"),
    ];
    let codes: Vec<&str> = patterns
        .iter()
        .filter_map(|(pattern, code)| {
            detail
                .windows(pattern.len())
                .any(|w| w == *pattern)
                .then_some(*code)
        })
        .collect();
    detail.fill(0);
    let errno = error
        .chain()
        .find_map(|e| e.downcast_ref::<std::io::Error>()?.raw_os_error());
    json!({"codes":codes, "errno":errno})
}

fn private_scan(script: &str) -> Result<Value> {
    let script = format!("__name__ = 'ilex_private_matcher'\n{script}");
    let output = Command::new("python3")
        .args(["-I", "-S", "-c", &script])
        .output()?;
    ensure!(
        output.status.success(),
        "private artifact scan failed; diagnostics suppressed"
    );
    let result: Value =
        serde_json::from_slice(&output.stdout).context("invalid private scan result")?;
    ensure!(
        result["secret_found"] == false && result["read_errors"] == 0,
        "credential found or unreadable evidence; details suppressed"
    );
    Ok(result)
}

fn artifact_privacy(roots: &[&Path], credentials: &Value) -> Result<Value> {
    private_scan(&format!(
        "{LOG_STREAM}\n{PRIVACY}\nprint(json.dumps(scan(json.loads({:?}), json.loads({:?}))))",
        serde_json::to_string(roots)?,
        credentials.to_string()
    ))
}

fn raw_logs(artifacts: &Path) -> Result<PathBuf> {
    Ok(artifacts
        .parent()
        .context("evidence work directory missing")?
        .join("raw-logs"))
}

pub(crate) fn format_log(source: &Path, destination: &Path) -> Result<()> {
    let script = format!(
        "__name__ = 'ilex_fixture_diagnostics'\n{LOG_STREAM}\n{DIAGNOSTICS}\npathlib.Path(sys.argv[2]).write_bytes(tail(pathlib.Path(sys.argv[1])))"
    );
    ensure!(
        Command::new("python3")
            .args(["-I", "-S", "-c", &script])
            .arg(source)
            .arg(destination)
            .output()?
            .status
            .success(),
        "could not format private log"
    );
    Ok(())
}

fn diagnostics(artifacts: &Path, output: &Path, credentials: &Value) -> Result<()> {
    let mut command = Command::new("python3");
    let script = format!("{LOG_STREAM}\n{DIAGNOSTICS}");
    command.args(["-I", "-S", "-c", &script]).arg(output);
    let private = raw_logs(artifacts)?;
    if private.try_exists()? {
        command.args(evidence_files(&private)?);
    }
    for file in [
        "no-tpm-unit-journal.txt",
        "no-tpm-luksDump.txt",
        "no-tpm-failed-units.txt",
        "host-fixture-error.txt",
        "installer-probe.json",
        "probe.json",
    ] {
        command.arg(artifacts.join(file));
    }
    ensure!(
        command.output()?.status.success(),
        "could not format private diagnostics"
    );
    artifact_privacy(&[output], credentials)?;
    Ok(())
}

fn guest_privacy(vm: &qemu::Vm, artifacts: &Path, label: &str, credentials: &Value) -> Result<()> {
    let script = format!(
        "__name__ = 'ilex_private_matcher'\n{LOG_STREAM}\n{PRIVACY}\nprint(json.dumps(scan(['/etc', '/tmp', '/var/tmp', '/var/log', '/var/lib/anaconda', '/var/lib/ilex', '/var/roothome', '/root', '/var/lib/systemd/coredump', '/var/spool/abrt', '/run/anaconda', '/run/ilex', '/run/ilex-enroll'], json.loads({:?}), journal=True)))",
        credentials.to_string()
    );
    let command = format!("python3 -I -S -c '{}'", script.replace('\'', "'\\''"));
    let output = vm.ssh(&command)?;
    ensure!(
        output.status.success(),
        "post-finalisation private scan failed; diagnostics suppressed"
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(
        result["secret_found"] == false
            && result["read_errors"] == 0
            && result["journal_scanned"] == true,
        "post-finalisation privacy failed; details suppressed"
    );
    fs::write(
        artifacts.join(format!("{label}-privacy.json")),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(())
}

/// A random lowercase secret of 48 letters: what a virtual keyboard can type
/// at the console prompts, and the length the evidence seal expects.
fn random_secret() -> Result<Secret> {
    let mut bytes = [0u8; 48];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    for byte in &mut bytes {
        *byte = b'a' + *byte % 26;
    }
    let secret = Secret::read(ilex_enroll::secret_fd(&bytes)?)?;
    bytes.fill(0);
    Ok(secret)
}

/// What the installation of the live installer leaves for the post-install
/// gates.
struct Installed {
    fixture: Fixture,
    /// The recovery key the helper handed over once and the person confirmed.
    recovery: Secret,
    /// `ilex-enroll probe --json` of the live installer.
    probe: Value,
    /// `ilex-enroll status --json` of the disk the installation wrote, before
    /// anything booted it.
    tokens: Value,
    /// The manifest digest the installation pinned.
    digest: String,
}

/// An encrypted installation by the live installer of the GNOME edition (the
/// ISO named by `ILEX_ISO_NEXT`): the real root helper, driven over its bus as
/// the interface drives it, on a Secure Boot VM with a persistent TPM, a
/// recovery key and the chip. With `testing_tag` the helper installs that tag
/// of the registry (a signed image of an earlier release) instead of the
/// composition's own.
fn install_with_helper(
    ws: &Workspace,
    work: &Path,
    artifacts: &Path,
    passphrase: &Secret,
    password: &Secret,
    testing_tag: Option<&str>,
    lifecycle: bool,
) -> Result<Installed> {
    use crate::iso_next::{
        Edition,
        security::{Credentials, Machine, Variant, install_running},
    };
    let mut machine = Machine::boot_iso(Edition::Gnome, work, true, artifacts)?;
    let result = (|| -> Result<(Secret, Value, Value, String)> {
        if let Some(tag) = testing_tag {
            ensure!(
                tag.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)),
                "unexpected installer image tag"
            );
            let vm = machine.vm()?;
            vm.wait_ssh(Duration::from_secs(600))
                .context("the live system did not come up")?;
            // The catalogue's testing tag (what a local `--testing-tag` build
            // writes) installs that tag for every composition: the signed
            // image of the release being upgraded from. Signatures are still
            // verified by the shipped policy.
            vm.ssh_ok(&format!(
                r#"set -eu
python3 -I -S - '{tag}' <<'PY'
import json, sys
path = '/usr/share/ilex-installer/compositions.json'
table = json.load(open(path))
table['testing-tag'] = sys.argv[1]
with open(path, 'w') as out:
    json.dump(table, out, indent=2)
PY
systemctl restart ilex-installerd.service"#
            ))?;
        }
        let (pass, pw) = (
            std::str::from_utf8(passphrase.bytes())?,
            std::str::from_utf8(password.bytes())?,
        );
        let mut secrets = vec![pass.to_owned(), pw.to_owned()];
        let name = "encrypted-install";
        let key = install_running(
            &mut machine,
            Variant::Confirmed,
            artifacts,
            name,
            &mut secrets,
            Credentials {
                passphrase: pass,
                password: pw,
                // The lifecycle's homes apply this public profile: the
                // installer's Daily user page writes the choice into
                // `setup.json`.
                daily_profile: lifecycle.then_some("andriishafar"),
            },
        )?;
        secrets.fill(String::new());
        let recovery = Secret::read(ilex_enroll::secret_fd(key.as_bytes())?)?;
        ensure!(
            recovery.bytes().len() == 71,
            "the recovery key is not in the form systemd makes"
        );
        // What the installation pinned.
        let log = fs::read_to_string(artifacts.join(format!("next-{name}-install.txt")))?;
        let plan: Value = serde_json::from_str(
            log.lines()
                .find_map(|l| l.strip_prefix("PLAN "))
                .context("no plan in the install log")?,
        )?;
        let digest = plan["digest"]
            .as_str()
            .context("the plan has no digest")?
            .to_owned();
        let probe: Value = serde_json::from_slice(&fs::read(
            artifacts.join(format!("next-{name}-probe.json")),
        )?)?;
        // The header the installation wrote, before a first boot touches it.
        let tokens: Value = serde_json::from_str(&machine.vm()?.ssh_ok(
            r#"set -eu
device=$(blkid -t TYPE=crypto_LUKS -o device | head -n 1)
test -b "$device"
uuid=$(cryptsetup luksUUID "$device")
udevadm settle
ilex-enroll status --json --device "/dev/disk/by-uuid/$uuid""#,
        )?)?;
        // The installed system's console: the harness reads its prompts from
        // the serial port and types the credentials of the fallback boots to
        // it, so the kernel's last `console=` has to be the port (as the
        // Anaconda fixture's `bootloader --append` made it). Only the boot
        // entry changes; nothing PCR 7 measures does.
        machine.vm()?.ssh_ok(
            r#"set -eu
device=/dev/disk/by-partlabel/ilex-boot
test -b "$device"
mkdir -p /run/target-boot
mount "$device" /run/target-boot
trap 'umount /run/target-boot' EXIT
entries=$(find /run/target-boot -path '*/entries/ostree-*.conf')
test -n "$entries"
for entry in $entries; do
    sed -i '/^options /s/$/ console=ttyS0,115200n8/' "$entry"
    grep -q '^options .*console=ttyS0,115200n8$' "$entry"
done
sync"#,
        )?;
        machine.power_off().context("powering the installer off")?;
        Ok((recovery, probe, tokens, digest))
    })();
    let (recovery, probe, tokens, digest) = result?;
    let (spec, root) = machine.release();
    ensure!(root == work, "the installation's files moved");
    let provider = LinuxProvider::new(ws.clone())?;
    let vm = provider.create(spec.clone())?;
    Ok(Installed {
        fixture: Fixture {
            provider,
            vm,
            spec,
            console: None,
        },
        recovery,
        probe,
        tokens,
        digest,
    })
}

fn guest_json(vm: &qemu::Vm, artifacts: &Path, file: &str, script: &str) -> Result<Value> {
    let bytes = vm.ssh_ok(script)?;
    let value: Value = serde_json::from_str(&bytes)?;
    fs::write(artifacts.join(file), serde_json::to_vec_pretty(&value)?)?;
    Ok(value)
}

fn type_at_vt(vm: &qemu::Vm, console: &mut PrivateConsole, credential: &Secret) -> Result<()> {
    send_private_input(
        vm,
        console,
        credential,
        &format!("vt_input({})", credential.bytes().len()),
    )
}

fn send_private_input(
    vm: &qemu::Vm,
    console: &mut PrivateConsole,
    credential: &Secret,
    action: &str,
) -> Result<()> {
    // Only the helper and public arguments cross SSH. The credential arrives
    // on the output-only-logged UART, with no journal or shell interpolation.
    let script = format!("__name__ = 'ilex_fixture_vt_input'\n{GUEST}\nharden()\n{action}");
    let command = format!(
        "set -eu; systemctl stop serial-getty@ttyS0.service; \
        install -d -m 0700 /run/ilex-encrypted-test; \
        rm -f /run/ilex-encrypted-test/vt-input-ready; \
        systemd-run --quiet --remain-after-exit --unit=ilex-encrypted-vt-input \
        /usr/bin/python3 -I -S -c '{}'",
        script.replace('\'', "'\\''")
    );
    ensure!(vm.ssh(&command)?.status.success(), "start private VT input");
    vm.ssh_ok("timeout 30 sh -c 'until test -f /run/ilex-encrypted-test/vt-input-ready; do sleep 0.2; done'")?;
    console.credential(credential)?;
    vm.ssh_ok("set -eu; timeout 45 sh -c 'until test \"$(systemctl show -p SubState --value ilex-encrypted-vt-input)\" = exited; do test \"$(systemctl show -p ActiveState --value ilex-encrypted-vt-input)\" != failed || exit 1; sleep 0.2; done'; test \"$(systemctl show -p Result --value ilex-encrypted-vt-input)\" = success; systemctl stop ilex-encrypted-vt-input; rm -f /run/ilex-encrypted-test/vt-input-ready")?;
    Ok(())
}

fn poweroff(fixture: &mut Fixture, artifacts: &Path, label: &str) -> Result<()> {
    let vm = fixture.vm.running_mut()?;
    let _ = vm.ssh("systemctl poweroff");
    let start = Instant::now();
    while vm.exited()?.is_none() {
        ensure!(
            start.elapsed() < Duration::from_secs(120),
            "guest poweroff timed out"
        );
        thread::sleep(Duration::from_millis(200));
    }
    fixture.logs(artifacts, label)?;
    fixture.provider.stop(&mut fixture.vm)
}

#[allow(clippy::too_many_arguments)] // One explicit disposable negative boot profile.
fn fallback(
    ws: &Workspace,
    source: &Path,
    template: &VmSpec,
    work: &Path,
    artifacts: &Path,
    credential: &Secret,
    credentials: &Value,
    installer_probe: &Value,
    original_tokens: &Value,
    expected: &Value,
    mode: &str,
    repair_passphrase: Option<&Secret>,
) -> Result<()> {
    let disk = work.join(format!("{mode}.qcow2"));
    cmd::run(
        Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2", "-F", "raw", "-b"])
            .arg(source)
            .arg(&disk),
    )?;
    let mut spec = template.clone();
    spec.run.push_str(&format!("-{mode}"));
    // Runtime paths include the owner-qualified checkout slug. Keep QMP and
    // swtpm Unix sockets within sockaddr_un even for the interrupted case.
    let family = match template.purpose.as_str() {
        "ed-l" => "l",
        "encrypted-space" | "ed-s" => "s",
        "encrypted-interrupted" | "ed-i" => "i",
        _ => "a",
    };
    let suffix = match mode {
        "pcr-mismatch" => "pcr",
        "recovery" => "recovery",
        "reissued-recovery" => "reissue",
        "no-tpm" => "no-tpm",
        "layout-recovery" => "layout",
        _ => bail!("unknown fallback case"),
    };
    let local_diagnostic = template.secure_boot == Requirement::Off;
    let prefix = if local_diagnostic { "ed" } else { "enroll" };
    spec.purpose = format!("{prefix}-{family}-{suffix}");
    spec.iso = None;
    spec.additional_disks.clear();
    spec.secure_boot = if mode == "pcr-mismatch" || local_diagnostic {
        Requirement::Off
    } else {
        Requirement::Required
    };
    spec.tpm = if mode == "no-tpm" || local_diagnostic {
        TpmLifecycle::Off
    } else {
        TpmLifecycle::Persistent {
            bundle_id: spec.run.clone(),
        }
    };
    // This negative has a separate identity. It measures fallback behavior and
    // cannot mint a passing T6 firmware/TPM qualification for a different profile.
    if !local_diagnostic {
        spec.security_launch = ilex_devkit::core::SecurityLaunch::QualificationFixture;
    }
    spec.boot = ilex_devkit::core::BootSource::Disk {
        path: disk,
        format: ilex_devkit::core::DiskFormat::Qcow2,
        writable: true,
    };
    let provider = LinuxProvider::new(ws.clone())?;
    let vm = provider.create(spec.clone())?;
    let mut fixture = Fixture {
        provider,
        vm,
        spec,
        console: None,
    };
    fixture.provider.start(&mut fixture.vm)?;
    let result = check_fallback(
        &mut fixture,
        artifacts,
        credential,
        credentials,
        installer_probe,
        original_tokens,
        expected,
        mode,
        local_diagnostic,
    );
    if mode == "no-tpm"
        && let Ok(vm) = fixture.vm.running_mut()
    {
        save_fallback_output(
            vm,
            artifacts,
            "no-tpm-unit-journal.txt",
            "journalctl -b -u ilex-enroll-no-tpm-completion.service --no-pager -n 200 -o short-monotonic",
        );
        save_fallback_output(
            vm,
            artifacts,
            "no-tpm-failed-units.txt",
            "systemctl --failed --no-legend --plain",
        );
    }
    if result.is_err() {
        let _ = fixture.logs(artifacts, mode);
    }
    result.map_err(|error| {
        error.context(GuestFailure::capture(Some(&mut fixture), "fallback_boot"))
    })?;
    if let Some(passphrase) = repair_passphrase
        && mode == "recovery"
        && !local_diagnostic
        && original_tokens["tokens"]
            .as_array()
            .is_some_and(|tokens| tokens.iter().any(|t| t["pcr7_sha256"] == true))
    {
        if expected["tpm_slot_budget"] == true {
            full_header_repair(
                &mut fixture,
                artifacts,
                credential,
                credentials,
                original_tokens,
            )
            .map_err(|error| {
                error.context(GuestFailure::capture(
                    Some(&mut fixture),
                    "full_header_repair",
                ))
            })?;
        } else {
            repair_fallback(
                &mut fixture,
                artifacts,
                credential,
                passphrase,
                credentials,
                original_tokens,
            )
            .map_err(|error| {
                error.context(GuestFailure::capture(
                    Some(&mut fixture),
                    "authenticated_repair",
                ))
            })?;
        }
    }
    fixture.logs(artifacts, mode)?;
    poweroff(&mut fixture, artifacts, mode)?;
    fixture.provider.destroy_owned(&mut fixture.vm)
}

#[allow(clippy::too_many_arguments)] // Assertions for a single disposable fallback VM.
fn check_fallback(
    fixture: &mut Fixture,
    artifacts: &Path,
    credential: &Secret,
    credentials: &Value,
    installer_probe: &Value,
    original_tokens: &Value,
    expected: &Value,
    mode: &str,
    local_diagnostic: bool,
) -> Result<()> {
    fixture.private_console()?;
    let vm = fixture.vm.running_mut()?;
    let start = Instant::now();
    loop {
        let serial = fs::read_to_string(vm.info.dir.join("serial.log"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if serial.contains("please enter")
            && (serial.contains("passphrase") || serial.contains("password"))
        {
            break;
        }
        ensure!(
            vm.exited()?.is_none() && start.elapsed() < Duration::from_secs(900),
            "no-TPM guest did not request fallback authentication"
        );
        thread::sleep(Duration::from_secs(2));
    }
    let ctl = Vmctl::from_info(vm.info.clone());
    ctl.save_screenshot(&artifacts.join(format!("{mode}-passphrase-prompt.png")))?;
    fixture
        .console
        .as_mut()
        .context("private console unavailable")?
        .credential(credential)?;
    vm.wait_ssh(Duration::from_secs(900))?;
    let probe = guest_json(
        vm,
        artifacts,
        &format!("{mode}-probe.json"),
        "ilex-enroll probe --json",
    )?;
    let deferred_reason = if local_diagnostic {
        "secure-boot-off"
    } else {
        "no-tpm"
    };
    if mode == "no-tpm" || local_diagnostic {
        let firmware_matches = if local_diagnostic {
            probe["secure_boot"] != true
        } else {
            probe["secure_boot"] == true
        };
        ensure!(
            firmware_matches && probe["tpm2"] == false && probe["reason"] == deferred_reason,
            "no-TPM mode was not preserved"
        );
    } else if mode == "pcr-mismatch" {
        let pcr = vm.ssh_ok("cat /sys/class/tpm/tpm0/pcr-sha256/7")?;
        let eventlog = vm.ssh("cat /sys/kernel/security/tpm0/binary_bios_measurements")?;
        ensure!(eventlog.status.success(), "mismatch event log missing");
        ensure!(
            probe["secure_boot"] == false
                && probe["tpm2"] == true
                && pcr.len() == 64
                && pcr.bytes().all(|b| b.is_ascii_hexdigit())
                && pcr.bytes().any(|b| b != b'0')
                && installer_probe["pcr7"] != pcr.trim(),
            "present TPM / changed Secure Boot PCR was not measured"
        );
        // The positive replay deliberately requires Secure Boot authority
        // events. Disabled Secure Boot omits those events; it is not an
        // eligible profile. Preserve its raw log and measured PCR instead of
        // applying (or weakening) the positive qualification contract.
        fs::write(artifacts.join("mismatch-eventlog.bin"), eventlog.stdout)?;
        fs::write(
            artifacts.join("mismatch-pcr.json"),
            serde_json::to_vec_pretty(&json!({"pcr7":pcr,
            "installer_pcr7":installer_probe["pcr7"],
            "secure_boot":false,"tpm2":true,"qualification":"unqualified"}))?,
        )?;
    } else {
        ensure!(
            probe["secure_boot"] == true && probe["tpm2"] == true,
            "recovery boot must retain Secure Boot and a present, different TPM"
        );
    }
    if mode == "layout-recovery" {
        vm.ssh_ok("set -eu; test ! -e /run/ilex/root-unlock.json; journalctl -b -u ilex-enrollment-report.service --no-pager | grep -F 'TPM enrollment was skipped for this storage layout'; if layout=$(ilex-enroll layout --root / 2>&1); then exit 1; fi; printf '%s\n' \"$layout\" | grep -F 'unsupported device-mapper layout'")?;
    } else {
        let evidence = guest_json(
            vm,
            artifacts,
            &format!("{mode}-unlock.json"),
            "cat /run/ilex/root-unlock.json",
        )?;
        ensure!(
            evidence["method"] == "fallback"
                && evidence["uuid"] == original_tokens["uuid"]
                && evidence["boot_id"] == vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?.trim(),
            "fallback lacks current root password-agent evidence"
        );
        let layout = guest_json(
            vm,
            artifacts,
            &format!("{mode}-layout.json"),
            "ilex-enroll layout --root /",
        )?;
        ensure!(
            layout["uuid"] == evidence["uuid"],
            "fallback evidence does not describe encrypted root"
        );
    }
    let bootc = guest_json(
        vm,
        artifacts,
        &format!("{mode}-bootc.json"),
        "bootc status --json",
    )?;
    ensure!(
        bootc["status"]["booted"]["image"]["imageDigest"] == expected["digest"]
            && bootc["spec"]["image"]["signature"] == "containerPolicy",
        "fallback boot did not use the exact enforcing candidate"
    );
    // Reuse acknowledged recovery during a local completion retry with no
    // TPM. This must defer to passphrase-only mode without generating a key
    // or changing any slot/token. No credential enters SSH, argv or a unit.
    let before = guest_json(
        vm,
        artifacts,
        &format!("{mode}-enrollment-before.json"),
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        mode != "no-tpm" || before["recovery"]["acknowledged"] == true,
        "recovery acknowledgement missing"
    );
    let uuid = before["uuid"]
        .as_str()
        .context("missing no-TPM root UUID")?;
    ensure!(
        uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "invalid no-TPM root UUID"
    );
    let token_command = format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}");
    let tokens_before = guest_json(
        vm,
        artifacts,
        &format!("{mode}-tokens-before.json"),
        &token_command,
    )?;
    if mode == "no-tpm" {
        save_fallback_output(
            vm,
            artifacts,
            "no-tpm-luksDump.txt",
            &format!("cryptsetup luksDump /dev/disk/by-uuid/{uuid}"),
        );
    }
    ensure!(
        tokens_before == *original_tokens,
        "fallback boot changed slots/tokens or resealed automatically"
    );
    if matches!(mode, "no-tpm" | "pcr-mismatch") {
        let deferred = guest_json(
            vm,
            artifacts,
            &format!("{mode}-repair-deferred.json"),
            "timeout 30 ilexctl security repair",
        )?;
        ensure!(
            deferred["state"] == "unavailable" && deferred["reason"] == probe["reason"],
            "ineligible repair did not defer before asking for a credential"
        );
    }
    if mode == "no-tpm" {
        vm.ssh_ok("chvt 8; systemd-run --quiet --remain-after-exit --unit=ilex-enroll-no-tpm-completion /usr/bin/ilex-enroll complete --root / --tty /dev/tty8").context("start no-TPM completion")?;
        vm.ssh_ok("timeout 60 sh -c 'until grep -aFq \"Re-enter the disk passphrase\" /dev/vcs8; do sleep 1; done'").context("wait for no-TPM passphrase prompt")?;
        type_at_vt(
            vm,
            fixture
                .console
                .as_mut()
                .context("private console unavailable")?,
            credential,
        )?;
        vm.ssh_ok("timeout 120 sh -c 'until grep -aFq \"Installation completion is ready.\" /dev/vcs8; do sleep 1; done'").context("wait for no-TPM completion confirmation")?;
        ctl.save_screenshot(&artifacts.join(format!("{mode}-passphrase-only.png")))?;
        ctl.key("ret")?;
        vm.ssh_ok("set -eu; timeout 30 sh -c 'until test \"$(systemctl show -p SubState --value ilex-enroll-no-tpm-completion)\" = exited; do sleep 1; done'; test \"$(systemctl show -p Result --value ilex-enroll-no-tpm-completion)\" = success; systemctl stop ilex-enroll-no-tpm-completion").context("verify no-TPM completion exit")?;
        let after = guest_json(
            vm,
            artifacts,
            &format!("{mode}-enrollment.json"),
            "cat /var/lib/ilex/security/enrollment.json",
        )?;
        let setup = guest_json(
            vm,
            artifacts,
            &format!("{mode}-setup.json"),
            "cat /etc/ilex/setup.json",
        )?;
        ensure!(
            after["state"] == "unavailable"
                && after["reason"] == deferred_reason
                && after["recovery"] == before["recovery"]
                && setup["security"]["tpm"]["status"] == "passphrase-only",
            "no-TPM completion did not preserve passphrase-only mode"
        );
        let tokens_after = guest_json(
            vm,
            artifacts,
            &format!("{mode}-tokens.json"),
            &token_command,
        )?;
        ensure!(
            tokens_after == tokens_before,
            "no-TPM completion changed LUKS slots/tokens"
        );
    }
    if mode == "layout-recovery" {
        ensure!(
            before["state"] == "unavailable"
                && before["reason"] == "unsupported-storage-layout"
                && before["recovery"]["acknowledged"] == true
                && before["tpm"].is_null()
                && tokens_before["slots"]
                    .as_array()
                    .is_some_and(|slots| slots.len() == 32)
                && tokens_before["tokens"].as_array().is_some_and(
                    |tokens| tokens.len() == 1 && tokens[0]["type"] == "systemd-recovery"
                ),
            "unsupported installation did not defer TPM enrollment and retain recovery"
        );
        vm.ssh_ok("set -eu; for attempt in 1 2; do report=$(timeout 30 ilex-enroll complete --root / --tty /dev/tty8 2>&1); printf '%s\n' \"$report\" | grep -F 'Installation can finish'; done; ! grep -Fq tpm2-device= /etc/crypttab")?;
        let after = guest_json(
            vm,
            artifacts,
            "layout-recovery-enrollment.json",
            "cat /var/lib/ilex/security/enrollment.json",
        )?;
        let setup = guest_json(
            vm,
            artifacts,
            "layout-recovery-setup.json",
            "cat /etc/ilex/setup.json",
        )?;
        let tokens_after =
            guest_json(vm, artifacts, "layout-recovery-tokens.json", &token_command)?;
        ensure!(
            before == after
                && tokens_before == tokens_after
                && setup["security"]["tpm"]["status"] == "passphrase-only"
                && setup["security"]["tpm"]["reason"] == "unsupported-storage-layout",
            "unsupported-layout retry changed recovery or enrollment state"
        );
    } else {
        vm.ssh_ok("ilex-enroll layout --root /")?;
    }
    vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; test \"$(stat -c %a /etc/ilex/setup.json)\" = 644; test -z \"$(systemctl --failed --no-legend --plain)\"")?;
    guest_privacy(vm, artifacts, mode, credentials)?;
    Ok(())
}

fn save_fallback_output(vm: &qemu::Vm, artifacts: &Path, file: &str, command: &str) {
    if let Ok(output) = vm.ssh(command) {
        let mut text = output.stdout;
        text.extend(output.stderr);
        let _ = fs::write(artifacts.join(file), text);
    }
}

fn header_digest(vm: &qemu::Vm, uuid: &str) -> Result<String> {
    vm.ssh_ok(&format!(
        r#"set -eu
umask 077
scratch=$(mktemp -d /run/ilex-header-digest.XXXXXXXX)
trap 'rm -f "$scratch/header"; rmdir "$scratch"' EXIT
cryptsetup luksHeaderBackup /dev/disk/by-uuid/{uuid} --header-backup-file "$scratch/header"
sha256sum < "$scratch/header"
"#
    ))
}

fn full_header_repair(
    fixture: &mut Fixture,
    artifacts: &Path,
    recovery: &Secret,
    credentials: &Value,
    original: &Value,
) -> Result<()> {
    let vm = fixture.vm.running_mut()?;
    let uuid = original["uuid"].as_str().context("root UUID missing")?;
    vm.ssh_ok("systemctl start ilex-security-verify.service")?;
    ensure!(
        original["slots"]
            .as_array()
            .is_some_and(|slots| slots.len() == 32),
        "expected every LUKS2 slot to be occupied"
    );
    let tokens = original["tokens"]
        .as_array()
        .context("root tokens missing")?;
    let tpm_token: u32 = tokens
        .iter()
        .find(|token| token["type"] == "systemd-tpm2")
        .and_then(|token| token["id"].as_str())
        .context("TPM token missing")?
        .parse()?;

    // A full header can still replace a provably owned TPM slot. Orphan that
    // slot on this disposable overlay so repair must find genuinely free space.
    let device = format!("/dev/disk/by-uuid/{uuid}");
    vm.ssh_ok(&format!(
        "cryptsetup token remove --token-id {tpm_token} {device}"
    ))?;
    let status = format!("ilex-enroll status --json --device {device}");
    let full: Value = serde_json::from_str(&vm.ssh_ok(&status)?)?;
    let remaining: Vec<_> = tokens
        .iter()
        .filter(|token| token["type"] != "systemd-tpm2")
        .collect();
    ensure!(
        full["slots"] == original["slots"] && full["tokens"] == json!(remaining),
        "fixture did not retain all slots and recovery metadata"
    );
    let header_before = header_digest(vm, uuid)?;
    let state_digest = "sha256sum /var/lib/ilex/security/enrollment.json /etc/ilex/setup.json";
    let state_before = vm.ssh_ok(state_digest)?;
    vm.ssh_ok(r#"set -eu
chvt 8
systemd-run --quiet --remain-after-exit --unit=ilex-fixture-full-repair -p SuccessExitStatus=1 /usr/bin/ilexctl security repair
timeout 90 sh -c 'until grep -aFq "for TPM repair (input is hidden)" /dev/vcs8; do sleep 1; done'
"#)?;
    let private = fixture
        .console
        .as_mut()
        .context("private console unavailable")?;
    type_at_vt(vm, private, recovery)?;
    vm.ssh_ok(r#"set -eu
timeout 180 sh -c 'until test "$(systemctl show -p SubState --value ilex-fixture-full-repair)" = exited; do sleep 1; done'
test "$(systemctl show -p ExecMainStatus --value ilex-fixture-full-repair)" = 1
journalctl -b -u ilex-fixture-full-repair --no-pager -o cat | grep -Fq 'LUKS2 has no free keyslot or keyslot area'
systemctl stop ilex-fixture-full-repair
test ! -e /var/lib/ilex/security/.pending-tpm.json
test ! -e /var/lib/ilex/security/.pending-recovery.json
"#).context("repair did not refuse for lack of a free LUKS2 slot")?;
    ensure!(
        header_digest(vm, uuid)? == header_before && vm.ssh_ok(state_digest)? == state_before,
        "full-header repair changed credential data or enrollment state"
    );
    fs::write(
        artifacts.join("full-header-repair.json"),
        b"{\"occupied_slots\":32,\"authenticated_refusal\":true,\"header_unchanged\":true,\"enrollment_unchanged\":true}\n",
    )?;
    guest_privacy(vm, artifacts, "full-header-repair", credentials)
}

fn repair_fallback(
    fixture: &mut Fixture,
    artifacts: &Path,
    recovery: &Secret,
    passphrase: &Secret,
    credentials: &Value,
    original: &Value,
) -> Result<()> {
    let vm = fixture.vm.running_mut()?;
    let boot = vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?;
    let uuid = original["uuid"].as_str().context("root UUID missing")?;
    let status = format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}");
    let verify = guest_json(
        vm,
        artifacts,
        "repair-before-verification.json",
        "ilexctl security verify",
    )?;
    ensure!(
        verify["state"] == "error" && verify["reason"] == "fallback-unlock-observed",
        "wrong TPM fallback did not enter verification error"
    );
    let before = guest_json(
        vm,
        artifacts,
        "repair-before-state.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    let original_digest = header_digest(vm, uuid)?;
    let mut bad_input = Secret::empty()?;
    let wrong = if passphrase.bytes() == [b'x'; 48] {
        b'z'
    } else {
        b'x'
    };
    for _ in 0..48 {
        bad_input.push(wrong)?;
    }
    vm.ssh_ok(r#"set -eu
chvt 8
systemd-run --quiet --remain-after-exit --unit=ilex-fixture-wrong-repair -p SuccessExitStatus=1 /usr/bin/ilexctl security repair
timeout 90 sh -c 'until grep -aFq "for TPM repair (input is hidden)" /dev/vcs8; do sleep 1; done'
"#)?;
    let private = fixture
        .console
        .as_mut()
        .context("private console unavailable")?;
    type_at_vt(vm, private, &bad_input)?;
    vm.ssh_ok(r#"set -eu
timeout 90 sh -c 'until test "$(systemctl show -p SubState --value ilex-fixture-wrong-repair)" = exited; do sleep 1; done'
test "$(systemctl show -p ExecMainStatus --value ilex-fixture-wrong-repair)" = 1
systemctl stop ilex-fixture-wrong-repair
test ! -e /var/lib/ilex/security/.pending-tpm.json
"#)?;
    ensure!(
        header_digest(vm, uuid)? == original_digest,
        "wrong repair credential changed the LUKS2 header"
    );
    let rejected = guest_json(
        vm,
        artifacts,
        "wrong-repair-state.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        rejected == before,
        "wrong repair credential changed the enrollment state"
    );
    // Run the shipped binary in an isolated mount namespace. The fixture
    // wrapper delegates every operation to the real cryptsetup, then cuts
    // the caller at one successful write. The guest root and host are intact.
    vm.ssh_ok(
        r#"set -eu
dir=/run/ilex-encrypted-test
install -d -m 0700 "$dir"
touch "$dir/cryptsetup-original"
cat > "$dir/cryptsetup-cut" <<'EOF'
#!/bin/sh
set -eu
dir=/run/ilex-encrypted-test
"$dir/cryptsetup-original" "$@"
cut=$(cat "$dir/repair-cut")
if [ "$cut" = "$1:${2-}" ]; then
    printf '%s\n' "$cut" > "$dir/repair-cut-hit"
    kill -KILL "$PPID"
fi
EOF
chmod 0700 "$dir/cryptsetup-cut"
chvt 8
"#,
    )?;
    let ctl = Vmctl::from_info(vm.info.clone());
    for (cut, evidence) in [
        ("token:remove", "retirement"),
        ("luksAddKey:--batch-mode", "allocation"),
    ] {
        vm.ssh_ok(&format!(
            r#"set -eu
dir=/run/ilex-encrypted-test
printf '%s\n' '{cut}' > "$dir/repair-cut"
rm -f "$dir/repair-cut-hit"
systemd-run --quiet --remain-after-exit --unit=ilex-fixture-repair -p SuccessExitStatus=SIGKILL \
unshare --mount --propagation private sh -ec '
    dir=/run/ilex-encrypted-test
    mount --bind /usr/sbin/cryptsetup "$dir/cryptsetup-original"
    exec /usr/bin/ilexctl security repair'
timeout 90 sh -c 'until grep -aFq "for TPM repair (input is hidden)" /dev/vcs8; do sleep 1; done'
pid=$(systemctl show -p MainPID --value ilex-fixture-repair)
test "$pid" -gt 1
nsenter --mount="/proc/$pid/ns/mnt" mount --bind "$dir/cryptsetup-cut" /usr/sbin/cryptsetup
"#
        ))?;
        if evidence == "retirement" {
            ctl.save_screenshot(&artifacts.join("authenticated-tpm-repair-prompt.png"))?;
        }
        type_at_vt(vm, private, recovery)?;
        vm.ssh_ok(&format!(r#"set -eu
timeout 180 sh -c 'until test "$(systemctl show -p SubState --value ilex-fixture-repair)" = exited; do sleep 1; done'
test "$(cat /run/ilex-encrypted-test/repair-cut-hit)" = '{cut}'
test "$(systemctl show -p ExecMainStatus --value ilex-fixture-repair)" = 9
systemctl stop ilex-fixture-repair
stty echo -F /dev/tty8
printf '\033[2J\033[3J\033[H' > /dev/tty8
test -f /var/lib/ilex/security/.pending-tpm.json
"#))?;
        let header = guest_json(
            vm,
            artifacts,
            &format!("repair-interrupted-{evidence}.json"),
            &status,
        )?;
        ensure!(
            header["slots"] == original["slots"]
                && header["tokens"]
                    .as_array()
                    .is_some_and(|tokens| tokens.iter().all(|t| t["type"] != "systemd-tpm2")),
            "interrupted repair did not leave the planned orphan and retained fallback slots"
        );
    }
    vm.ssh_ok(
        r#"set -eu
systemd-run --quiet --remain-after-exit --unit=ilex-fixture-repair /usr/bin/ilexctl security repair
timeout 90 sh -c 'until grep -aFq "for TPM repair (input is hidden)" /dev/vcs8; do sleep 1; done'
"#,
    )?;
    type_at_vt(vm, private, recovery)?;
    vm.ssh_ok(r#"set -eu
timeout 180 sh -c 'until test "$(systemctl show -p SubState --value ilex-fixture-repair)" = exited; do sleep 1; done'
test "$(systemctl show -p Result --value ilex-fixture-repair)" = success
systemctl stop ilex-fixture-repair
test ! -e /var/lib/ilex/security/.pending-tpm.json
test ! -e /var/lib/ilex/security/.pending-recovery.json
"#)?;
    let pending = guest_json(
        vm,
        artifacts,
        "repair-pending.json",
        "ilexctl security verify",
    )?;
    let after = guest_json(
        vm,
        artifacts,
        "repair-after-state.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    let new_header = guest_json(vm, artifacts, "repair-after-header.json", &status)?;
    ensure!(
        pending["state"] == "pending"
            && after["recovery"] == before["recovery"]
            && after["tpm_header"] != before["tpm_header"]
            && new_header == *original,
        "repair leaked a slot/token, changed recovery, or verified before a cold boot"
    );
    let recovery_slot = before["recovery"]["slot"]
        .as_u64()
        .context("missing recovery slot")?;
    let tpm_slot = before["tpm"]["slot"]
        .as_u64()
        .context("missing original TPM slot")?;
    let mut other_slots = original["slots"]
        .as_array()
        .context("missing original slots")?
        .iter()
        .filter_map(|id| id.as_str()?.parse::<u64>().ok())
        .filter(|id| *id != recovery_slot && *id != tpm_slot);
    let password_slot = other_slots.next().context("missing disk passphrase slot")?;
    ensure!(
        other_slots.next().is_none(),
        "ambiguous disk passphrase slot"
    );
    for (credential, slot) in [(passphrase, password_slot), (recovery, recovery_slot)] {
        send_private_input(
            vm,
            private,
            credential,
            &format!(
                "verify_fallback_key({}, '{uuid}', {slot})",
                credential.bytes().len()
            ),
        )?;
    }
    fs::write(
        artifacts.join("repair-fallback-authentication.json"),
        serde_json::to_vec_pretty(&json!({
            "passphrase_slot":password_slot, "recovery_slot":recovery_slot, "authenticated":true
        }))?,
    )?;
    guest_privacy(vm, artifacts, "authenticated-tpm-repair", credentials)?;
    poweroff(fixture, artifacts, "before-repaired-cold")?;
    fixture.disk_boot()?;
    let vm = fixture.vm.running_mut()?;
    vm.wait_ssh(Duration::from_secs(900))?;
    let verified = guest_json(
        vm,
        artifacts,
        "repaired-cold-verification.json",
        "ilexctl security verify",
    )?;
    let cold_header = guest_json(vm, artifacts, "repaired-cold-header.json", &status)?;
    ensure!(
        verified["state"] == "verified"
            && verified["method"] == "tpm2"
            && verified["uuid"] == uuid
            && verified["boot_id"] != boot.trim()
            && cold_header == new_header,
        "the repaired TPM did not unlock the same disk automatically on cold boot"
    );
    check_stale_unlock(vm, artifacts, boot.trim(), &verified)?;
    guest_privacy(vm, artifacts, "repaired-cold", credentials)?;
    Ok(())
}

fn check_stale_unlock(
    vm: &qemu::Vm,
    artifacts: &Path,
    stale_boot: &str,
    verified: &Value,
) -> Result<()> {
    ensure!(
        stale_boot.len() == 36
            && stale_boot
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b'-'),
        "unexpected fallback boot ID"
    );
    let stale = guest_json(
        vm,
        artifacts,
        "repaired-stale-evidence.json",
        &format!(
            r#"set -eu
dir=/run/ilex-encrypted-test
install -d -m 0700 "$dir"
sed 's/"boot_id": "[^"]*"/"boot_id": "{stale_boot}"/' /run/ilex/root-unlock.json > "$dir/stale-evidence"
chmod 0600 "$dir/stale-evidence"
grep -Fq '{stale_boot}' "$dir/stale-evidence"
unshare --mount --propagation private sh -ec '
mount --bind /run/ilex-encrypted-test/stale-evidence /run/ilex/root-unlock.json
exec /usr/bin/ilexctl security verify'
"#
        ),
    )?;
    let persisted = guest_json(
        vm,
        artifacts,
        "repaired-stale-state.json",
        "cat /var/lib/ilex/security/enrollment.json",
    )?;
    ensure!(
        stale["state"] == "pending"
            && stale["boot_id"] == verified["boot_id"]
            && stale["method"].is_null()
            && persisted["state"] == "pending"
            && persisted["verification"] == stale,
        "stale unlock evidence left a verified state"
    );
    let restored = guest_json(
        vm,
        artifacts,
        "repaired-restored-verification.json",
        "ilexctl security verify",
    )?;
    ensure!(
        restored == *verified,
        "real current boot evidence did not restore verification"
    );
    vm.ssh_ok("rm -f /run/ilex-encrypted-test/stale-evidence")?;
    Ok(())
}

fn same_tpm_mismatch(
    ws: &Workspace,
    fixture: &mut Fixture,
    artifacts: &Path,
    passphrase: &Secret,
    credentials: &Value,
    installer_probe: &Value,
    tokens: &Value,
) -> Result<()> {
    let (vars, original_vars) = recovery_boot(ws, fixture, artifacts, passphrase)?;
    let vm = fixture.vm.running_mut()?;
    let probe = guest_json(
        vm,
        artifacts,
        "same-tpm-mismatch-probe.json",
        "ilex-enroll probe --json",
    )?;
    let pcr = vm.ssh_ok("cat /sys/class/tpm/tpm0/pcr-sha256/7")?;
    ensure!(
        probe["secure_boot"] == false
            && probe["tpm2"] == true
            && pcr.len() == 64
            && installer_probe["pcr7"] != pcr.trim(),
        "same-TPM Secure Boot/PCR policy was not changed"
    );
    fs::write(artifacts.join("same-tpm-mismatch-pcr.txt"), pcr)?;
    let unlock = guest_json(
        vm,
        artifacts,
        "same-tpm-mismatch-unlock.json",
        "cat /run/ilex/root-unlock.json",
    )?;
    ensure!(
        unlock["method"] == "fallback"
            && unlock["uuid"] == tokens["uuid"]
            && unlock["boot_id"] == vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?.trim(),
        "policy rejection lacks matching root fallback evidence"
    );
    let uuid = tokens["uuid"].as_str().context("root UUID missing")?;
    let after = guest_json(
        vm,
        artifacts,
        "same-tpm-mismatch-tokens.json",
        &format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}"),
    )?;
    ensure!(
        after == *tokens,
        "policy mismatch automatically resealed or changed fallback slots"
    );
    guest_privacy(vm, artifacts, "same-tpm-mismatch", credentials)?;
    poweroff(fixture, artifacts, "same-tpm-mismatch")?;
    fs::write(vars, original_vars)?;
    Ok(())
}

/// The caller must stop the disposable guest before restoring its saved VARS.
fn recovery_boot(
    ws: &Workspace,
    fixture: &mut Fixture,
    artifacts: &Path,
    credential: &Secret,
) -> Result<(PathBuf, Vec<u8>)> {
    let mut negative = fixture.spec.clone();
    negative.security_launch = ilex_devkit::core::SecurityLaunch::QualificationFixture;
    let vars = security::bundle_path(ws, &negative)?.join("VARS.fd");
    let original_vars = fs::read(&vars)?;
    security::disable_fixture_secure_boot(ws, &negative)?;
    fixture.disk_boot()?;
    fixture.private_console()?;
    let vm = fixture.vm.running_mut()?;
    let ctl = Vmctl::from_info(vm.info.clone());
    let start = Instant::now();
    loop {
        let serial = fs::read_to_string(vm.info.dir.join("serial.log"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if serial.contains("please enter")
            && (serial.contains("passphrase") || serial.contains("password"))
        {
            break;
        }
        ensure!(
            vm.exited()?.is_none() && start.elapsed() < Duration::from_secs(1800),
            "same-TPM PCR mismatch did not retain a prompt"
        );
        thread::sleep(Duration::from_secs(2));
    }
    ctl.save_screenshot(&artifacts.join("same-tpm-pcr-mismatch-prompt.png"))?;
    fixture
        .console
        .as_mut()
        .context("private console unavailable")?
        .credential(credential)?;
    vm.wait_ssh(Duration::from_secs(1800))?;
    Ok((vars, original_vars))
}

fn assertions(
    fixture: &mut Fixture,
    artifacts: &Path,
    expected: &Value,
    installer_probe: &Value,
) -> Result<()> {
    let mut check = "automatic_ssh";
    let result = (|| {
        let vm = fixture.vm.running_mut()?;
        vm.wait_ssh(Duration::from_secs(900))?; // No keyboard/password on first boot.
        check = "installed_manifest";
        let bootc = guest_json(vm, artifacts, "bootc.json", "bootc status --json")?;
        ensure!(
            bootc["status"]["booted"]["image"]["imageDigest"] == expected["digest"],
            "installed digest differs from signed candidate"
        );
        check = "installed_signature_policy";
        ensure!(
            bootc["spec"]["image"]["signature"] == "containerPolicy",
            "target origin is not enforcing"
        );
        check = "installed_pcr7";
        let probe = guest_json(vm, artifacts, "probe.json", "ilex-enroll probe --json")?;
        ensure!(
            probe["verdict"] == "eligible" && probe["pcr7"] == installer_probe["pcr7"],
            "ISO/installed PCR 7 mismatch"
        );
        check = "installed_pcr7_replay";
        let raw = vm.ssh("cat /sys/kernel/security/tpm0/binary_bios_measurements")?;
        ensure!(raw.status.success(), "installed event log unavailable");
        fs::write(artifacts.join("eventlog.bin"), &raw.stdout)?;
        let replay = security::replay_pcr7(&raw.stdout)?;
        ensure!(
            probe["pcr7"] == replay.sha256,
            "installed event log replay mismatch"
        );
        fs::write(
            artifacts.join("replay.json"),
            serde_json::to_vec_pretty(&replay)?,
        )?;
        check = "installed_root_unlock";
        let layout = guest_json(vm, artifacts, "layout.json", "ilex-enroll layout --root /")?;
        let evidence = guest_json(
            vm,
            artifacts,
            "root-unlock.json",
            "cat /run/ilex/root-unlock.json",
        )?;
        ensure!(
            evidence["schema"] == 1
                && evidence["uuid"] == layout["uuid"]
                && evidence["boot_id"] == vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?.trim()
                && evidence["method"] == "tpm2",
            "first boot lacks direct TPM unlock evidence"
        );
        check = "installed_token_policy";
        let uuid = layout["uuid"].as_str().context("no root UUID")?;
        ensure!(
            uuid.len() == 36 && uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'),
            "invalid root UUID"
        );
        let tokens = guest_json(
            vm,
            artifacts,
            "tokens.json",
            &format!("ilex-enroll status --json --device /dev/disk/by-uuid/{uuid}"),
        )?;
        ensure!(
            tokens["tokens"]
                .as_array()
                .context("no tokens")?
                .iter()
                .any(|t| t["pcr7_sha256"] == true),
            "explicit token policy missing"
        );
        check = "installed_verifier_service";
        vm.ssh_ok("systemctl is-enabled --quiet ilex-security-verify.service && timeout -k 10s 240s systemctl is-system-running --wait")?;
        check = "installed_enrollment_state";
        let state = guest_json(
            vm,
            artifacts,
            "enrollment.json",
            "cat /var/lib/ilex/security/enrollment.json",
        )?;
        ensure!(
            state["state"] == "verified"
                && state["recovery"]["acknowledged"] == true
                && state["verification"]["boot_id"] == evidence["boot_id"]
                && state["verification"]["method"] == "tpm2",
            "the boot verifier did not persist the observed TPM unlock"
        );
        ensure!(
            evidence["token"]
                .as_str()
                .and_then(|id| id.parse::<u64>().ok())
                == state["tpm"]["token"].as_u64()
                && evidence["slot"] == state["tpm"]["slot"],
            "unlock evidence differs from the enrolled token/slot"
        );
        check = "installed_root_configuration";
        vm.ssh_ok("set -eu; test \"$(getenforce)\" = Enforcing; test \"$(stat -c %a /etc/ilex/setup.json)\" = 644; grep -qw 'rd.luks.options=tpm2-device=auto' /proc/cmdline; ! grep -q headless /etc/crypttab")?;
        // Preserve structured health before asserting it; a partial diagnostic
        // can identify a failed unit without exposing raw command error chains.
        check = "installed_system_health";
        let failed = guest_json(
            vm,
            artifacts,
            "failed-units.json",
            "systemctl list-units --all --state=failed --output=json --no-pager",
        )?;
        ensure!(
            failed.as_array().is_some_and(Vec::is_empty),
            "installed system health: failed units"
        );
        let health = vm.ssh("timeout -k 10s 240s systemctl is-system-running --wait")?;
        let state = String::from_utf8_lossy(&health.stdout).trim().to_string();
        fs::write(
            artifacts.join("system-health.json"),
            serde_json::to_vec_pretty(&json!({"state":state,"success":health.status.success()}))?,
        )?;
        ensure!(
            health.status.success() && state == "running",
            "installed system health"
        );
        check = "installed_enrollment_self_check";
        vm.ssh_ok("ilex-enroll --self-check")
            .context("installed enrollment self-check")?;
        check = "installed_storage_topology";
        vm.ssh_ok(&format!("grep 'UUID={uuid}' /etc/crypttab | grep -q tpm2-device=auto; ! grep -q headless /proc/cmdline"))?;
        let topology = vm.ssh_ok("set -eu; lsblk --json --output NAME,TYPE,FSTYPE,UUID,MOUNTPOINTS; cat /etc/crypttab; cat /proc/cmdline; cat /proc/swaps; for path in /sysroot /var /boot /boot/efi; do findmnt --target \"$path\"; done")?;
        fs::write(artifacts.join("storage.txt"), topology)?;
        Ok::<(), anyhow::Error>(())
    })();
    result.with_context(|| GuestFailure::capture(Some(fixture), check))
}

fn evidence_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files.extend(evidence_files(&entry.path())?);
        } else {
            ensure!(kind.is_file(), "unsupported evidence file type");
            files.push(entry.path());
        }
    }
    Ok(files)
}

fn copy_evidence(source: &Path, destination: &Path) -> Result<()> {
    let files = evidence_files(source)?;
    ensure!(
        files.iter().all(|path| {
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_ascii_lowercase();
            !name.contains("serial") || name.ends_with("-serial.redacted.log")
        }),
        "raw serial log refused at evidence export"
    );
    for path in files {
        let target = destination.join(path.strip_prefix(source)?);
        fs::create_dir_all(target.parent().context("evidence has no parent")?)?;
        fs::copy(path, target)?;
    }
    Ok(())
}

fn clear_evidence(directory: &Path) -> Result<()> {
    // This is only the namespace returned for this checkout's encrypted tier.
    // Remove stale evidence so a failed retry cannot inherit an earlier green.
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// The release gates' tiers. Both install through the live installer of the
/// GNOME edition (`ILEX_ISO_NEXT`), whose helper installs from the registry:
///
/// * `security::encrypted_install` (`lifecycle` false): `ILEX_IMAGE` is the
///   published, signed `ghcr.io/reidond/ilex@sha256:…` the installer's tag
///   resolves to; the installation must pin exactly that digest.
/// * `security::upgrade_rollback` (`lifecycle` true): the baseline
///   (`ILEX_ENCRYPTED_UPGRADE_FROM`, a signed digest, and `ILEX_ENCRYPTED_UPGRADE_TAG`,
///   the registry tag the installer installs it by) is installed, then
///   upgraded to `ILEX_IMAGE` and rolled back.
fn run_fixture(lifecycle: bool) -> Result<()> {
    ilex_enroll::harden()?;
    let ws = common::workspace()?;
    let tier = if lifecycle {
        "encrypted-upgrade-rollback"
    } else {
        "encrypted-install"
    };
    let published = qemu::artifact_dir(&ws, tier)?;
    clear_evidence(&published)?;
    fs::write(
        published.join("result.json"),
        b"{\"qualification\":\"unqualified\",\"reason\":\"run not complete\"}\n",
    )?;
    let run = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let work = ws.ensure_state()?.join("encrypted-fixtures").join(&run);
    fs::create_dir_all(&work)?;
    fs::set_permissions(&work, fs::Permissions::from_mode(0o700))?;
    // Until both privacy gates pass, raw diagnostics remain inside the private
    // fixture directory. CI always uploads only `published`.
    let artifacts = work.join("evidence");
    fs::create_dir(&artifacts)?;
    let start = Instant::now();
    let started_unix_seconds = unix_seconds();
    let mut stage = "platform";
    let mut credentials = Value::Null;
    let finish = |fixture: &mut Fixture,
                  iso: &Path,
                  credentials: &Value,
                  export_fingerprints: &[Value],
                  stage: &mut &str|
     -> Result<()> {
        let iso_hash = cmd::output(Command::new("sha256sum").arg(iso))?;
        fs::write(artifacts.join("iso-sha256.txt"), iso_hash)?;
        *stage = "cleanup";
        // Drop remains the failure-path backstop. On success, propagating
        // destruction errors prevents a retained private TPM bundle going green.
        fixture.provider.destroy_owned(&mut fixture.vm)?;
        *stage = "privacy_export";
        let scan = artifact_privacy(&[&artifacts, &raw_logs(&artifacts)?], credentials)?;
        diagnostics(&artifacts, &artifacts.join("diagnostics"), credentials)?;
        fs::write(
            artifacts.join("final-artifact-privacy.json"),
            serde_json::to_vec_pretty(&scan)?,
        )?;
        save_private_fingerprints(tier, export_fingerprints)?;
        *stage = "complete";
        Ok(())
    };
    let mut active_fixture = None;
    let mut result = (|| -> Result<()> {
        let iso = PathBuf::from(
            std::env::var_os("ILEX_ISO_NEXT").context("ILEX_ISO_NEXT required: the GNOME ISO")?,
        );
        ensure!(iso.is_file(), "exact installer ISO missing");
        stage = "fixture";
        let image = common::image()?;
        let digest_of = |reference: &str| -> Result<String> {
            reference
                .strip_prefix("ghcr.io/reidond/ilex@")
                .filter(|digest| lifecycle::valid_digest(digest))
                .map(str::to_owned)
                .with_context(|| {
                    format!("the encrypted gates need a signed ghcr.io/reidond/ilex@sha256:… reference, not {reference}")
                })
        };
        let (mut expected, registry, mut update, testing_tag) = if lifecycle {
            let v1 = std::env::var("ILEX_ENCRYPTED_UPGRADE_FROM")
                .context("set ILEX_ENCRYPTED_UPGRADE_FROM to the qualifying rollback image")?;
            let tag = std::env::var("ILEX_ENCRYPTED_UPGRADE_TAG").context(
                "set ILEX_ENCRYPTED_UPGRADE_TAG to the registry tag of the rollback image",
            )?;
            let digest = digest_of(&v1)?;
            let registry = registry(&ws, &work)?;
            let mut update = registry.push(&image, "update")?;
            let inputs = lifecycle::record_inputs(&artifacts, &v1, &image)?;
            update["boot_payload"] = inputs["update"]["boot_payload"].clone();
            update["fixture_kernel"] = inputs["update"]["fixture_kernel"].clone();
            let expected = json!({
                "source": v1, "digest": digest, "published_source": true,
                "boot_payload": inputs["baseline"]["boot_payload"],
            });
            (expected, Some(registry), Some(update), Some(tag))
        } else {
            let digest = digest_of(&image)?;
            let expected = json!({"source": image, "digest": digest, "published_source": true});
            (expected, None, None, None)
        };
        expected["preserve_data"] = json!(false);
        expected["interrupt_completion"] = json!(false);
        expected["local_diagnostic"] = json!(false);
        expected["layout_refusal"] = json!(false);
        expected["tpm_slot_budget"] = json!(false);
        fs::write(
            artifacts.join("candidate.json"),
            serde_json::to_vec_pretty(&expected)?,
        )?;
        let passphrase = random_secret()?;
        let maintenance = random_secret()?;
        stage = "installation";
        let installed = install_with_helper(
            &ws,
            &work,
            &artifacts,
            &passphrase,
            &maintenance,
            testing_tag.as_deref(),
            lifecycle,
        );
        let Installed {
            fixture,
            recovery,
            probe: installer_probe,
            tokens: original_tokens,
            digest,
        } = installed?;
        let fixture = active_fixture.insert(fixture);
        stage = "installer_evidence";
        ensure!(
            digest == expected["digest"].as_str().unwrap_or_default(),
            "the installer pinned {digest}, not the image under test"
        );
        // The credentials of this run, as the evidence seal wants them.
        let mut installed_secrets = vec![&passphrase, &recovery, &maintenance];
        credentials = fingerprints(&installed_secrets);
        fs::write(
            artifacts.join("installer-credentials.json"),
            serde_json::to_vec_pretty(&credentials)?,
        )?;
        let mut export_fingerprints: Vec<_> = installed_secrets
            .drain(..)
            .map(private_fingerprint)
            .collect();
        ensure!(
            installer_probe["verdict"] == "eligible",
            "installer probe unavailable"
        );
        fs::write(
            artifacts.join("installer-probe.json"),
            serde_json::to_vec_pretty(&installer_probe)?,
        )?;
        fs::write(
            artifacts.join("installer-tokens.json"),
            serde_json::to_vec_pretty(&original_tokens)?,
        )?;
        let disk = match &fixture.spec.boot {
            ilex_devkit::core::BootSource::Disk { path, .. } => path.clone(),
            other => bail!("the installed VM boots from {other:?}"),
        };
        if let Some(update) = update.take() {
            stage = "encrypted_lifecycle";
            let home_password = lifecycle::home_password()?;
            credentials = fingerprints(&[&passphrase, &recovery, &maintenance, &home_password]);
            export_fingerprints.push(private_fingerprint(&home_password));
            fs::write(
                artifacts.join("home-credentials.json"),
                serde_json::to_vec_pretty(&fingerprints(&[&home_password]))?,
            )?;
            lifecycle::qualify(
                &ws,
                fixture,
                &artifacts,
                registry.as_ref().context("lifecycle registry missing")?,
                &expected,
                &update,
                &installer_probe,
                &original_tokens,
                &recovery,
                &home_password,
                &credentials,
                &mut stage,
            )?;
            return finish(
                fixture,
                &iso,
                &credentials,
                &export_fingerprints,
                &mut stage,
            );
        }
        stage = "pcr_mismatch_first_boot";
        fallback(
            &ws,
            &disk,
            &fixture.spec,
            &work,
            &artifacts,
            &passphrase,
            &credentials,
            &installer_probe,
            &original_tokens,
            &expected,
            "pcr-mismatch",
            None,
        )?;
        stage = "recovery_key_boot";
        fallback(
            &ws,
            &disk,
            &fixture.spec,
            &work,
            &artifacts,
            &recovery,
            &credentials,
            &installer_probe,
            &original_tokens,
            &expected,
            "recovery",
            Some(&passphrase),
        )?;
        stage = "first_installed_boot";
        fixture.disk_boot()?;
        let booted = assertions(fixture, &artifacts, &expected, &installer_probe);
        fixture.logs(&artifacts, "installed")?;
        booted?;
        guest_privacy(
            fixture.vm.running_mut()?,
            &artifacts,
            "installed-after-finalisation",
            &credentials,
        )?;
        stage = "cold_restart";
        poweroff(fixture, &artifacts, "installed")?;
        fixture.disk_boot()?;
        let cold = artifacts.join("cold");
        fs::create_dir_all(&cold)?;
        assertions(fixture, &cold, &expected, &installer_probe)?;
        guest_privacy(fixture.vm.running_mut()?, &cold, "cold", &credentials)?;
        fixture.logs(&artifacts, "cold")?;
        poweroff(fixture, &artifacts, "cold")?;
        stage = "same_tpm_policy_mismatch";
        same_tpm_mismatch(
            &ws,
            fixture,
            &artifacts,
            &passphrase,
            &credentials,
            &installer_probe,
            &original_tokens,
        )?;
        stage = "no_tpm_fallback";
        fallback(
            &ws,
            &disk,
            &fixture.spec,
            &work,
            &artifacts,
            &passphrase,
            &credentials,
            &installer_probe,
            &original_tokens,
            &expected,
            "no-tpm",
            None,
        )?;
        finish(
            fixture,
            &iso,
            &credentials,
            &export_fingerprints,
            &mut stage,
        )
    })();
    if lifecycle
        && result.is_err()
        && let Some(fixture) = active_fixture.as_mut()
    {
        let _ = fixture.logs(&artifacts, "lifecycle-failure");
    }
    let guest_failure = result.as_ref().err().map(|error| {
        error
            .downcast_ref::<GuestFailure>()
            .cloned()
            .unwrap_or_else(|| GuestFailure::capture(active_fixture.as_mut(), stage))
    });
    drop(active_fixture);
    if let Err(error) = &result {
        let _ = fs::write(
            published.join("failure-code.json"),
            serde_json::to_vec_pretty(&failure_code(error))?,
        );
        let _ = fs::write(
            artifacts.join("host-fixture-error.txt"),
            format!("{error:#}\n"),
        );
    }
    if credentials.is_array() && result.is_err() {
        let diagnostic_text = artifacts.join("diagnostics");
        let target = published.join("diagnostics");
        let capture = (|| {
            diagnostics(&artifacts, &diagnostic_text, &credentials)?;
            if !evidence_files(&diagnostic_text)?.is_empty() {
                copy_evidence(&diagnostic_text, &target)?;
                artifact_privacy(&[&target], &credentials)?;
            }
            Ok::<(), anyhow::Error>(())
        })();
        if capture.is_err() {
            for directory in [&diagnostic_text, &target] {
                if directory.exists() {
                    clear_evidence(directory)?;
                }
            }
            let _ = fs::write(
                published.join("diagnostics-unavailable.json"),
                b"{\"reason\":\"capture or privacy verification failed\"}\n",
            );
        }
    }
    if result.is_ok() {
        let export = (|| -> Result<()> {
            copy_evidence(&artifacts, &published)?;
            // Scan the actual copied/exported set too, including other CI
            // artifact tiers. The workflow repeats this gate after all cases.
            let root = std::env::var_os("ILEX_ARTIFACTS")
                .map(PathBuf::from)
                .unwrap_or_else(|| published.clone());
            artifact_privacy(&[&root], &credentials)?;
            Ok(())
        })();
        if let Err(error) = export {
            clear_evidence(&published)?;
            stage = "evidence_export";
            result = Err(error.context("publishing privacy-checked evidence"));
        }
    }
    // Credentials, signing key, disk and unverified diagnostics are never
    // artifacts. Cleanup failure must not leave a passing result behind.
    if let Err(error) = fs::remove_dir_all(&work) {
        stage = "cleanup";
        result = Err(error.into());
    }
    let failure_summary = result.as_ref().err().map(|error| {
        let guest = guest_failure.unwrap_or_else(|| GuestFailure::capture(None, stage));
        json!({
            "case": tier, "stage": stage, "started_unix_seconds": started_unix_seconds,
            "guest": guest, "diagnostic": failure_code(error),
        })
    });
    fs::write(
        published.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"qualification": if result.is_ok() {"passed"} else {"unqualified"}, "stage":stage, "elapsed_seconds":start.elapsed().as_secs_f64(), "error":result.as_ref().err().map(|_|"private failure details suppressed"), "failure_summary":failure_summary, "storage":"automatic-empty-disk", "scope":"exact live installer ISO, published signed image, installed boot; physical hardware remains unqualified"}),
        )?,
    )?;
    result.map_err(|_| {
        let summary = failure_summary.unwrap_or(Value::Null);
        anyhow::anyhow!(
            "encrypted qualification failed at {stage}; private diagnostics suppressed\n{summary}"
        )
    })
}
