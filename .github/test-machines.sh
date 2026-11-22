#!/bin/bash
set -euo pipefail
: > "$RUNNER_TEMP/tested-machines"
nohup bash .github/vm-prep.sh ephemeral > /dev/null 2>&1 &
while IFS=$'\t' read -r tag parent machine shells base base_id previous; do
    bash .github/publication-tag.sh "$tag"
    bash .github/publication-tag.sh "$parent"
    podman pull --quiet "$base"
    bash .github/machine-base.sh files "$base" "$base_id"
    podman run --rm -i --network=none --entrypoint python3 "$base" -I -S - < .github/check-filecaps.py
    # No composition name enters sg's constant shell command. Build inherits
    # the pinned parent's full selection; the publisher checks it independently.
    selection_shells=$shells
    if [ "$selection_shells" = - ]; then selection_shells=none; fi
    image=$(ILEX_SHELLS="$selection_shells" cargo ilexctl build --machine "$machine" --on "$base")
    reference=-
    if [ "$previous" != - ]; then reference="$IMAGE_REPO@$previous"; fi
    cargo ilexctl changed "$reference" --machine "$machine" --image "$image" > "$RUNNER_TEMP/machine-change"
    cat "$RUNNER_TEMP/machine-change"
    if grep -qx 'changed=false' "$RUNNER_TEMP/machine-change" && [ "$FORCE_TEST" != true ]; then
        printf -- '- %s: skipped publication, full fingerprint unchanged at %s\n' "$tag" "$previous" >> "$GITHUB_STEP_SUMMARY"
        continue
    fi
    cargo ilexctl prepare-publish "$image" --machine "$machine"
    image_id=$(podman image inspect --format '{{.Id}}' "$image")
    podman run --rm -i --network=none --entrypoint python3 "$image_id" -I -S - < .github/check-filecaps.py
    bash .github/vm-prep.sh wait vm-stack
    export ILEX_IMAGE="sha256:${image_id#sha256:}"
    sg libvirt -c 'cargo ilexctl test ephemeral::'
    publish=false
    if grep -qx 'changed=true' "$RUNNER_TEMP/machine-change"; then publish=true; fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "$tag" "$parent" "$base" "$image" "$image_id" "$previous" "$publish" >> "$RUNNER_TEMP/tested-machines"
    printf -- '- %s: tests passed on %s, planned tag %s; changed=%s\n' \
        "$tag" "$image_id" "$previous" "$publish" >> "$GITHUB_STEP_SUMMARY"
done < "$RUNNER_TEMP/pending-machines"
if grep -q $'\ttrue$' "$RUNNER_TEMP/tested-machines"; then echo 'changed=true' >> "$GITHUB_OUTPUT"; fi
