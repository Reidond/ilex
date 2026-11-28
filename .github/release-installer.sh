#!/bin/bash
# The exact downloaded ISOs passed their install trials against VERIFIED_DIGEST
# (the GNOME edition, ilex Glabra) and VERIFIED_DIGEST_KDE (the KDE edition, ilex
# Kingiana) in this job, and the encrypted gates installed through the first.
set -euo pipefail
if [ "${GITHUB_ACTIONS:-}" != true ] || [ "${GITHUB_REF:-}" != refs/heads/main ] ||
   [ "${GITHUB_REPOSITORY:-}" != Reidond/ilex ]; then
    echo '::error::installer releases require this repository on main in Actions'; exit 2
fi
case "${GITHUB_EVENT_NAME:-}" in push|schedule|workflow_dispatch) ;; *) exit 2 ;; esac
[ "${INSTALL_RESULT:-}" = success ] || { echo '::error::installer installation did not pass'; exit 2; }
: "${VERIFIED_DIGEST:?verified/tested installer digest required}" "${VERIFIED_DIGEST_KDE:?verified/tested KDE digest required}" "${ISO_DIR:?ISO directory required}"
GITHUB_OUTPUT=/dev/null bash .github/installer-target.sh "$VERIFIED_DIGEST"
GITHUB_OUTPUT=/dev/null INSTALLER_TAG="${INSTALLER_TAG_KDE:?KDE composition required}" bash .github/installer-target.sh "$VERIFIED_DIGEST_KDE"
(cd "$ISO_DIR" && sha256sum --check ilex-glabra-installer.iso.sha256 ilex-kingiana-installer.iso.sha256)
# The transition alias: the GNOME edition's exact bytes under the name the
# previous installer released, with its own checksum.
ln -f "$ISO_DIR/ilex-glabra-installer.iso" "$ISO_DIR/ilex-installer.iso" ||
    cp "$ISO_DIR/ilex-glabra-installer.iso" "$ISO_DIR/ilex-installer.iso"
printf '%s  ilex-installer.iso\n' "$(cut -d' ' -f1 "$ISO_DIR/ilex-glabra-installer.iso.sha256")" > "$ISO_DIR/ilex-installer.iso.sha256"
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
assets=()
for name in ilex-glabra-installer ilex-kingiana-installer ilex-installer; do
    assets+=("$ISO_DIR/$name.iso" "$ISO_DIR/$name.iso.sha256")
done
gh release create "$tag" "${assets[@]}" \
    --target "$GITHUB_SHA" --title "ilex installers ${base#installer-}" \
    --notes-file "$ISO_DIR/release-notes.md" --latest
for name in ilex-glabra-installer ilex-kingiana-installer ilex-installer; do
    echo "https://github.com/$GITHUB_REPOSITORY/releases/download/$tag/$name.iso"
    echo "https://github.com/$GITHUB_REPOSITORY/releases/latest/download/$name.iso"
done
