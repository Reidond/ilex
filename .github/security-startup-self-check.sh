#!/bin/bash
# Exercise the real security --self-check failure path before any guest boots.
set -euo pipefail
: "${ILEX_IMAGE:?select the test image for this checkout}"
: "${ILEX_ARTIFACTS:?select the public evidence directory}"
evidence="$ILEX_ARTIFACTS/security/startup-self-check"
mkdir -p "$evidence"
# Keep the private runtime short enough for a 48-character checkout slug.
fixture=$(mktemp -d /tmp/i6.XXXXXX)
touch "$fixture/.t6-fixture"
cleanup() {
    if [[ "$fixture" == /tmp/i6.* && -f "$fixture/.t6-fixture" ]]; then
        rm -rf -- "$fixture"
    fi
}
trap cleanup EXIT
export ILEX_CHECK_SWTPM ILEX_CHECK_SETUP ILEX_CHECK_QEMU
ILEX_CHECK_SWTPM=$(command -v swtpm)
ILEX_CHECK_SETUP=$(command -v swtpm_setup)
ILEX_CHECK_QEMU=$(command -v qemu-system-x86_64)
mkdir "$fixture/bin"
qemu-img create -q -f qcow2 "$fixture/input.qcow2" 128M
cat > "$fixture/bin/swtpm" <<'SH'
#!/bin/bash
set -eu
case "$*" in
    *"/ilex-vm/"*"/tpm.sock,"*)
        echo 'ILEX-INJECTED-SWTPM-STDOUT'
        echo 'ILEX-INJECTED-SWTPM-STDERR: refused before socket creation' >&2
        exit 73 ;;
    *) exec "$ILEX_CHECK_SWTPM" "$@" ;;
esac
SH
cat > "$fixture/bin/swtpm_setup" <<'SH'
#!/bin/bash
set -eu
if [[ "$ILEX_CHECK_PHASE" == setup && "${1:-}" != --version ]]; then
    echo 'ILEX-INJECTED-SETUP-STDOUT'
    echo 'ILEX-INJECTED-SETUP-STDERR: manufacturing refused' >&2
    exit 74
fi
exec "$ILEX_CHECK_SETUP" "$@"
SH
cat > "$fixture/bin/qemu-system-x86_64" <<'SH'
#!/bin/bash
set -eu
if [[ "${1:-}" == --version ]]; then exec "$ILEX_CHECK_QEMU" "$@"; fi
echo 'Unexpected QEMU launch' > "$ILEX_CHECK_LAUNCHED"
exit 75
SH
chmod 700 "$fixture/bin/"*
for phase in emulator setup; do
    mkdir -p "$evidence/$phase"
    if env -u CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER \
        PATH="$fixture/bin:$PATH" ILEX_STATE_DIR="$fixture/state-$phase" \
        XDG_RUNTIME_DIR="$fixture" ILEX_DISK="$fixture/input.qcow2" \
        ILEX_SECURITY_MEMORY=128M ILEX_CHECK_PHASE="$phase" \
        ILEX_CHECK_LAUNCHED="$fixture/qemu-launched" ILEX_ARTIFACTS="$evidence/$phase" \
        timeout --signal=TERM --kill-after=10s 5m cargo ilexctl doctor security --self-check --accelerator kvm \
        > "$evidence/$phase/command.log" 2>&1; then
        echo "injected $phase startup failure unexpectedly qualified" >&2
        exit 1
    fi
    test ! -e "$fixture/qemu-launched"
    python3 - "$evidence/$phase/security" "$phase" <<'PY'
import json, pathlib, sys
root, phase = pathlib.Path(sys.argv[1]), sys.argv[2]
assert json.loads((root/'result.json').read_text())['qualification'] == 'unqualified'
name = 'swtpm.log' if phase == 'emulator' else 'swtpm-setup.log'
text = (root/('failed-start-'+name)).read_text()
prefix = 'SWTPM' if phase == 'emulator' else 'SETUP'
for stream in ['STDOUT', 'STDERR']:
    assert 'ILEX-INJECTED-'+prefix+'-'+stream in text, (phase, stream, text)
if phase == 'emulator':
    setup = (root/'failed-start-swtpm-setup.log').read_text()
    assert 'Successfully activated PCR banks sha256' in setup, setup
    assert 'Successfully authored TPM state' in setup, setup
print(phase+': expected startup failure; diagnostic stdout/stderr retained before cleanup')
PY
done
printf '{"result":"passed","emulator_exit_before_socket":true,"setup_exit":true,"logs_retained":true,"real_sha256_manufacture_before_emulator_failure":true,"qemu_launched":false}\n' \
    | tee "$evidence/result.json"
