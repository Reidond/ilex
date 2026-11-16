#!/bin/bash
# Authenticated reads use REGISTRY_AUTH_FILE (skopeo) and DOCKER_CONFIG (cosign).
# An optional expected digest makes absence/movement fatal after installation.
set -euo pipefail
repo=${IMAGE_REPO:?image repository required}
tag=${INSTALLER_TAG:?installer composition required}
expected=${1:-}
# Every offered tag must exist with a signature before this ISO can release.
# Preserve the exact-default-digest verification and install gate below.
if cargo xtask iso --check-registry; then :; else
    rc=$?
    if [ "$rc" = 3 ]; then
        if [ -n "$expected" ]; then echo '::error::an offered composition disappeared after testing'; exit 2; fi
        echo '::warning::Not releasing installer: offered compositions/signatures are not all published'
        echo 'available=false' >> "$GITHUB_OUTPUT"
        exit 0
    fi
    exit "$rc"
fi
bash .github/publication-tag.sh "$tag"
if [ -n "$expected" ]; then [[ "$expected" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2; fi
digest=$(bash .github/publish-head.sh digest "$repo:$tag")
if [ "$digest" = - ]; then
    if [ -n "$expected" ]; then
        echo "::error::installer target $repo:$tag disappeared after testing"; exit 2
    fi
    echo "::warning::Not releasing installer: $repo:$tag does not exist yet; the next full run retries"
    echo 'available=false' >> "$GITHUB_OUTPUT"
    exit 0
fi
if [ -n "$expected" ] && [ "$digest" != "$expected" ]; then
    echo "::error::installer target moved: tested $expected, current $digest"; exit 2
fi
cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true "$repo@$digest"
echo "installer target signature verified: $repo:$tag at $digest"
printf 'available=true\ndigest=%s\ntarget=%s:%s\n' "$digest" "$repo" "$tag" >> "$GITHUB_OUTPUT"
