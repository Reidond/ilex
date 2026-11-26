#!/bin/bash
# The shared qualification gate of every root that is not the installer's.
#
# The installer root's `build` job (with the security::firmware tier) and its
# `upgrade` job exercise the boot chain and the update path that every root
# shares, so a non-installer root's signing (and its machine layers') waits for
# both to have succeeded in this run. Only the signing waits: such a root's own
# build and tests never do. The decision is read from the jobs API, which is how
# `upgrade` already waits for `build`; nothing here can fail the installer root.
#
# The decision is made from evidence, not from job conclusions alone (see
# installer-qualification.jq, shared with the run's summary): a job carried
# over by "Re-run failed jobs" keeps its old conclusion, so a green `upgrade`
# job that tested nothing in attempt 1 must not qualify the candidate staged in
# attempt 2. Both jobs `success` qualifies when the installer root was unchanged
# in this run (its tiers are skipped steps of a green job), when no v1 exists
# (the upgrade tier has nothing to test), or when `e2e tests (upgrade)` passed
# after the candidate was staged. Anything else, or no verdict before the
# deadline, does not: the decision fails closed.
#
# Output (GITHUB_OUTPUT): qualified=true|false. Exit 1 only on a deadline.
# Environment: GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT, GH_TOKEN;
# QUALIFICATION_DEADLINE seconds (default 7500: the installer jobs are capped at
# 120 minutes each); QUALIFICATION_POLL seconds (default 60: every root's gate
# polls the run's jobs with the repository's shared token);
# QUALIFICATION_JOBS_FILE a saved jobs listing to read instead of the API
# (checks of this script).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
deadline=${QUALIFICATION_DEADLINE:-7500}
poll=${QUALIFICATION_POLL:-60}
started=$SECONDS
errors=0
last_error=
errfile=$(mktemp)
trap 'rm -f "$errfile"' EXIT

fetch() {
    if [ -n "${QUALIFICATION_JOBS_FILE:-}" ]; then
        jq -c '.jobs[] | {name, status, conclusion, steps: [(.steps // [])[] | {name, conclusion, completed_at}]}' "$QUALIFICATION_JOBS_FILE" | jq -s -c .
        return
    fi
    gh api --paginate \
        "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT/jobs?per_page=100" \
        --jq '.jobs[] | {name, status, conclusion, steps: [(.steps // [])[] | {name, conclusion, completed_at}]}' | jq -s -c .
}

describe() { # step conclusion -> words
    case "$1" in
        success) echo passed ;;
        skipped) echo skipped ;;
        -) echo "not listed" ;;
        *) echo "$1" ;;
    esac
}

result='{}'
verdict=
while :; do
    # An API error is not a verdict: keep polling (the decision fails closed
    # at the deadline), but say so instead of hiding it.
    if jobs=$(fetch 2>"$errfile"); then
        errors=0
    else
        jobs='[]'
        errors=$((errors + 1))
        last_error=$(head -c 300 "$errfile" | tr '\n' ' ' | sed 's/ *$//')
        echo "jobs API request failed (${errors} in a row): ${last_error:-no message}"
        if [ "$errors" -eq 5 ]; then
            echo "::warning title=Installer qualification: jobs API keeps failing::${errors} requests in a row failed (${last_error:-no message}); still polling, and no verdict means this root is not signed"
        fi
    fi
    result=$(jq -c -f "$here/installer-qualification.jq" <<<"$jobs")
    build=$(jq -r .build <<<"$result")
    upgrade=$(jq -r .upgrade <<<"$result")
    verdict=$(jq -r .verdict <<<"$result")
    [ "$verdict" != pending ] && break
    if [ $((SECONDS - started)) -ge "$deadline" ]; then
        echo "::error::no installer qualification verdict after ${deadline}s (build: $build, upgrade: $upgrade${last_error:+; last jobs API error: $last_error}); the signing of this root stays blocked"
        exit 1
    fi
    echo "waiting for the installer root's qualification (build: $build, upgrade: $upgrade)"
    sleep "$poll"
done

why=$(jq -r .why <<<"$result")
security=$(jq -r .security <<<"$result")
tier=$(jq -r .tier <<<"$result")
echo "installer qualification: build job $build, upgrade job $upgrade"
echo "  security::firmware (installer build): $(describe "$security")"
echo "  upgrade:: (installer upgrade job): $(describe "$tier")"
echo "  evidence: $why"
echo "  => qualified=$verdict"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        printf '### Installer qualification: %s\n\n' "$([ "$verdict" = true ] && echo met || echo 'NOT met')"
        printf -- '- installer `build` job: %s; `security::firmware`: %s\n' "$build" "$(describe "$security")"
        printf -- '- installer `upgrade` job: %s; `upgrade::`: %s\n' "$upgrade" "$(describe "$tier")"
        printf -- '- evidence: %s\n' "$why"
    } >> "$GITHUB_STEP_SUMMARY"
fi
if [ "$verdict" = false ]; then
    echo "::warning title=Installer qualification not met::$why. This root is not signed in this run (its own build and tests are unaffected). A job carried over from an earlier attempt qualifies nothing: re-run all jobs, or re-run the installer root's upgrade job"
fi
echo "qualified=$verdict" >> "${GITHUB_OUTPUT:-/dev/null}"
