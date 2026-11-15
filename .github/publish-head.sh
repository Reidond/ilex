#!/bin/bash
# image RUN REPO TAG, or revision RUN REPO PUBLISHED_SHA_OR_DASH (installer).
# 0: forward publication; 3: green skip; 2: unreadable state or rewritten history.
# PATH-injected gh/skopeo provide local controls without registry/API writes.
set -euo pipefail
fail() { echo "::error::$*" >&2; exit 2; }
# digest TAG: read an immutable snapshot; '-' means explicit registry absence.
# swap TAG EXPECTED: compare immediately before publication (serialized jobs).
# These modes need no GitHub API, so local registry controls use real reads.
image_digest() {
    local tag=$1 digest error
    local inspect=(skopeo inspect --retry-times 3 --format '{{.Digest}}')
    case "$tag" in localhost:*|127.0.0.1:*|\[::1\]:*) inspect+=(--tls-verify=false) ;; esac
    error=$(mktemp)
    if digest=$("${inspect[@]}" "docker://$tag" 2>"$error"); then
        rm -f "$error"
        [[ "$digest" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid registry digest for $tag: $digest"
        echo "$digest"
    else
        if ! grep -qiE 'manifest unknown|name unknown' "$error"; then
            cat "$error" >&2; rm -f "$error"
            fail "cannot read published digest of $tag"
        fi
        rm -f "$error"
        echo -
    fi
}
case ${1:-} in
    digest) image_digest "${2:?image tag required}"; exit 0 ;;
    swap)
        tag=${2:?image tag required}; expected=${3:-}
        [[ "$expected" = - || "$expected" =~ ^sha256:[0-9a-f]{64}$ ]] ||
            fail "missing/invalid tested registry snapshot for $tag: $expected"
        current=$(image_digest "$tag")
        if [ "$current" != "$expected" ]; then
            echo "::warning::Not publishing $tag: tested registry snapshot $expected differs from current $current; start a new full run instead of replaying a stale digest." >&2
            exit 3
        fi
        echo "publish digest swap: $tag expected=$expected current=$current; allow"
        exit 0 ;;
esac
mode=${1:-}; run=${2:-}; repo=${3:-}; target=${4:-}
[[ "$run" =~ ^[0-9a-f]{40}$ && "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] ||
    fail 'publish ancestry guard requires a full run SHA and owner/repository'
head=$(gh api "repos/$repo/git/ref/heads/main" --jq .object.sha) || fail 'cannot read main head'
[[ "$head" =~ ^[0-9a-f]{40}$ ]] || fail "invalid main head: $head"
published=-
case "$mode" in
    image)
        [ -n "$target" ] || fail 'image tag required'
        inspect=(skopeo inspect --config)
        case "$target" in localhost:*|127.0.0.1:*|\[::1\]:*) inspect+=(--tls-verify=false) ;; esac
        error=$(mktemp)
        if config=$("${inspect[@]}" "docker://$target" 2>"$error"); then
            rm -f "$error"
            jq -e 'type == "object" and (.config | type == "object") and
                (.config.Labels == null or (.config.Labels | type == "object"))' <<<"$config" >/dev/null ||
                fail "unreadable config/labels on $target"
            if jq -e '(.config.Labels // {}) | has("org.opencontainers.image.revision")' <<<"$config" >/dev/null; then
                published=$(jq -er '.config.Labels["org.opencontainers.image.revision"] | select(type == "string")' <<<"$config") ||
                    fail "unreadable revision label on $target"
                [[ "$published" =~ ^[0-9a-f]{40}$ ]] || fail "invalid published revision on $target: $published"
            fi
        else
            # Only explicit registry absence permits a first publish.
            if ! grep -qiE 'manifest unknown|name unknown' "$error"; then
                cat "$error" >&2; rm -f "$error"
                fail "cannot read published revision from $target"
            fi
            rm -f "$error"
        fi
        ;;
    revision) published=$target; target=installer ;;
    *) fail 'usage: publish-head.sh image|revision RUN REPO TAG|PUBLISHED_SHA_OR_DASH' ;;
esac
[[ "$published" = - || "$published" =~ ^[0-9a-f]{40}$ ]] ||
    fail "invalid published revision on $target: $published"

# GitHub documents BASE...HEAD (equivalent to git log BASE..HEAD):
# https://docs.github.com/en/rest/commits/commits#compare-two-commits
# Its REST OpenAPI commit-comparison status enum is ahead/behind/identical/diverged.
# HEAD ahead of BASE, or identical, proves BASE is an ancestor. No shallow git.
compare() {
    local status
    status=$(gh api "repos/$repo/compare/$1...$2?per_page=1" --jq .status) ||
        fail "cannot compare $1...$2 (published=$published run=$run main=$head)"
    case "$status" in ahead|behind|identical|diverged) echo "$status" ;;
        *) fail "invalid comparison status $status for $1...$2" ;; esac
}
if [ "$published" != - ]; then
    lineage=$(compare "$published" "$head")
    case "$lineage" in ahead|identical) ;;
        *) fail "$target published revision $published is not an ancestor of main $head (run $run, $lineage); refusing rewritten/unrelated history. Owner recovery: see CLAUDE.md#published-history-recovery (merge history, or remove the affected package version/installer release); no override." ;; esac
fi
upper=$(compare "$run" "$head")
case "$upper" in ahead|identical) ;;
    *) echo "::warning::Not publishing $target: run $run is not an ancestor of main $head ($upper); published=$published" >&2; exit 3 ;; esac
lower=first-publish
if [ "$published" != - ]; then
    lower=$(compare "$published" "$run")
    case "$lower" in ahead|identical) ;;
        *) echo "::warning::Not publishing $target: published $published is not an ancestor of run $run ($lower); main=$head" >&2; exit 3 ;; esac
fi
echo "publish ancestry guard: $target published=$published run=$run main=$head; published-to-run=$lower run-to-main=$upper; allow"
