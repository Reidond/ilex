#!/bin/bash
# Run on the selected Linux qualification runner, after firmware qualification.
# Artifacts remain private until the calling job seals the export.
set -euo pipefail
: "${ILEX_IMAGE:?candidate (v2) is required}"
: "${ILEX_ISO:?exact installer ISO is required}"
: "${ILEX_ARTIFACTS:?private evidence root is required}"
: "${ILEX_PRIVATE_FINGERPRINTS:?private fingerprint directory is required}"
tier=encrypted-upgrade-rollback
mkdir -p "$ILEX_ARTIFACTS/$tier"
qualified=false
finish() {
    result=$?
    if [[ "$qualified" != true ]]; then
        rm -f -- "$ILEX_PRIVATE_FINGERPRINTS/$tier.json"
        printf '{"qualification":"unqualified","exit_code":%s}\n' "$result" > "$ILEX_ARTIFACTS/$tier/ci-result.json"
        echo 'Encrypted upgrade/rollback **UNQUALIFIED**. A compatible signed baseline with a different kernel and a runner that completes the secure lifecycle within the job budget are required.' >> "$GITHUB_STEP_SUMMARY"
    fi
}
trap finish EXIT
if [[ ! "${ILEX_ENCRYPTED_UPGRADE_FROM:-}" =~ ^ghcr\.io/reidond/ilex@sha256:[0-9a-f]{64}$ ]]; then
    echo '::error::Set ILEX_ENCRYPTED_UPGRADE_FROM to the supported signed rollback image, pinned by digest.'
    exit 1
fi
cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true "$ILEX_ENCRYPTED_UPGRADE_FROM" >/dev/null
podman pull --quiet "$ILEX_ENCRYPTED_UPGRADE_FROM"
export ILEX_SECURITY_ACCELERATOR
ILEX_SECURITY_ACCELERATOR=$(cat "$ILEX_ARTIFACTS/security/selected-accelerator")
case "$ILEX_SECURITY_ACCELERATOR" in kvm|tcg) ;; *) exit 1 ;; esac
cargo xtask security --require-qualified --accelerator "$ILEX_SECURITY_ACCELERATOR" --memory 6G
timeout --signal=TERM --kill-after=30s 105m cargo xtask test security::upgrade_rollback -- --test-threads 1
python3 - "$ILEX_ARTIFACTS/$tier/result.json" <<'PY'
import json, sys
with open(sys.argv[1]) as source:
    assert json.load(source)['qualification'] == 'passed', 'encrypted lifecycle remains unqualified'
PY
qualified=true
echo "Encrypted upgrade/rollback **passed** on $ILEX_SECURITY_ACCELERATOR / 6G / 2 vCPUs. Hyper-V and physical qualification are tracked separately." >> "$GITHUB_STEP_SUMMARY"
