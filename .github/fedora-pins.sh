#!/bin/bash
# The two floating Fedora images every root's build starts from, pinned once per
# run so that every root of the run builds from the same boot chain.
#
#   fedora-pins.sh resolve
#       Print `bootc=<digest>` and `builder=<digest>` (lines for GITHUB_OUTPUT):
#       the manifest digests of the registry's current fedora-bootc:45 and
#       fedora:45. The `snapshot` job runs this once.
#   fedora-pins.sh pull BOOTC_DIGEST BUILDER_DIGEST
#       Pull both by digest and tag them locally as `:45`, which is how
#       `ilexctl` (BASE_IMAGE/BUILDER_IMAGE), the Containerfile and the rechunker
#       name them. Every root's job runs this before `changed --inputs-only` and
#       builds with `--no-pull`, so the root's inputs key names the pinned
#       digests, never whatever the tag points at when that job happens to start.
#
# Without it each root pulled the floating tags itself, and an upstream push
# between two roots' pulls could leave the installer root unchanged (its tiers
# skipped, the qualification gate true) while another root built and signed a
# new, untested boot chain.
#
# Keep the references in step with `BASE_IMAGE` and `BUILDER_IMAGE` in
# crates/ilex-tooling-linux/src/image.rs.
set -euo pipefail

bootc_repo=quay.io/fedora/fedora-bootc
builder_repo=quay.io/fedora/fedora
tag=45

valid() { # digest
    [[ $1 =~ ^sha256:[0-9a-f]{64}$ ]]
}

case "${1:-}" in
    resolve)
        command -v skopeo >/dev/null || { sudo apt-get update -qq && sudo apt-get install -y skopeo; }
        for name in bootc builder; do
            repo=${name}_repo
            digest=$(skopeo inspect --format '{{.Digest}}' "docker://${!repo}:$tag")
            valid "$digest" || { echo "::error::skopeo returned no digest for ${!repo}:$tag: '$digest'" >&2; exit 1; }
            echo "pinned ${!repo}:$tag = $digest" >&2
            echo "$name=$digest"
        done
        ;;
    pull)
        [ $# -eq 3 ] || { echo "usage: $0 pull BOOTC_DIGEST BUILDER_DIGEST" >&2; exit 2; }
        pins=("$2" "$3")
        repos=("$bootc_repo" "$builder_repo")
        for i in 0 1; do
            valid "${pins[$i]}" || { echo "::error::not a sha256 digest for ${repos[$i]}: '${pins[$i]}'" >&2; exit 1; }
        done
        for i in 0 1; do
            podman pull --quiet "${repos[$i]}@${pins[$i]}"
            podman tag "${repos[$i]}@${pins[$i]}" "${repos[$i]}:$tag"
            echo "pinned ${repos[$i]}:$tag = ${pins[$i]} (local image digest: $(podman image inspect --format '{{.Digest}}' "${repos[$i]}:$tag"))"
        done
        ;;
    *)
        echo "usage: $0 resolve | pull BOOTC_DIGEST BUILDER_DIGEST" >&2
        exit 2
        ;;
esac
