#!/bin/bash
# Each layer resolves only its own parent's successful test/publication row.
# One job plans one root's layers (ROOT_TAG, with ROOTS holding that root's
# inventory row): a layer of any other root belongs to that root's own job.
set -euo pipefail
bash .github/publication-tag.sh "${ROOT_TAG:?root composition tag required}"
python3 -I .github/root-table.py <<<"$ROOTS" > "$RUNNER_TEMP/root-rows"
: > "$RUNNER_TEMP/pending-machines"
while IFS=$'\t' read -r tag kind parent machine shells; do
    bash .github/publication-tag.sh "$tag"
    [ "$kind" = layer ] || continue
    bash .github/publication-tag.sh "$parent"
    [ "$parent" = "$ROOT_TAG" ] || continue
    row=$(awk -F '\t' -v parent="$parent" '$1 == parent' "$RUNNER_TEMP/root-rows")
    [ -n "$row" ] || { echo "::error::no tested root row for $parent"; exit 2; }
    IFS=$'\t' read -r _ snapshot BASE_CHANGED STAGED _ _ <<<"$row"
    if [ "$GITHUB_REF" = refs/heads/main ] && [ "$BASE_CHANGED" = true ]; then
        published=$(jq -er --arg tag "$parent" \
            '[.[] | select(.tag == $tag) | .published] | if length == 0 then "false" elif length == 1 then .[0] | tostring else error("duplicate publication") end' <<<"$PUBLISHED_ROOTS")
        if [ "$published" = false ]; then
            echo "::notice::$tag skipped: changed parent $parent was not published"
            continue
        fi
        [ "$published" = true ] || exit 2
    fi
    if [ "$STAGED" != - ] && { [ "$BASE_CHANGED" = true ] || [ "$GITHUB_REF" != refs/heads/main ]; }; then
        digest=$STAGED
    else
        # A forced unchanged main run must use its signed parent, not staging.
        digest=$snapshot
    fi
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
    base="$IMAGE_REPO@$digest"
    echo "$tag parent: $parent at $base"
    bash .github/machine-base.sh registry "$base" > "$RUNNER_TEMP/base-info"
    base_id=$(sed -n 's/^base_id=//p' "$RUNNER_TEMP/base-info")
    previous=$(jq -er --arg tag "$tag" '.[$tag]' <<<"$SNAPSHOTS")
    reference=-
    if [ "$previous" != - ]; then reference="$IMAGE_REPO@$previous"; fi
    cargo ilexctl changed "$reference" --machine "$machine" --inputs-only --base-id "$base_id" > "$RUNNER_TEMP/machine-inputs"
    cat "$RUNNER_TEMP/machine-inputs"
    if [ "$BASE_CHANGED" != true ] && [ "$FORCE_TEST" != true ] &&
       grep -qx 'inputs_changed=false' "$RUNNER_TEMP/machine-inputs" &&
       grep -qx 'base_changed=false' "$RUNNER_TEMP/machine-inputs"; then
        echo "$tag: published parent and machine inputs unchanged; skipping without pulling"
        printf -- '- %s: skipped before build, inputs/parent unchanged at %s\n' "$tag" "$previous" >> "$GITHUB_STEP_SUMMARY"
        continue
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$tag" "$parent" "$machine" "$shells" "$base" "$base_id" "$previous" >> "$RUNNER_TEMP/pending-machines"
done < "$RUNNER_TEMP/publications"
if [ -s "$RUNNER_TEMP/pending-machines" ]; then echo 'build=true' >> "$GITHUB_OUTPUT"; fi
