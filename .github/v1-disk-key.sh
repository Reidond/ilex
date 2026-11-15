#!/bin/bash
# Must match upgrade.rs's bcvk install options. The previous profile permitted
# GPT's ESP automount to cover OSTree's /boot bind mount.
set -euo pipefail
digest=${1:?v1 digest required}
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
[[ "$BCVK_VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 2
echo "v1-disk-gpt-auto-off-$BCVK_VERSION-${digest#sha256:}"
