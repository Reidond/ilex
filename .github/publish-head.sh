#!/bin/bash
# Compare the run commit with a freshly resolved main head, immediately before
# invoking a publisher. Exit 3 means an overtaken run: green, no publication.
# Both arguments are data; callers resolve main with gh, local controls need no API.
set -euo pipefail
run=${1:?run commit required}
head=${2:?current main commit required}
if ! [[ "$run" =~ ^[0-9a-f]{40}$ && "$head" =~ ^[0-9a-f]{40}$ ]]; then
    echo '::error::publish head guard requires two full commit SHAs' >&2
    exit 2
fi
if [ "$run" != "$head" ]; then
    echo "::warning::Not publishing: run commit $run is no longer head of main ($head); a newer run publishes." >&2
    exit 3
fi
echo "publish head guard: run $run is current main $head"
