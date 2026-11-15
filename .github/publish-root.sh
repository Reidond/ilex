#!/bin/bash
# The build snapshot belongs to this tag, even when upgrade used legacy latest.
set -euo pipefail
without_key() { env -u COSIGN_PRIVATE_KEY -u COSIGN_PASSWORD "$@"; }
without_key bash .github/publication-tag.sh "$TAG"
[[ "$STAGED" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
if { [ "$V1_EXISTS" = true ] && [[ "$V1_DIGEST" =~ ^sha256:[0-9a-f]{64}$ ]]; } ||
   { [ "$V1_EXISTS" = false ] && [ "$V1_DIGEST" = - ]; }; then :; else
    echo '::error::upgrade did not record an explicit v1 digest or first-publish absence'; exit 2
fi
[[ "$OWN" = - || "$OWN" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
if [ "$OWN" != - ] && [ "$OWN" != "$V1_DIGEST" ]; then
    echo '::error::upgrade v1 differs from its installer-tag snapshot'; exit 2
fi
if [ "$TESTED" != "$STAGED" ]; then
    if [ -n "$TESTED" ] || [ "$V1_EXISTS" != false ]; then
        echo "::error::the build job staged ${STAGED:-nothing}, but the upgrade tier tested ${TESTED:-nothing}"
        exit 2
    fi
    echo "no published v1 yet: the first publish has no upgrade tier"
fi
if without_key bash .github/publish-head.sh image "$GITHUB_SHA" "$GITHUB_REPOSITORY" "$IMAGE_REPO:$TAG" &&
   without_key bash .github/publish-head.sh swap "$IMAGE_REPO:$TAG" "$SNAPSHOT"; then
    "$XTASK_PUBLISHER" publish --digest "$STAGED" --as "$TAG"
    echo 'published=true' >> "$GITHUB_OUTPUT"
    echo "root publication result: $TAG published=true"
else
    rc=$?
    if [ "$rc" = 3 ]; then
        echo 'published=false' >> "$GITHUB_OUTPUT"
        echo "root publication result: $TAG published=false (guard skip)"
        exit 0
    fi
    exit "$rc"
fi
