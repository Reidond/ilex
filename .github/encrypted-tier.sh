#!/bin/bash
# Exact-ISO tests own their registry, keys and media. Upload public evidence only.
set -euo pipefail
export ILEX_ARTIFACTS="${ILEX_ARTIFACTS:-$RUNNER_TEMP/security-artifacts}"
mkdir -p "$ILEX_ARTIFACTS/encrypted-install" "$ILEX_ARTIFACTS/encrypted-existing-space"
qualified=false
finish() {
    result=$?
    if [[ "$qualified" != true ]]; then
        for tier in encrypted-install encrypted-existing-space; do
            printf '{"qualification":"unqualified","exit_code":%s}\n' "$result" > "$ILEX_ARTIFACTS/$tier/ci-result.json"
        done
        echo 'Exact encrypted storage gates: **UNQUALIFIED**; see public evidence for both storage modes.' >> "$GITHUB_STEP_SUMMARY"
    fi
    return "$result"
}
trap finish EXIT
export ILEX_SECURITY_ACCELERATOR
ILEX_SECURITY_ACCELERATOR=$(cat "$ILEX_ARTIFACTS/security/selected-accelerator")
case "$ILEX_SECURITY_ACCELERATOR" in kvm|tcg) ;; *) exit 1 ;; esac
cargo xtask security --require-qualified --accelerator "$ILEX_SECURITY_ACCELERATOR" --memory 6G
# Each case uses the exact qualified 6 GiB profile and its own private fixture.
timeout --signal=TERM --kill-after=30s 175m cargo xtask test security::encrypted_ -- --test-threads 1
python3 - "$ILEX_ARTIFACTS" <<'PY'
import json, pathlib, sys
for tier in ('encrypted-install', 'encrypted-existing-space'):
    result = pathlib.Path(sys.argv[1]) / tier / 'result.json'
    assert json.loads(result.read_text())['qualification'] == 'passed', tier + ' remains unqualified'
PY
qualified=true
echo "Exact encrypted storage gates: **qualified**, automatic storage and existing free space, exact ISO and disposable signed candidate, $ILEX_SECURITY_ACCELERATOR / 6G / 2 vCPUs." >> "$GITHUB_STEP_SUMMARY"
