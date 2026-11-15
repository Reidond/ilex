#!/bin/bash
# Keep per-root snapshots/evidence/results. A hard failure is red and stops
# publication; prior successful tags remain signed, and the next run catches up.
set -euo pipefail
without_key() { env -u COSIGN_PRIVATE_KEY -u COSIGN_PASSWORD "$@"; }
without_key python3 -I .github/root-table.py <<<"$ROOTS" > "$RUNNER_TEMP/root-rows"
results='[]'
while IFS=$'\t' read -r TAG SNAPSHOT changed STAGED FINGERPRINT IMAGE_ID; do
    export TAG SNAPSHOT STAGED FINGERPRINT IMAGE_ID
    without_key bash .github/publication-tag.sh "$TAG"
    published=false
    if [ "$changed" = true ]; then
        result="$RUNNER_TEMP/root-result"
        : > "$result"
        GITHUB_OUTPUT="$result" bash .github/publish-root.sh
        published=$(sed -n 's/^published=//p' "$result")
        [[ "$published" = true || "$published" = false ]] || exit 2
    fi
    # shellcheck disable=SC2016 # jq variables, expanded by jq
    results=$(without_key jq -c --arg tag "$TAG" --argjson published "$published" \
        '. + [{$tag, $published}]' <<<"$results")
    if [ "$TAG" = "$INSTALLER_TAG" ]; then
        echo "published=$published" >> "$GITHUB_OUTPUT"
    fi
    printf -- '- %s: published=%s (changed=%s, own snapshot %s)\n' \
        "$TAG" "$published" "$changed" "$SNAPSHOT" >> "$GITHUB_STEP_SUMMARY"
done < "$RUNNER_TEMP/root-rows"
echo "roots=$results" >> "$GITHUB_OUTPUT"
