#!/bin/bash
# Select a signed, immutable baseline from the pre-publication snapshot or
# from retained composition history when qualifying an unchanged image.
set -euo pipefail
repo=${IMAGE_REPO:?image repository required}
tag=${INSTALLER_TAG:?installer composition required}
snapshot=${INSTALLER_BASELINE_SNAPSHOT:?installer snapshot required}
target=${INSTALLER_BASELINE_TARGET:-}
reference=${ILEX_ENCRYPTED_UPGRADE_FROM:-}
digest_pattern='^sha256:[0-9a-f]{64}$'
fail() { echo "::error::$*" >&2; exit 1; }
bash .github/publication-tag.sh "$tag"
[[ "$repo" =~ ^ghcr\.io/[a-z0-9-]+/ilex$ ]] || fail 'Invalid installer image repository.'
[[ "$snapshot" = - || "$snapshot" =~ $digest_pattern ]] || fail 'Invalid installer snapshot digest.'
[[ -z "$target" || "$target" = - || "$target" =~ $digest_pattern ]] || fail 'Invalid installer target digest.'

if [[ -n "$reference" ]]; then
    digest=${reference#"$repo@"}
    [[ "$digest" =~ $digest_pattern ]] || fail "Expected a rollback override of $repo@sha256:<digest>."
    [[ "$digest" != "$target" ]] || fail 'The rollback override points to the installer target itself.'
    origin=override
elif [[ "$snapshot" = - ]]; then
    origin=no-published-composition
elif [[ "$snapshot" != "$target" ]]; then
    reference="$repo@$snapshot"
    origin=registry-snapshot
else
    # Date tags track nightly publications and revision tags retain same-day
    # history. Skip staged versions and select by signed digest, not by tag.
    owner=${repo#ghcr.io/}
    owner=${owner%/ilex}
    history=$(gh api --paginate "/users/$owner/packages/container/ilex/versions?per_page=100" |
        jq -sr --arg tag "$tag" --arg target "$target" '
            add as $versions |
            ($versions | [.[].metadata.container.tags[]?] | INDEX(.)) as $tags |
            $versions | sort_by(.created_at, .id) | reverse |
            map(select(.name == $target or
                (.name | test("^sha256:[0-9a-f]{64}$")))) |
            (map(.name) | index($target)) as $current |
            if $current == null then error("Published target missing from package history") else
                .[$current + 1:][] | select(.name != $target) |
                select(any(.metadata.container.tags[]?;
                    startswith($tag + "_") and (.[($tag | length) + 1:] |
                    test("^([0-9]{8}|sha-[0-9a-f]{7,40})$")))) |
                select($tags[(.name | sub(":"; "-")) + ".sig"]) | .name
            end') || fail 'Could not read signed installer history from GHCR.'
    if [[ -n "$history" ]]; then
        reference="$repo@${history%%$'\n'*}"
        origin=previous-signed-composition
    else
        origin=no-previous-distinct-signed-composition
    fi
fi

if [[ -n "$reference" ]]; then
    cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true "$reference" >/dev/null
    printf 'Encrypted lifecycle baseline (%s) **%s**.\n' "$origin" "$reference" >> "$GITHUB_STEP_SUMMARY"
else
    printf 'Encrypted fresh-install qualification only (%s).\n' "$origin" >> "$GITHUB_STEP_SUMMARY"
fi
printf 'reference=%s\n' "$reference" >> "$GITHUB_OUTPUT"
mkdir -p "${ILEX_ARTIFACTS:?evidence directory required}/security"
jq -n --arg reference "$reference" --arg origin "$origin" --arg snapshot "$snapshot" --arg target "$target" \
    '{$reference, $origin, $snapshot, $target}' > "$ILEX_ARTIFACTS/security/encrypted-baseline.json"
