#!/bin/bash
# Plain image install: no fixture kargs. Keep separate from the old gpt-auto-off
# profile; legacy v1's running mount is repaired only after its unmodified boot.
set -euo pipefail
digest=${1:?v1 digest required}
[[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || exit 2
[[ "$BCVK_VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || exit 2
echo "v1-disk-boot-mask-$BCVK_VERSION-${digest#sha256:}"
