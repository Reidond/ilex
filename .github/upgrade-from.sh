#!/bin/bash
# The upgrade source may be legacy latest; own is always the installer's tag.
# Errors are fatal; only explicit absence permits fallback or a first-publish skip.
set -euo pipefail
repo=${1:?repository required}; tag=${2:?installer composition required}
bash .github/publication-tag.sh "$tag"
own=$(bash .github/publish-head.sh digest "$repo:$tag")
digest=$own
if [ "$digest" = - ]; then
    digest=$(bash .github/publish-head.sh digest "$repo:latest")
    if [ "$digest" != - ]; then
        echo "::notice::no $repo:$tag yet; using legacy :latest as upgrade v1 ($digest)"
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
