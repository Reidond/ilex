#!/bin/bash
# No signing credentials in this step. Execute the resulting binary directly
# later: a different Cargo environment cannot cause compilation with the key.
#
# The key-bearing publisher never runs cargo, rustc or kache either, so
# everything it needs from them is computed here: the crate closure of the root
# image (`cargo metadata`, which starts rustc and so a RUSTC_WRAPPER, and may
# download crates) is recorded in a file the publisher reads. The record names
# the checkout's source digest, so a file from another tree is refused.
set -euo pipefail
cargo build --locked -p ilexctl
target=$(cargo metadata --no-deps --format-version 1 | jq -er .target_directory)
publisher="$target/debug/ilexctl"
test -x "$publisher"
echo "prebuilt publisher: $publisher"
echo "XTASK_PUBLISHER=$publisher" >> "$GITHUB_ENV"
crates="${RUNNER_TEMP:?runner directory required}/image-crates"
"$publisher" --ilexctl-in-checkout info --image-crates > "$crates"
echo "recorded image crates ($(grep -c '^crate ' "$crates") crates) without signing credentials: $crates"
echo "ILEX_IMAGE_CRATES=$crates" >> "$GITHUB_ENV"
