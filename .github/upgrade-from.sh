#!/bin/bash
# The upgrade source may be legacy latest; own is always the installer's tag.
# Errors are fatal; only explicit absence permits fallback or a first-publish skip.
set -euo pipefail
repo=${1:?repository required}; tag=${2:?installer composition required}
bash .github/publication-tag.sh "$tag"
own=${3:?installer snapshot required}
legacy=${4:?legacy snapshot required}
for snapshot in "$own" "$legacy"; do
    [[ "$snapshot" = - || "$snapshot" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
done
digest=$own
# The enforcing VM must check a real signed image, including before the
# installer's composition has ever been published. Never skip a missing tag.
echo "ILEX_SIGPOLICY_LEGACY=0" >> "$GITHUB_ENV"
if [ "$digest" = - ]; then
    digest=$legacy
    if [ "$digest" != - ]; then
        echo "::notice::no $repo:$tag yet; using legacy :latest as upgrade v1 ($digest)"
        echo "ILEX_SIGPOLICY_LEGACY=1" >> "$GITHUB_ENV"
    fi
fi
if [ "$digest" != - ]; then
    disk_key=$(bash .github/v1-disk-key.sh "$digest")
fi
echo "own=$own" >> "$GITHUB_OUTPUT"
echo "digest=$digest" >> "$GITHUB_OUTPUT"
if [ "$digest" != - ]; then
    echo "v1: $repo@$digest (installer $tag, own=$own)"
    echo "ILEX_UPGRADE_FROM=$repo@$digest" >> "$GITHUB_ENV"
    echo "exists=true" >> "$GITHUB_OUTPUT"
    echo "disk_key=$disk_key" >> "$GITHUB_OUTPUT"
else
    echo "::notice::no $repo:$tag or legacy :latest yet: nothing to upgrade from"
    echo "exists=false" >> "$GITHUB_OUTPUT"
fi
