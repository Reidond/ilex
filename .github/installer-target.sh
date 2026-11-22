#!/bin/bash
# Authenticated reads use REGISTRY_AUTH_FILE (skopeo) and DOCKER_CONFIG (cosign).
# Inputs are the run's published installer digest or its initial snapshot.
set -euo pipefail
repo=${IMAGE_REPO:?image repository required}
tag=${INSTALLER_TAG:?installer composition required}
recheck=${1:-}
expected=${recheck:-${INSTALLER_SNAPSHOT:?installer snapshot required}}
# Every offered tag must exist with a signature before this ISO can release.
# Preserve the exact-default-digest verification and install gate below.
if cargo xtask iso --check-registry; then :; else
    rc=$?
    if [ "$rc" = 3 ]; then
        if [ -n "$recheck" ]; then echo '::error::an offered composition disappeared after testing'; exit 2; fi
        echo '::warning::Not releasing installer: offered compositions/signatures are not all published'
        echo 'available=false' >> "$GITHUB_OUTPUT"
        exit 0
    fi
    exit "$rc"
fi
bash .github/publication-tag.sh "$tag"
[[ "$expected" = - || "$expected" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
digest=$(bash .github/publish-head.sh digest "$repo:$tag")
if [ "$digest" = - ] || [ "$digest" != "$expected" ]; then
    echo "::error::installer target moved: expected $expected, current $digest"; exit 2
fi
cosign verify --key rootfs/etc/pki/containers/ilex.pub --insecure-ignore-tlog=true "$repo@$expected"
echo "installer target signature verified: $repo:$tag at $expected"
printf 'available=true\ndigest=%s\ntarget=%s:%s\n' "$expected" "$repo" "$tag" >> "$GITHUB_OUTPUT"
