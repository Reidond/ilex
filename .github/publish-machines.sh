#!/bin/bash
# All rows passed capability and VM tests. Publish each independently, keeping
# its planned registry snapshot alongside the immutable tested image ID.
set -euo pipefail
without_key() { env -u COSIGN_PRIVATE_KEY -u COSIGN_PASSWORD "$@"; }
: "${XTASK_PUBLISHER:?prebuilt publisher required}" "${BASE:?tested base required}"
: "${IMAGE_REPO:?repository required}" "${RUNNER_TEMP:?runner directory required}"
: > "$RUNNER_TEMP/machine-publications"
published_count=0; skipped_count=0
record() {
    printf '%s\t%s\t%s\t%s\n' "$machine" "$image_id" "$1" "$2" >> "$RUNNER_TEMP/machine-publications"
    printf '%s: %s (%s), tested ID %s\n' "$machine" "$1" "$2" "$image_id"
    printf -- '- `%s`: **%s** (%s), tested ID `%s`, planned tag `%s`\n' \
        "$machine" "$1" "$2" "$image_id" "$expected" >> "$GITHUB_STEP_SUMMARY"
}
while IFS=$'\t' read -r machine image image_id expected changed; do
    if [ "$changed" = false ]; then
        record skipped 'fingerprint unchanged'; skipped_count=$((skipped_count + 1)); continue
    fi
    [ "$changed" = true ] || { echo "::error::invalid tested row for $machine" >&2; exit 2; }
    if without_key bash .github/publish-head.sh image "$GITHUB_SHA" "$GITHUB_REPOSITORY" "$IMAGE_REPO:$machine"; then :; else
        rc=$?
        if [ "$rc" = 3 ]; then
            record skipped ancestry; skipped_count=$((skipped_count + 1)); continue
        fi
        exit "$rc"
    fi
    # A newer same-commit machine digest must not be replaced by an old tested
    # list. A whole-job rerun re-plans from current tags and rebuilds/tests.
    if without_key bash .github/publish-head.sh swap "$IMAGE_REPO:$machine" "$expected"; then :; else
        rc=$?
        if [ "$rc" = 3 ]; then
            record skipped 'registry snapshot moved'; skipped_count=$((skipped_count + 1)); continue
        fi
        exit "$rc"
    fi
    latest=$(without_key bash .github/publish-head.sh digest "$IMAGE_REPO:latest")
    if [ "$BASE" != "$IMAGE_REPO@$latest" ]; then
        echo "::error::machine tested base $BASE differs from current :latest $IMAGE_REPO@$latest" >&2
        exit 2
    fi
    "$XTASK_PUBLISHER" publish "$image" --machine "$machine" --tested-image-id "$image_id"
    record published 'ancestry, registry snapshot and base match'
    published_count=$((published_count + 1))
done < "$RUNNER_TEMP/tested-machines"
echo "machine publication summary: $published_count published, $skipped_count skipped"
