#!/bin/bash
# Boot real ilex first, then fail only one unsigned EFI control before QMP.
set -euo pipefail
source "$(dirname "${BASH_SOURCE[0]}")/diagnose.sh"
: "${ILEX_IMAGE:?select the test image for this checkout}"
: "${ILEX_ARTIFACTS:?select the public evidence directory}"
control=${1:-off}
case "$control" in off|on) ;; *) echo 'control must be off or on' >&2; exit 1 ;; esac
evidence="$ILEX_ARTIFACTS/security/control-startup-self-check/$control"
mkdir -p "$evidence"
# A fresh artifact generation cannot inherit an earlier control's evidence.
output=$(mktemp -d "$evidence/run.XXXXXX")
diag_logs "$output/command.log" "$output/security/result.json"
fixture=$(mktemp -d /tmp/i17.XXXXXX)
touch "$fixture/.pr17-fixture"
cleanup() {
    if [[ "$fixture" == /tmp/i17.* && -f "$fixture/.pr17-fixture" ]]; then
        rm -rf -- "$fixture"
    fi
}
trap cleanup EXIT
export ILEX_CHECK_QEMU ILEX_CHECK_CONTROL ILEX_CHECK_EFI_LAUNCH
ILEX_CHECK_QEMU=$(command -v qemu-system-x86_64)
ILEX_CHECK_CONTROL=$control
ILEX_CHECK_EFI_LAUNCH="$fixture/efi-launch"
mkdir "$fixture/bin"
cat > "$fixture/bin/qemu-system-x86_64" <<'SH'
#!/bin/bash
set -eu
name=
previous=
for arg; do
    if [[ "$previous" == -name ]]; then name=$arg; break; fi
    previous=$arg
done
if [[ "$name" == *-security-efi-"$ILEX_CHECK_CONTROL"-* ]]; then
    printf '%s\n' "$name" > "$ILEX_CHECK_EFI_LAUNCH"
    echo 'ILEX-INJECTED-EFI-QEMU-STDOUT'
    echo 'ILEX-INJECTED-EFI-QEMU-STDERR: refused before QMP creation' >&2
    exit 76
fi
# Version probes and every normal ilex boot use the real executable unchanged.
exec "$ILEX_CHECK_QEMU" "$@"
SH
chmod 700 "$fixture/bin/qemu-system-x86_64"
if env -u CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER \
    PATH="$fixture/bin:$PATH" ILEX_ARTIFACTS="$output" \
    timeout --signal=TERM --kill-after=20s 45m \
    cargo ilexctl doctor security --self-check --accelerator kvm \
    > "$output/command.log" 2>&1; then
    diag_fail 'the injected unsigned-control startup failure unexpectedly qualified'
fi
# The nested run must fail only at the injected EFI-control launch. Failing
# earlier (its disk, a platform or guest stage) never reaches the wrapper.
if [[ ! -s "$ILEX_CHECK_EFI_LAUNCH" ]]; then
    diag_fail "the nested security::firmware run ended before the injected EFI-control QEMU launch; its output follows"
fi
python3 - "$output/security" "$control" <<'PY'
import json, pathlib, sys
root, control = pathlib.Path(sys.argv[1]), sys.argv[2]
read = lambda name: json.loads((root/name).read_text())
result = read('result.json')
assert result['qualification'] == 'unqualified', result
assert result['disposition'] == 'platform_unsupported_or_unqualified', result
assert result['stage'] == 'platform', result
assert 'exit status: 76' in result['error'], result
log = (root/f'unsigned-{control}-failed-start-qemu.log').read_text()
for stream in ['STDOUT', 'STDERR']:
    assert 'ILEX-INJECTED-EFI-QEMU-'+stream in log, log
assert not (root/f'unsigned-{control}.json').exists(), 'failed control reported success'
if control == 'on':
    assert read('unsigned-off.json')['evidence'] == 'serial execution marker'
# The fault must occur after actual ilex first/warm/cold boots and TPM decrypt.
digest = read('image.json')['Digest']
for boot in ['first', 'warm', 'cold']:
    state, replay = read(boot+'-state.json'), read(boot+'-pcr7.json')
    assert state['secure_boot']['value'] == 1 and state['setup_mode']['value'] == 0
    assert state['tpm_version'] == '2' and state['tpm_resource_manager']
    assert replay['sha256'] == state['pcr7_sha256'].lower()
    assert replay['policy_events'] > 0 and replay['authority_events'] > 0
    assert read(boot+'-bootc.json')['status']['booted']['image']['imageDigest'] == digest
assert read('timing.json')['tpm_decrypt_after_warm_and_cold'] is True
assert 'loaded under lockdown; no unsigned-module taint' in (root/'amdgpu.txt').read_text()
caps = read('failed-profile.json')
assert caps['qualification']['outcome']['status'] == 'failed', caps
assert caps['features']['firmware_enforcement']['status'] != 'available', caps
assert caps['features']['persistent_tpm']['status'] != 'available', caps
summary = {'result': 'passed', 'control': control, 'tier_qualification': 'unqualified',
           'disposition': result['disposition'], 'real_ilex_restart_checks': True,
           'startup_logs_retained': True, 'evidence': str(root)}
(root.parent/'self-check.json').write_text(json.dumps(summary, indent=2)+'\n')
print(json.dumps(summary))
PY
