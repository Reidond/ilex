#!/bin/bash
# registry BASE CHANGED NAMES REPO: config/labels only, no layer pull. Exit 10
# is exclusively an unchanged, unlabelled base before any machine publication.
# files BASE CONFIG_DIGEST: after pulling, verify identity and required files.
# Every other failure is fatal, including podman's 125/126/127 probe errors.
set -euo pipefail
fail() { echo "::error::$*" >&2; exit 1; }
base=${2:?base required}
case ${1:-} in
    registry)
        changed=${3:?base changed flag required}
        names=${4:?validated machine names file required}
        repo=${5:?repository required}
        inspect=(skopeo inspect)
        case "$base" in localhost:*|127.0.0.1:*|\[::1\]:*) inspect+=(--tls-verify=false) ;; esac
        raw=$("${inspect[@]}" --raw "docker://$base")
        id=$(jq -er '.config.digest' <<<"$raw")
        [[ "$id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid base config digest: $id"
        config=$("${inspect[@]}" --config "docker://$base")
        jq -e '.config | type == "object"' <<<"$config" >/dev/null || fail "invalid base config for $base"
        jq -e '(.config.Labels // {}) | type == "object"' <<<"$config" >/dev/null || fail "invalid base labels for $base"
        if jq -e '(.config.Labels // {}) | has("ilex.machine")' <<<"$config" >/dev/null; then
            jq -e '.config.Labels | .["ilex.machine"] == "generic" and
                (.["ilex.shells"] | type == "string") and
                (.["ilex.default-shell"] | type == "string") and
                (.["ilex.design-systems"] | type == "string") and
                (.["ilex.default-design-system"] | type == "string")' <<<"$config" >/dev/null ||
                fail "$base has ilex.machine but incompatible configuration labels; refusing a legacy skip"
        else
            rc=$?
            [ "$rc" = 1 ] || fail "label probe failed for $base (exit $rc)"
            [ "$changed" = false ] || fail "newly built base $base has no ilex.machine label; this is a regression"
            while IFS= read -r machine; do
                # info already checked manifest equality; defend the tag boundary too.
                [[ "$machine" =~ ^[a-z][a-z0-9-]*$ ]] || fail "invalid machine name: $machine"
                case "$machine" in
                    generic|latest|candidate|main|rechunked|upgrade|base|sha*|wt-*) fail "reserved machine name: $machine" ;;
                esac
                error=$(mktemp)
                if "${inspect[@]}" --raw "docker://$repo:$machine" > /dev/null 2>"$error"; then
                    rm -f "$error"
                    fail "$repo:$machine already exists; unlabelled base $base is a regression, not a legacy skip"
                else
                    rc=$?
                    # Permission/transport errors are never evidence of absence.
                    if ! grep -qiE 'manifest unknown|name unknown' "$error"; then
                        cat "$error" >&2; rm -f "$error"
                        fail "checking $repo:$machine failed (exit $rc); cannot establish legacy eligibility"
                    fi
                    rm -f "$error"
                fi
            done < "$names"
            echo "::notice::Skipping machines: unchanged $base has no ilex.machine label and no published machine exists; publish a phase-0 base first." >&2
            exit 10
        fi
        echo "base_id=$id"
        ;;
    files)
        expected=${3:?config digest required}
        actual=$(podman image inspect --format '{{.Id}}' "$base")
        [ "sha256:${actual#sha256:}" = "$expected" ] ||
            fail "$base pulled image ID $actual differs from registry config digest $expected"
        for path in /usr/share/ilex/.generated.json /usr/share/ilex/.overlays.json /usr/share/ilex/machine.json; do
            if podman run --rm --network=none --entrypoint /usr/bin/test "$base" -s "$path"; then
                continue
            else
                rc=$?
                fail "$base required-file probe $path failed (exit $rc); missing metadata or podman failure, never a legacy skip"
            fi
        done
        ;;
    *) fail 'usage: machine-base.sh registry BASE CHANGED NAMES REPO | files BASE CONFIG_DIGEST' ;;
esac
