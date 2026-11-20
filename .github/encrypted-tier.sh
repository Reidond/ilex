#!/bin/bash
# Exact-ISO tests own their registry, keys and media. Upload public evidence only.
set -euo pipefail
export ILEX_ARTIFACTS="${ILEX_ARTIFACTS:-$RUNNER_TEMP/security-artifacts}"
tiers=(encrypted-install encrypted-existing-space encrypted-interrupted)
for tier in "${tiers[@]}"; do mkdir -p "$ILEX_ARTIFACTS/$tier"; done
qualified=false
finish() {
    result=$?
    if [[ "$qualified" != true ]]; then
        for tier in "${tiers[@]}"; do
            printf '{"qualification":"unqualified","exit_code":%s}\n' "$result" > "$ILEX_ARTIFACTS/$tier/ci-result.json"
        done
        echo 'Exact encrypted storage/interruption gates: **UNQUALIFIED**; missing prerequisites, failed or incomplete evidence fail this step.' >> "$GITHUB_STEP_SUMMARY"
    fi
    return "$result"
}
trap finish EXIT
[[ -n "${ILEX_IMAGE:-}" && -f "${ILEX_ISO:-}" ]] || { echo '::error::encrypted qualification requires a local candidate and exact ISO'; exit 1; }
command -v openssl >/dev/null
python3 -B crates/ilex-e2e/src/encrypted/log_stream_check.py
export ILEX_SECURITY_ACCELERATOR
ILEX_SECURITY_ACCELERATOR=$(cat "$ILEX_ARTIFACTS/security/selected-accelerator")
case "$ILEX_SECURITY_ACCELERATOR" in kvm|tcg) ;; *) exit 1 ;; esac
cargo xtask security --require-qualified --accelerator "$ILEX_SECURITY_ACCELERATOR" --memory 6G
# Each case uses the exact qualified 6 GiB profile and its own private fixture.
timeout --signal=TERM --kill-after=30s 175m cargo xtask test security::encrypted_ -- --test-threads 1
python3 - "$ILEX_ARTIFACTS" <<'PY'
import json, pathlib, sys
for tier in ('encrypted-install', 'encrypted-existing-space', 'encrypted-interrupted'):
    result = pathlib.Path(sys.argv[1]) / tier / 'result.json'
    assert json.loads(result.read_text())['qualification'] == 'passed', tier + ' remains unqualified'
PY
qualified=true
echo "Exact encrypted install gates: **passed**, automatic storage, existing free space, interrupted completion, PCR mismatch and recovery-key boots, $ILEX_SECURITY_ACCELERATOR / 6G / 2 vCPUs." >> "$GITHUB_STEP_SUMMARY"
