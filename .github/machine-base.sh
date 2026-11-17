#!/bin/bash
# registry BASE: config/labels only, no layer pull. Every composition parent
# was created by this scheme; an unlabelled parent is always a regression.
# files BASE CONFIG_DIGEST: after pulling, verify identity and required files.
# Every other failure is fatal, including podman's 125/126/127 probe errors.
set -euo pipefail
fail() { echo "::error::$*" >&2; exit 1; }
base=${2:?base required}
case ${1:-} in
    registry)
        inspect=(skopeo inspect)
        case "$base" in localhost:*|127.0.0.1:*|\[::1\]:*) inspect+=(--tls-verify=false) ;; esac
        raw=$("${inspect[@]}" --raw "docker://$base")
        id=$(jq -er '.config.digest' <<<"$raw")
        [[ "$id" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "invalid base config digest: $id"
        config=$("${inspect[@]}" --config "docker://$base")
        jq -e '.config | type == "object"' <<<"$config" >/dev/null || fail "invalid base config for $base"
        jq -e '(.config.Labels // {}) | type == "object"' <<<"$config" >/dev/null || fail "invalid base labels for $base"
        jq -e '.config.Labels | .["ilex.machine"] == "generic" and
            (.["ilex.shells"] | type == "string") and
            (.["ilex.default-shell"] | type == "string")' <<<"$config" >/dev/null ||
            fail "$base has missing/incompatible root configuration labels"
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
    *) fail 'usage: machine-base.sh registry BASE | files BASE CONFIG_DIGEST' ;;
esac
