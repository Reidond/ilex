//! Part-4 consumers of T6's contract. Guest assertions use VmControl so a
//! future Hyper-V provider can reuse them without QMP or a Linux host path.
use anyhow::{Context, Result, ensure};
use ilex_devkit::{config::Selection, core::VmControl, security};
use serde_json::Value;
use std::{fs, path::Path};

fn exec(control: &impl VmControl, script: &str) -> Result<Vec<u8>> {
    // VmControl returns successful stdout. Preserve both diagnostic streams
    // and the exit code even when an assertion fails inside the guest.
    let wrapper = r#"import json, subprocess, sys
try:
    p = subprocess.run(['sh', '-eu', '-c', sys.argv[1]], capture_output=True, timeout=60)
    print(json.dumps({'code': p.returncode, 'stdout': p.stdout.decode(errors='replace'), 'stderr': p.stderr.decode(errors='replace')}))
except subprocess.TimeoutExpired:
    print(json.dumps({'code': 124, 'stdout': '', 'stderr': 'guest probe exceeded 60 seconds'}))
"#;
    let bytes = control.guest_exec(
        &[
            "timeout",
            "--kill-after=5s",
            "65s",
            "python3",
            "-I",
            "-S",
            "-c",
            wrapper,
            script,
        ]
        .map(str::to_owned),
    )?;
    let output: Value = serde_json::from_slice(&bytes)?;
    ensure!(output["code"] == 0, "guest probe failed: {output}");
    Ok(output["stdout"]
        .as_str()
        .context("guest probe stdout missing")?
        .as_bytes()
        .to_vec())
}

fn capture(control: &impl VmControl, artifacts: &Path, name: &str, script: &str) -> Result<Value> {
    let bytes = exec(control, script)?;
    fs::write(artifacts.join(name), &bytes)?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {name}"))
}

pub fn guest(
    control: &impl VmControl,
    artifacts: &Path,
    label: &str,
    digest: &str,
    expected: &Selection,
) -> Result<()> {
    // Retain observations before asserting: a bad guest must be diagnosable.
    let state = capture(
        control,
        artifacts,
        &format!("{label}-state.json"),
        r#"python3 -I -S - <<'PY'
import glob, json, pathlib, stat
def read(path):
    try:
        return pathlib.Path(path).read_text().strip()
    except OSError as error:
        return {'error': str(error)}
def efi(name):
    paths = glob.glob('/sys/firmware/efi/efivars/' + name + '-8be4df61-93ca-11d2-aa0d-00e098032b8c')
    if len(paths) != 1:
        return {'error': 'expected one EFI global variable'}
    data = pathlib.Path(paths[0]).read_bytes()
    return {'hex': data.hex(), 'value': data[4] if len(data) == 5 else None}
try:
    tpmrm = stat.S_ISCHR(pathlib.Path('/dev/tpmrm0').stat().st_mode)
except OSError:
    tpmrm = False
print(json.dumps({
    'secure_boot': efi('SecureBoot'), 'setup_mode': efi('SetupMode'),
    'tpm_version': read('/sys/class/tpm/tpm0/tpm_version_major'),
    'tpm_resource_manager': tpmrm,
    'pcr7_sha256': read('/sys/class/tpm/tpm0/pcr-sha256/7'),
    'lockdown': read('/sys/kernel/security/lockdown'),
    'firmware_vendor': read('/sys/class/dmi/id/bios_vendor'),
    'firmware_version': read('/sys/class/dmi/id/bios_version'),
    'firmware_date': read('/sys/class/dmi/id/bios_date'),
    'kernel': read('/proc/sys/kernel/osrelease'),
    'boot_id': read('/proc/sys/kernel/random/boot_id'),
    'command_line': read('/proc/cmdline')
}, indent=2))
PY"#,
    )?;
    ensure!(
        state["secure_boot"]["value"] == 1,
        "guest SecureBoot != 1: {state}"
    );
    ensure!(
        state["setup_mode"]["value"] == 0,
        "guest SetupMode != 0: {state}"
    );
    ensure!(
        state["tpm_version"] == "2" && state["tpm_resource_manager"] == true,
        "guest TPM2/resource manager missing: {state}"
    );
    let pcr = state["pcr7_sha256"].as_str().unwrap_or_default();
    ensure!(
        pcr.len() == 64
            && pcr.bytes().all(|b| b.is_ascii_hexdigit())
            && pcr.bytes().any(|b| b != b'0'),
        "guest SHA-256 PCR7 is unavailable or unmeasured: {state}"
    );
    let lockdown = state["lockdown"].as_str().unwrap_or_default();
    ensure!(
        lockdown.contains("[integrity]") || lockdown.contains("[confidentiality]"),
        "guest kernel lockdown is disabled: {state}"
    );

    let bootc = capture(
        control,
        artifacts,
        &format!("{label}-bootc.json"),
        "bootc status --json",
    )?;
    let booted = &bootc["status"]["booted"]["image"];
    ensure!(
        booted["imageDigest"] == digest,
        "guest booted digest {} differs from candidate {digest}; ILEX_DISK must contain the selected image",
        booted["imageDigest"]
    );
    let config = capture(
        control,
        artifacts,
        &format!("{label}-ilex.json"),
        "ilex-agent config --json",
    )?;
    ensure!(
        config["image"]["schema"] == 1 && config["machine"]["schema"] == 1,
        "guest lacks ilex configuration schema 1"
    );
    for (key, default_key, names, default) in [
        (
            "shells",
            "default-shell",
            &expected.shells,
            &expected.default_shell,
        ),
        (
            "design-systems",
            "default-design-system",
            &expected.design_systems,
            &expected.default_design_system,
        ),
    ] {
        let actual = config["image"][key]
            .as_array()
            .context("guest configuration list missing")?;
        ensure!(
            actual
                .iter()
                .map(|v| v["name"].as_str())
                .collect::<Vec<_>>()
                == names.iter().map(|s| Some(s.as_str())).collect::<Vec<_>>(),
            "guest {key} differs from candidate labels"
        );
        ensure!(
            config["image"][default_key] == serde_json::to_value(default)?,
            "guest default {key} differs from candidate labels"
        );
    }
    ensure!(
        config["machine"]["name"] == expected.machine,
        "guest machine differs from candidate labels"
    );
    let log = control.guest_exec(
        &[
            "timeout",
            "30s",
            "cat",
            "/sys/kernel/security/tpm0/binary_bios_measurements",
        ]
        .map(str::to_owned),
    )?;
    fs::write(artifacts.join(format!("{label}-eventlog.bin")), &log)?;
    let replay = security::replay_pcr7(&log)?;
    fs::write(
        artifacts.join(format!("{label}-pcr7.json")),
        serde_json::to_vec_pretty(&replay)?,
    )?;
    ensure!(
        pcr.eq_ignore_ascii_case(&replay.sha256),
        "guest PCR7 {pcr} differs from event-log replay {}",
        replay.sha256
    );
    println!(
        "security {label}: ilex {digest}; SB=1 SetupMode=0 TPM2 lockdown; PCR7={pcr}; {} replayed events",
        replay.events
    );
    Ok(())
}

