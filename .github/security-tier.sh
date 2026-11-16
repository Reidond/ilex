#!/bin/bash
# Candidate security qualification on this runner, never inferred from WSL.
set -euo pipefail
export ILEX_ARTIFACTS="${ILEX_ARTIFACTS:-$RUNNER_TEMP/security-artifacts}"
mkdir -p "$ILEX_ARTIFACTS/security"
started=$SECONDS
qualified=false
finish() {
    result=$?
    if [[ "$qualified" != true ]]; then
        echo '::error::security::firmware UNQUALIFIED (failure, timeout or missing evidence)'
        echo 'security::firmware: **UNQUALIFIED**; see security evidence.' >> "$GITHUB_STEP_SUMMARY"
        printf '{"qualification":"unqualified","exit_code":%s}\n' "$result" > "$ILEX_ARTIFACTS/security/ci-result.json"
    fi
    return "$result"
}
trap finish EXIT
timeout --signal=TERM --kill-after=10s 5m cargo xtask disk --self-check > "$ILEX_ARTIFACTS/security/disk-alias-self-check.json"
cargo xtask security > "$ILEX_ARTIFACTS/security/runner-probe.json"
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
echo "security accelerator: **$accel** (runner probe retained); 4 GiB, 2 vCPUs; boot deadline KVM=420s / TCG=900s; total deadline=45m." >> "$GITHUB_STEP_SUMMARY"
timeout --signal=TERM --kill-after=20s 45m cargo xtask security --self-check --accelerator "$accel"
python3 - "$ILEX_ARTIFACTS/security/result.json" <<'PY'
import json,sys
assert json.load(open(sys.argv[1]))['qualification'] == 'passed', 'missing guest qualification'
PY
qualified=true
echo "security::firmware: **qualified**, $accel, $((SECONDS-started)) seconds; candidate $ILEX_IMAGE." >> "$GITHUB_STEP_SUMMARY"
