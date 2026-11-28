#!/bin/bash
# Candidate security qualification on this runner, never inferred from WSL.
set -euo pipefail
# A failing step is named in the job log. The nested self-check scripts report
# their own logs; this script's steps print straight to the job log.
source "$(dirname "${BASH_SOURCE[0]}")/diagnose.sh"
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
        diag_fail 'swtpm AppArmor profile is still loaded; security tier unqualified'
    fi
fi
timeout --signal=TERM --kill-after=10s 5m cargo ilexctl disk --self-check > "$ILEX_ARTIFACTS/security/disk-alias-self-check.json"
cargo ilexctl doctor security > "$ILEX_ARTIFACTS/security/runner-probe.json"
timeout --signal=TERM --kill-after=10s 12m bash .github/security-startup-self-check.sh
# KVM_CHECK_EXTENSION(KVM_CAP_X86_SMM), not a virtualized-host heuristic.
# Secure guests require KVM with SMM; there is no software-emulation route.
if ! python3 - "$ILEX_ARTIFACTS/security/runner-probe.json" <<'PY'
import json,sys
probe=json.load(open(sys.argv[1]))
raise SystemExit(0 if (probe['kvm_smm'] or 0) > 0 else 1)
PY
then
    echo '::error::runner lacks KVM with SMM; secure guests cannot be qualified'
    exit 1
fi
echo "security accelerator: **kvm** (runner probe retained); ${ILEX_SECURITY_MEMORY:-4G}, 2 vCPUs; boot deadline 420s; each fault/qualification run deadline=45m." >> "$GITHUB_STEP_SUMMARY"
# Real guest boots precede an injected EFI-control startup failure. Run this
# before normal qualification: the expected failure invalidates its profile.
bash .github/security-control-startup-self-check.sh
timeout --signal=TERM --kill-after=20s 45m cargo ilexctl doctor security --self-check --accelerator kvm --memory "${ILEX_SECURITY_MEMORY:-4G}"
python3 - "$ILEX_ARTIFACTS/security/result.json" <<'PY'
import json,sys
assert json.load(open(sys.argv[1]))['qualification'] == 'passed', 'missing guest qualification'
PY
qualified=true
echo "security::firmware: **qualified**, kvm, $((SECONDS-started)) seconds; candidate $ILEX_IMAGE." >> "$GITHUB_STEP_SUMMARY"
