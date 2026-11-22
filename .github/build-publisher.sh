#!/bin/bash
# No signing credentials in this step. Execute the resulting binary directly
# later: a different Cargo environment cannot cause compilation with the key.
set -euo pipefail
cargo build --locked -p ilexctl
target=$(cargo metadata --no-deps --format-version 1 | jq -er .target_directory)
publisher="$target/debug/ilexctl"
test -x "$publisher"
echo "prebuilt publisher: $publisher"
echo "XTASK_PUBLISHER=$publisher" >> "$GITHUB_ENV"
