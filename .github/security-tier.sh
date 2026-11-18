#!/bin/bash
# Candidate security qualification on this runner, never inferred from WSL.
set -euo pipefail
export ILEX_ARTIFACTS="${ILEX_ARTIFACTS:-$RUNNER_TEMP/security-artifacts}"
mkdir -p "$ILEX_ARTIFACTS/security"
started=$SECONDS
qualified=false
finish() {
    result=$?
    if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
        # Collect the host denial, too, when an inherited confinement rule
        # prevents a child from writing a useful application log.
        { sudo -n dmesg | grep -iE 'apparmor.*(swtpm|ilex)|swtpm.*denied' || true; } \
            > "$ILEX_ARTIFACTS/security/apparmor-audit.log" 2>&1
    fi
    if [[ "$qualified" != true ]]; then
        echo '::error::security::firmware UNQUALIFIED (failure, timeout or missing evidence)'
        echo 'security::firmware: **UNQUALIFIED**; see security evidence.' >> "$GITHUB_STEP_SUMMARY"
        printf '{"qualification":"unqualified","exit_code":%s}\n' "$result" > "$ILEX_ARTIFACTS/security/ci-result.json"
    fi
    return "$result"
}
trap finish EXIT
# Ubuntu's swtpm profile permits libvirt sockets, not this checkout's private
# ilex-vm runtime paths. Only this disposable Actions runner unloads that one
# executable's profile; guest Secure Boot, manufactured TPM and banks stay
# mandatory. Local hosts and all other AppArmor profiles are unchanged.
if [[ "${GITHUB_ACTIONS:-}" == true && -f /etc/apparmor.d/usr.bin.swtpm ]]; then
    cp /etc/apparmor.d/usr.bin.swtpm "$ILEX_ARTIFACTS/security/usr.bin.swtpm"
    sudo -n cat /sys/kernel/security/apparmor/profiles > "$ILEX_ARTIFACTS/security/apparmor-before.txt"
    if grep -qE '^(swtpm|/usr/bin/swtpm) ' "$ILEX_ARTIFACTS/security/apparmor-before.txt"; then
        echo 'CI: unloading the swtpm AppArmor profile for private ilex-vm sockets'
        sudo -n apparmor_parser -R /etc/apparmor.d/usr.bin.swtpm
    fi
    sudo -n cat /sys/kernel/security/apparmor/profiles > "$ILEX_ARTIFACTS/security/apparmor-after.txt"
    if grep -qE '^(swtpm|/usr/bin/swtpm) ' "$ILEX_ARTIFACTS/security/apparmor-after.txt"; then
        echo 'swtpm AppArmor profile is still loaded; security tier unqualified' >&2
        exit 1
    fi
fi
timeout --signal=TERM --kill-after=10s 5m cargo xtask disk --self-check > "$ILEX_ARTIFACTS/security/disk-alias-self-check.json"
cargo xtask security > "$ILEX_ARTIFACTS/security/runner-probe.json"
timeout --signal=TERM --kill-after=10s 12m bash .github/security-startup-self-check.sh
# KVM_CHECK_EXTENSION(KVM_CAP_X86_SMM), not a virtualized-host heuristic.
requested=${ILEX_SECURITY_ACCELERATOR:-probe}
case "$requested" in
probe)
accel=$(python3 - "$ILEX_ARTIFACTS/security/runner-probe.json" <<'PY'
import json,sys
probe=json.load(open(sys.argv[1]))
print('kvm' if (probe['kvm_smm'] or 0) > 0 else 'tcg')
PY
)
;;
kvm|tcg) accel=$requested ;;
*) echo "invalid security accelerator: $requested" >&2; exit 1 ;;
esac
echo "security: explicitly selecting $accel (request=$requested; runner probe retained); no silent QEMU fallback"
printf '%s\n' "$accel" > "$ILEX_ARTIFACTS/security/selected-accelerator"
echo "security accelerator: **$accel** (runner probe retained); ${ILEX_SECURITY_MEMORY:-4G}, 2 vCPUs; boot deadline KVM=420s / TCG=900s; each fault/qualification run deadline=45m." >> "$GITHUB_STEP_SUMMARY"
# Real guest boots precede an injected EFI-control startup failure. Run this
# before normal qualification: the expected failure invalidates its profile.
ILEX_SECURITY_ACCELERATOR="$accel" bash .github/security-control-startup-self-check.sh
timeout --signal=TERM --kill-after=20s 45m cargo xtask security --self-check --accelerator "$accel" --memory "${ILEX_SECURITY_MEMORY:-4G}"
python3 - "$ILEX_ARTIFACTS/security/result.json" <<'PY'
import json,sys
assert json.load(open(sys.argv[1]))['qualification'] == 'passed', 'missing guest qualification'
PY
qualified=true
echo "security::firmware: **qualified**, $accel, $((SECONDS-started)) seconds; candidate $ILEX_IMAGE." >> "$GITHUB_STEP_SUMMARY"
