#!/bin/bash
# The installer has already staged and passed ephemeral. Finish its inventory,
# then build every other root; export evidence only after every test succeeds.
set -euo pipefail
bash .github/publication-tag.sh "$INSTALLER_TAG"
roots='[]'
record() {
    roots=$(jq -c --arg tag "$tag" --arg snapshot "$snapshot" --argjson changed "$changed" \
        --arg digest "$digest" --arg fingerprint "$fingerprint" --arg image_id "$image_id" \
        '. + [{$tag, $snapshot, $changed, $digest, $fingerprint, $image_id}]' <<<"$roots")
    printf '| %s | %s | %s | %s |\n' "$tag" "$changed" "$digest" "$image_id" >> "$GITHUB_STEP_SUMMARY"
}
printf '### Root tests\n\n| Root | Changed | Staged digest | Ephemeral tested ID |\n| --- | --- | --- | --- |\n' >> "$GITHUB_STEP_SUMMARY"
tag=$INSTALLER_TAG; snapshot=$INSTALLER_SNAPSHOT; changed=$INSTALLER_CHANGED
digest=${INSTALLER_DIGEST:--}; fingerprint=${INSTALLER_FINGERPRINT:--}; image_id=${INSTALLER_IMAGE_ID:--}
record
while IFS=$'\t' read -r tag kind _ _ shells; do
    bash .github/publication-tag.sh "$tag"
    [ "$kind" = root ] && [ "$tag" != "$INSTALLER_TAG" ] || continue
    started=$SECONDS
    snapshot=$(jq -er --arg tag "$tag" '.[$tag]' <<<"$SNAPSHOTS")
    reference=-
    if [ "$snapshot" != - ]; then reference="$IMAGE_REPO@$snapshot"; fi
    : > "$RUNNER_TEMP/root-inputs"
    GITHUB_OUTPUT="$RUNNER_TEMP/root-inputs" cargo xtask changed "$reference" --root "$tag" --inputs-only
    if grep -qx 'inputs_changed=false' "$RUNNER_TEMP/root-inputs" && [ "$FORCE_TEST" != true ]; then
        changed=false; digest=-; fingerprint=-; image_id=-
        echo "$tag: selected sources and upstream images unchanged; skipping before build"
        record
        continue
    fi
    selection=$shells
    if [ "$selection" = - ]; then selection=none; fi
    export ILEX_SHELLS=$selection
    image="localhost/ilex:candidate.$tag"
    rechunked="localhost/ilex:rechunked.$tag"
    cargo xtask build --tag "$image" --no-pull
    : > "$RUNNER_TEMP/root-change"
    GITHUB_OUTPUT="$RUNNER_TEMP/root-change" cargo xtask changed "$reference" --image "$image"
    changed=$(sed -n 's/^changed=//p' "$RUNNER_TEMP/root-change")
    fingerprint=$(sed -n 's/^fingerprint=//p' "$RUNNER_TEMP/root-change")
    digest=-; image_id=-
    if [ "$changed" = true ] || [ "$FORCE_TEST" = true ]; then
        cargo xtask rechunk "$image" --output "$rechunked" --fingerprint "$fingerprint"
        bash .github/vm-prep.sh wait "root-pull.$tag"
        image_id=$(podman image inspect --format '{{.Id}}' "$rechunked")
        image_id="sha256:${image_id#sha256:}"
        stage_tag=$(bash .github/candidate-tag.sh "$tag")
        : > "$RUNNER_TEMP/root-stage"
        GITHUB_OUTPUT="$RUNNER_TEMP/root-stage" cargo xtask publish "$image_id" --stage --as "$tag" \
            --stage-tag "$stage_tag" --stage-snapshot "$snapshot"
        digest=$(sed -n 's/^digest=//p' "$RUNNER_TEMP/root-stage")
        fingerprint=$(sed -n 's/^fingerprint=//p' "$RUNNER_TEMP/root-stage")
        podman run --rm -i --network=none --entrypoint python3 "$image_id" -I -S - < .github/check-filecaps.py
        bash .github/vm-prep.sh wait vm-stack
        ILEX_IMAGE="$image_id" sg libvirt -c 'cargo xtask test ephemeral::'
    else
        fingerprint=-
        echo "$tag: fingerprint unchanged; no rechunk, stage, tests or publication"
    fi
    record
    echo "root build duration: $tag $((SECONDS - started)) seconds"
done < "$RUNNER_TEMP/publications"
python3 -I .github/root-table.py <<<"$roots" > "$RUNNER_TEMP/tested-roots"
{
    echo "roots=$roots"
    echo "changed=$(jq -r 'any(.changed)' <<<"$roots")"
    echo "other_changed=$(jq -r --arg installer "$INSTALLER_TAG" '[.[] | select(.tag != $installer)] | any(.changed)' <<<"$roots")"
} >> "$GITHUB_OUTPUT"
