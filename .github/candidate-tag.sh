#!/bin/bash
# Print the staging tag shared by the build and upgrade runners.
set -euo pipefail
tag=${1:?root composition tag required}
bash .github/publication-tag.sh "$tag"
ref=${GITHUB_REF:?GitHub ref required}
if [ "$ref" = refs/heads/main ]; then
    printf 'candidate.%s\n' "$tag"
    exit 0
fi

# The hash covers the full, case-sensitive ref and the composition. The
# readable part can be shortened or collapse to the same slug for other refs.
slug=$(printf '%s' "${ref#refs/*/}" | LC_ALL=C tr 'A-Z' 'a-z' | LC_ALL=C tr -cs 'a-z0-9' '-')
hash=$(printf '%s\n' "$ref" "$tag" | sha256sum)
printf 'candidate-%s-%s\n' "${slug:0:32}" "${hash%% *}"
