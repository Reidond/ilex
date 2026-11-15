#!/bin/bash
# Resolve the newest published installer release's actual tag commit, not its
# target_commitish (which can be a moving branch). '-' means no installer yet.
set -euo pipefail
fail() { echo "::error::$*" >&2; exit 2; }
repo=${1:?owner/repository required}
releases=$(gh api "repos/$repo/releases?per_page=100" --paginate --slurp) ||
    fail 'cannot read installer releases'
tag=$(jq -er '
    if type != "array" or any(.[]; type != "array") then error("invalid pages") else . end |
    flatten | map(select(.draft == false and .prerelease == false and
        (.tag_name | startswith("installer-")))) |
    sort_by(.published_at) | last | if . == null then "-" else
        if (.published_at | type != "string") or (.published_at | length == 0)
        then error("invalid publication date") else .tag_name end end
' <<<"$releases") || fail 'unreadable installer release metadata'
if [ "$tag" = - ]; then echo -; exit 0; fi
[[ "$tag" =~ ^installer-[0-9]{8}(-[0-9]+)?$ ]] || fail "invalid installer tag: $tag"
revision=$(gh api "repos/$repo/commits/$tag" --jq .sha) ||
    fail "cannot resolve installer tag $tag"
[[ "$revision" =~ ^[0-9a-f]{40}$ ]] || fail "invalid installer revision: $revision"
echo "installer release $tag records commit $revision" >&2
echo "$revision"
