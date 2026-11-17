#!/bin/bash
# Exact-ISO tests own their registry, keys and media. Upload public evidence only.
set -euo pipefail
export ILEX_ARTIFACTS="${ILEX_ARTIFACTS:-$RUNNER_TEMP/security-artifacts}"
mkdir -p "$ILEX_ARTIFACTS/encrypted-install"
qualified=false
finish() {
    result=$?
    if [[ "$qualified" != true ]]; then
        printf '{"qualification":"unqualified","exit_code":%s}\n' "$result" > "$ILEX_ARTIFACTS/encrypted-install/ci-result.json"
        echo 'security::encrypted_install: **UNQUALIFIED**; see public evidence.' >> "$GITHUB_STEP_SUMMARY"
    fi
    return "$result"
}
trap finish EXIT
export ILEX_SECURITY_ACCELERATOR
ILEX_SECURITY_ACCELERATOR=$(cat "$ILEX_ARTIFACTS/security/selected-accelerator")
case "$ILEX_SECURITY_ACCELERATOR" in kvm|tcg) ;; *) exit 1 ;; esac
cargo xtask security --require-qualified --accelerator "$ILEX_SECURITY_ACCELERATOR" --memory 6G
timeout --signal=TERM --kill-after=30s 175m cargo xtask test security::encrypted_install
python3 - "$ILEX_ARTIFACTS/encrypted-install/result.json" <<'PY'
import json, sys
assert json.load(open(sys.argv[1]))['qualification'] == 'passed', 'encrypted ISO remains unqualified'
PY
qualified=true
echo "security::encrypted_install: **qualified**, exact ISO and disposable signed candidate, $ILEX_SECURITY_ACCELERATOR / 6G / 2 vCPUs." >> "$GITHUB_STEP_SUMMARY"
