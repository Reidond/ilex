#!/bin/bash
# The exact downloaded ISO passed iso:: against VERIFIED_DIGEST in this job.
set -euo pipefail
if [ "${GITHUB_ACTIONS:-}" != true ] || [ "${GITHUB_REF:-}" != refs/heads/main ] ||
   [ "${GITHUB_REPOSITORY:-}" != Reidond/ilex ]; then
    echo '::error::installer releases require this repository on main in Actions'; exit 2
fi
case "${GITHUB_EVENT_NAME:-}" in push|schedule|workflow_dispatch) ;; *) exit 2 ;; esac
[ "${INSTALL_RESULT:-}" = success ] || { echo '::error::installer installation did not pass'; exit 2; }
: "${VERIFIED_DIGEST:?verified/tested installer digest required}" "${ISO_DIR:?ISO directory required}"
GITHUB_OUTPUT=/dev/null bash .github/installer-target.sh "$VERIFIED_DIGEST"
(cd "$ISO_DIR" && sha256sum --check ilex-installer.iso.sha256)
published=$(bash .github/installer-revision.sh "$GITHUB_REPOSITORY")
if bash .github/publish-head.sh revision "$GITHUB_SHA" "$GITHUB_REPOSITORY" "$published"; then :; else
    rc=$?
    if [ "$rc" = 3 ]; then exit 0; fi
    exit "$rc"
fi
base="installer-$(date -u +%Y%m%d)"
tag=$base
n=2
while gh release view "$tag" >/dev/null 2>&1; do tag="$base-$n"; n=$((n + 1)); done
gh release create "$tag" "$ISO_DIR/ilex-installer.iso" "$ISO_DIR/ilex-installer.iso.sha256" \
    --target "$GITHUB_SHA" --title "ilex installer ${base#installer-}" \
    --notes-file "$ISO_DIR/release-notes.md" --latest
echo "https://github.com/$GITHUB_REPOSITORY/releases/download/$tag/ilex-installer.iso"
echo "https://github.com/$GITHUB_REPOSITORY/releases/latest/download/ilex-installer.iso"
