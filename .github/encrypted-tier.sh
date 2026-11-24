#!/bin/bash
# Exact-ISO tests own their registry, keys and media. Upload public evidence only.
set -euo pipefail
export ILEX_ARTIFACTS="${ILEX_ARTIFACTS:-$RUNNER_TEMP/security-artifacts}"
tiers=(encrypted-install encrypted-existing-space encrypted-interrupted)
for tier in "${tiers[@]}"; do mkdir -p "$ILEX_ARTIFACTS/$tier"; done
qualified=false
private_fingerprints=
finish() {
    result=$?
    if [[ "$qualified" != true ]]; then
        if [[ -n "$private_fingerprints" ]]; then
            for tier in "${tiers[@]}"; do rm -f -- "$private_fingerprints/$tier.json"; done
            rmdir -- "$private_fingerprints"
        fi
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
cargo ilexctl doctor security --require-qualified --accelerator kvm --memory 6G
# Endpoint hashes stay in a private runner directory, outside the evidence.
private_fingerprints=$(mktemp -d "$RUNNER_TEMP/ilex-fingerprints.XXXXXXXX")
export ILEX_PRIVATE_FINGERPRINTS="$private_fingerprints"
# Each case uses the exact qualified 6 GiB profile and its own private fixture.
timeout --signal=TERM --kill-after=30s 175m cargo ilexctl test security::encrypted_ -- --test-threads 1
python3 - "$ILEX_ARTIFACTS" <<'PY'
import json, pathlib, sys
for tier in ('encrypted-install', 'encrypted-existing-space', 'encrypted-interrupted'):
    result = pathlib.Path(sys.argv[1]) / tier / 'result.json'
    assert json.loads(result.read_text())['qualification'] == 'passed', tier + ' remains unqualified'
PY
printf 'ILEX_PRIVATE_FINGERPRINTS=%s\n' "$private_fingerprints" >> "$GITHUB_ENV"
qualified=true
echo "Exact encrypted install gates: **passed**, automatic storage, existing free space, interrupted completion, PCR mismatch and recovery-key boots, kvm / 6G / 2 vCPUs." >> "$GITHUB_STEP_SUMMARY"
