#!/bin/bash
# Defence at shell/output boundaries; info --publications validates membership
# and manifests before returning any rows. Keep the component rules in config.rs.
set -euo pipefail
fail() { echo "::error::invalid publication tag: ${1:-}" >&2; exit 2; }
tag=${1:-}
[[ "$tag" =~ ^[a-z][a-z0-9-]*(\.[a-z][a-z0-9-]*)*$ ]] || fail "$tag"
[ "${#tag}" -le 116 ] || fail "$tag" # room for _sha-<7 hex>
[ "$tag" != base ] || exit 0
IFS=. read -r -a names <<< "$tag"
case "${names[0]}" in
    latest|candidate*|main|rechunked|upgrade|base|sha*|wt-*) fail "$tag" ;;
    generic) [ "${#names[@]}" -gt 1 ] || fail "$tag" ;;
esac
for ((i=1; i<${#names[@]}; i++)); do
    [ "${names[i]}" != none ] || fail "$tag"
    for ((j=1; j<i; j++)); do
        [ "${names[i]}" != "${names[j]}" ] || fail "$tag"
    done
done