/// No passthrough is needed to prove that Fedora accepts its in-tree amdgpu
/// module under lockdown. Actual GPU rendering and physical firmware stay a
/// separate ms-7c35 qualification.
pub fn amdgpu(control: &impl VmControl, artifacts: &Path) -> Result<()> {
    let script = r#"
kernel=$(uname -r)
printf 'kernel=%s\n' "$kernel"
rpm -q kernel-core kernel-modules-core amd-gpu-firmware
for field in filename intree signer sig_id vermagic; do
    printf '%s: ' "$field"
    modinfo -F "$field" amdgpu
done
test "$(modinfo -F intree amdgpu)" = Y
test -n "$(modinfo -F signer amdgpu)"
# New GPUs use amdgpu's wildcard display-class aliases and IP discovery.
# Ask kmod to match a VGA modalias rather than requiring a literal device ID.
modalias=pci:v00001002d00007550sv00000000sd00000000bc03sc00i00
printf 'RX 9070 XT display modalias: %s\n' "$modalias"
modprobe --resolve-alias "$modalias" | grep -Fx amdgpu
rpm -qf "$(modinfo -n amdgpu)"
# kmod-libs/devel are Fedora's module-loader libraries, not external drivers.
if rpm -qa --qf '%{NAME}\n' | grep -Eiv '^kmod-(libs|devel)$' | grep -Ei '(^akmod|^kmod-|nvidia.*(kmod|driver))'; then
    echo 'out-of-tree module package requires a separate signing review' >&2
    exit 1
fi
for dir in extra updates weak-updates; do
    if test -d "/usr/lib/modules/$kernel/$dir"; then
        test -z "$(find "/usr/lib/modules/$kernel/$dir" -type f -o -type l)"
    fi
done
modprobe amdgpu
test -d /sys/module/amdgpu
taint=$(cat /proc/sys/kernel/tainted)
test "$((taint & 8192))" -eq 0
cat /sys/kernel/security/lockdown
echo 'Fedora in-tree amdgpu loaded under lockdown; no unsigned-module taint'
"#;
    let output = exec(control, script);
    // Save successful diagnostics; guest_exec retains stderr on a failure.
    match output {
        Ok(bytes) => fs::write(artifacts.join("amdgpu.txt"), bytes)?,
        Err(error) => {
            fs::write(artifacts.join("amdgpu.txt"), format!("{error:#}"))?;
            return Err(error.context("amdgpu image-level lockdown check"));
        }
    }
    Ok(())
}
