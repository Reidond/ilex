#!/bin/bash
# PR 11 produces the installer root only. PR 12 supplies a table of all roots;
# keep each layer's explicit parent and selection in the inventory for it.
set -euo pipefail
bash .github/publication-tag.sh "$ROOT_TAG"
: > "$RUNNER_TEMP/pending-machines"
while IFS=$'\t' read -r tag kind parent machine shells; do
    bash .github/publication-tag.sh "$tag"
    [ "$kind" = layer ] || continue
    bash .github/publication-tag.sh "$parent"
    if [ "$parent" != "$ROOT_TAG" ]; then
        echo "::notice::$tag deferred: this workflow currently builds only root $ROOT_TAG"
        continue
    fi
    if [ -n "$STAGED" ] && { [ "$BASE_CHANGED" = true ] || [ "$GITHUB_REF" != refs/heads/main ]; }; then
        if [ "$TESTED" != "$STAGED" ] && { [ -n "$TESTED" ] || [ "$V1_EXISTS" != false ]; }; then
            echo "::error::the upgrade tier did not test the staged parent $STAGED"; exit 2
        fi
        digest=$STAGED
    else
        # A forced unchanged main run must use its signed parent, not staging.
        digest=$(bash .github/publish-head.sh digest "$IMAGE_REPO:$parent")
    fi
    [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
    base="$IMAGE_REPO@$digest"
    echo "$tag parent: $parent at $base"
    bash .github/machine-base.sh registry "$base" > "$RUNNER_TEMP/base-info"
    base_id=$(sed -n 's/^base_id=//p' "$RUNNER_TEMP/base-info")
    previous=$(bash .github/publish-head.sh digest "$IMAGE_REPO:$tag")
    reference="$IMAGE_REPO:$tag"
    if [ "$previous" != - ]; then reference="$IMAGE_REPO@$previous"; fi
    cargo xtask changed "$reference" --machine "$machine" --inputs-only --base-id "$base_id" > "$RUNNER_TEMP/machine-inputs"
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
