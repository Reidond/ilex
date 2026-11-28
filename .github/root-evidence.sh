#!/bin/bash
# One root's tested-inventory row, written only after every test of that root's
# job succeeded (the job output is consumed solely by jobs that need the job to
# have succeeded). The row is a one-element JSON array in the schema
# root-table.py validates; the publisher and the machine planner validate it again.
# TAG SNAPSHOT [CHANGED DIGEST FINGERPRINT IMAGE_ID]: an unchanged root that
# was not forced has '-' for every test field.
set -euo pipefail
tag=${TAG:?root composition tag required}
bash .github/publication-tag.sh "$tag"
snapshot=${SNAPSHOT:?own registry snapshot required}
changed=${CHANGED:-false}
digest=${DIGEST:--}
fingerprint=${FINGERPRINT:--}
image_id=${IMAGE_ID:--}
[[ "$changed" = true || "$changed" = false ]] || { echo "::error::invalid changed flag for $tag: $changed"; exit 2; }
roots=$(jq -c -n --arg tag "$tag" --arg snapshot "$snapshot" --argjson changed "$changed" \
    --arg digest "$digest" --arg fingerprint "$fingerprint" --arg image_id "$image_id" \
    '[{$tag, $snapshot, $changed, $digest, $fingerprint, $image_id}]')
python3 -I .github/root-table.py <<<"$roots" > /dev/null
{
    printf '### Root %s\n\n| Root | Changed | Staged digest | Ephemeral tested ID |\n| --- | --- | --- | --- |\n' "$tag"
    printf '| %s | %s | %s | %s |\n' "$tag" "$changed" "$digest" "$image_id"
} >> "$GITHUB_STEP_SUMMARY"
{
    echo "roots=$roots"
    echo "changed=$changed"
} >> "$GITHUB_OUTPUT"
if [ -n "${STARTED:-}" ]; then
    echo "root build duration: $tag $(($(date +%s) - STARTED)) seconds"
fi
