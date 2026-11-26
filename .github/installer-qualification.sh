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
# A job conclusion of `success` qualifies, including when the installer root was
# unchanged in this run (its tiers are skipped steps of a green job) or no v1
# exists (the upgrade tier has nothing to test). Anything else, or no verdict
# before the deadline, does not: the decision fails closed.
#
# Output (GITHUB_OUTPUT): qualified=true|false. Exit 1 only on a deadline.
# Environment: GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT, GH_TOKEN;
# QUALIFICATION_DEADLINE seconds (default 7500: the installer jobs are capped at
# 120 minutes each); QUALIFICATION_POLL seconds (default 15);
# QUALIFICATION_JOBS_FILE a saved jobs listing to read instead of the API
# (checks of this script).
set -euo pipefail

deadline=${QUALIFICATION_DEADLINE:-7500}
poll=${QUALIFICATION_POLL:-15}
started=$SECONDS

# The caller job is `pipeline` in publish.yml, so the installer root's jobs are
# `pipeline / build` and `pipeline / upgrade` (`build`/`upgrade` if the pipeline
# is ever run directly). Another root's jobs have longer names and never match.
fetch() {
    if [ -n "${QUALIFICATION_JOBS_FILE:-}" ]; then
        jq -c '.jobs[] | {name, status, conclusion, steps: [(.steps // [])[] | {name, conclusion}]}' "$QUALIFICATION_JOBS_FILE" | jq -s -c .
        return
    fi
    gh api --paginate \
        "repos/$GITHUB_REPOSITORY/actions/runs/$GITHUB_RUN_ID/attempts/$GITHUB_RUN_ATTEMPT/jobs?per_page=100" \
        --jq '.jobs[] | {name, status, conclusion, steps: [(.steps // [])[] | {name, conclusion}]}' | jq -s -c .
}

# One word per installer job: `success`, `pending` (not completed, or not
# listed yet) or the job's other conclusion (failure, timed_out, cancelled...).
state() { # jobs-json build|upgrade
    jq -r --arg job "$2" '
        [.[] | select(.name == $job or .name == "pipeline / \($job)")] | first
        | if . == null or .status != "completed" then "pending"
          elif .conclusion == "success" then "success"
          else (.conclusion // "unknown") end' <<<"$1"
}

step() { # jobs-json job step -> that step's conclusion, or "-"
    jq -r --arg job "$2" --arg step "$3" '
        [.[] | select(.name == $job or .name == "pipeline / \($job)")] | first
        | [(.steps // [])[] | select(.name == $step) | .conclusion][0] // "-"' <<<"$1"
}

verdict=
while :; do
    jobs=$(fetch 2>/dev/null) || jobs='[]'
    build=$(state "$jobs" build)
    upgrade=$(state "$jobs" upgrade)
    if [ "$build" = success ] && [ "$upgrade" = success ]; then
        verdict=true
    elif { [ "$build" != success ] && [ "$build" != pending ]; } ||
         { [ "$upgrade" != success ] && [ "$upgrade" != pending ]; }; then
        # One failed tier decides at once: no need to wait for the other.
        verdict=false
    fi
    [ -z "$verdict" ] || break
    if [ $((SECONDS - started)) -ge "$deadline" ]; then
        echo "::error::no installer qualification verdict after ${deadline}s (build: $build, upgrade: $upgrade); the signing of this root stays blocked"
        exit 1
    fi
    echo "waiting for the installer root's qualification (build: $build, upgrade: $upgrade)"
    sleep "$poll"
done

security=$(step "$jobs" build 'Qualify candidate secure firmware and persistent TPM')
tier=$(step "$jobs" upgrade 'e2e tests (upgrade)')
describe() { # step conclusion -> words
    case "$1" in
        success) echo passed ;;
        skipped) echo "not run (installer root unchanged, or no v1 to upgrade from)" ;;
        -) echo "not reached" ;;
        *) echo "$1" ;;
    esac
}
echo "installer qualification: build job $build, upgrade job $upgrade"
echo "  security::firmware (installer build): $(describe "$security")"
echo "  upgrade:: (installer upgrade job): $(describe "$tier")"
echo "  => qualified=$verdict"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        printf '### Installer qualification: %s\n\n' "$([ "$verdict" = true ] && echo met || echo 'NOT met')"
        printf -- '- installer `build` job: %s; `security::firmware`: %s\n' "$build" "$(describe "$security")"
        printf -- '- installer `upgrade` job: %s; `upgrade::`: %s\n' "$upgrade" "$(describe "$tier")"
    } >> "$GITHUB_STEP_SUMMARY"
fi
if [ "$verdict" = false ]; then
    echo "::warning title=Installer qualification not met::the installer root's build ($build) or upgrade ($upgrade) job did not succeed, so this root is not signed in this run (its own build and tests are unaffected)"
fi
echo "qualified=$verdict" >> "${GITHUB_OUTPUT:-/dev/null}"
