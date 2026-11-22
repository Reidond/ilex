#!/bin/bash
# All rows passed capability and VM tests. Publish each independently, keeping
# its planned registry snapshot alongside the immutable tested image ID.
set -euo pipefail
without_key() { env -u COSIGN_PRIVATE_KEY -u COSIGN_PASSWORD "$@"; }
: "${XTASK_PUBLISHER:?prebuilt publisher required}"
: "${IMAGE_REPO:?repository required}" "${RUNNER_TEMP:?runner directory required}"
: > "$RUNNER_TEMP/machine-publications"
published_count=0; skipped_count=0
record() {
    printf '%s\t%s\t%s\t%s\n' "$tag" "$image_id" "$1" "$2" >> "$RUNNER_TEMP/machine-publications"
    printf '%s: %s (%s), tested ID %s\n' "$tag" "$1" "$2" "$image_id"
    printf -- '- %s: **%s** (%s), tested ID %s, planned tag %s\n' \
        "$tag" "$1" "$2" "$image_id" "$expected" >> "$GITHUB_STEP_SUMMARY"
}
while IFS=$'\t' read -r tag parent base image image_id expected changed; do
    without_key bash .github/publication-tag.sh "$tag"
    without_key bash .github/publication-tag.sh "$parent"
    if [ "$changed" = false ]; then
        record skipped 'fingerprint unchanged'; skipped_count=$((skipped_count + 1)); continue
    fi
    [ "$changed" = true ] || { echo "::error::invalid tested row for ${tag}" >&2; exit 2; }
    if without_key bash .github/publish-head.sh image "$GITHUB_SHA" "$GITHUB_REPOSITORY" "$IMAGE_REPO:$tag" "$expected"; then :; else
        rc=$?
        if [ "$rc" = 3 ]; then
            record skipped ancestry; skipped_count=$((skipped_count + 1)); continue
        fi
        exit "$rc"
    fi
    # A newer same-commit machine digest must not be replaced by an old tested
    # list. A fresh run takes new snapshots and rebuilds/tests.
    if without_key bash .github/publish-head.sh swap "$IMAGE_REPO:$tag" "$expected"; then :; else
        rc=$?
        if [ "$rc" = 3 ]; then
            record skipped 'registry snapshot moved'; skipped_count=$((skipped_count + 1)); continue
        fi
        exit "$rc"
    fi
    current=$(without_key bash .github/publish-head.sh digest "$IMAGE_REPO:$parent")
    if [ "$base" != "$IMAGE_REPO@$current" ]; then
        echo "::error::${tag} tested parent $base differs from current :$parent $IMAGE_REPO@$current" >&2
        exit 2
    fi
    "$XTASK_PUBLISHER" --ilexctl-in-checkout publish "$image" --as "$tag" --tested-image-id "$image_id" \
        --tested-parent-digest "${base##*@}"
    record published 'ancestry, registry snapshot and base match'
    published_count=$((published_count + 1))
done < "$RUNNER_TEMP/tested-machines"
echo "machine publication summary: $published_count published, $skipped_count skipped"
echo "Machine publication summary: $published_count published, $skipped_count skipped" >> "$GITHUB_STEP_SUMMARY"
