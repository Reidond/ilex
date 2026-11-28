#!/bin/bash
# Print a directory of shims for cargo, rustc, rustup, rustdoc and kache that
# fail loudly. A process holding the signing key runs with it first on PATH:
# the publisher itself never starts them (it reads the closure recorded by
# build-publisher.sh), and this makes any regression a red step before a
# signature exists, not a compiler running with the key.
set -euo pipefail
dir=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/no-cargo.XXXXXX")
for tool in cargo rustc rustup rustdoc kache; do
    printf '#!/bin/sh\necho "::error::%s ran in a process that holds signing credentials; the signing path must not run cargo, rustc or kache" >&2\nexit 97\n' \
        "$tool" > "$dir/$tool"
    chmod +x "$dir/$tool"
done
echo "$dir"
