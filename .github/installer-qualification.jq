# The installer root's qualification verdict, from the run's jobs listing.
# One program for the gate (installer-qualification.sh) and the run's summary
# (roots-report), so that the summary never says "met" when the gate says false.
#
# Input: the jobs of one run attempt, as
#   [{name, status, conclusion, steps: [{name, conclusion, completed_at}]}]
# Output: {build, upgrade, verdict, why, security, tier}
#   build, upgrade  `success`, `pending` or the job's other conclusion
#   verdict         "true" | "false" | "pending"
#   why             one sentence; the evidence behind a decided verdict
#   security, tier  conclusion of `security::firmware`'s step and of the
#                   `upgrade::` step, `-` when the step is not listed
#
# A job conclusion alone is not evidence. "Re-run failed jobs" carries every
# job that succeeded in an earlier attempt into the new one, listed under the
# new attempt number but with its old steps and timestamps: an `upgrade` job
# that ended green in attempt 1 because the installer build failed before it
# staged anything (so it tested nothing) would otherwise qualify the candidate
# that attempt 2's build staged. The upgrade tier must therefore have passed
# *on the candidate staged in this attempt*: when the build's "Stage the
# candidate" step succeeded and a v1 exists (the upgrade job's "Wait for the
# candidate" step was not skipped), `e2e tests (upgrade)` must have succeeded
# and completed no earlier than the staging step. Step timestamps are what
# separates a carried-over job from a fresh one (`run_attempt` does not: a
# carried-over job reports the new attempt). The staging step skipped means the
# installer root is unchanged and nothing was staged to test; no v1 means the
# upgrade tier has nothing to test: both qualify, as before. A step the listing
# lacks decides nothing in favour of qualifying.
#
# The caller job is `pipeline` in publish.yml, so the installer root's jobs are
# `pipeline / build` and `pipeline / upgrade` (`build`/`upgrade` if the pipeline
# is ever run directly). Another root's jobs have longer names and never match.

def job($name):
    [.[] | select(.name == $name or .name == "pipeline / \($name)")] | first;

def state:
    if . == null or .status != "completed" then "pending"
    elif .conclusion == "success" then "success"
    else (.conclusion // "unknown") end;

def step($name):
    if . == null then null
    else [(.steps // [])[] | select(.name == $name)] | first end;

def concl:
    if . == null then "-" else (.conclusion // "-") end;

def when:
    if . == null then "" else (.completed_at // "") end;

def decided: . != "success" and . != "pending";

job("build") as $build
| job("upgrade") as $upgrade
| ($build | state) as $bs
| ($upgrade | state) as $us
| ($build | step("Stage the candidate")) as $stage
| ($upgrade | step("Wait for the candidate")) as $wait
| ($upgrade | step("e2e tests (upgrade)")) as $e2e
| ($build | step("Qualify candidate secure firmware and persistent TPM")) as $sec
| (if ($stage | concl) == "-" then
       {ok: false, why: "the installer build job lists no \"Stage the candidate\" step to judge the upgrade tier against"}
   elif ($stage | concl) == "skipped" then
       {ok: true, why: "the installer root staged no candidate in this run (unchanged), so there is no upgrade tier to wait for"}
   elif ($stage | concl) != "success" then
       {ok: false, why: "the installer build job's \"Stage the candidate\" step ended \($stage | concl)"}
   elif ($wait | concl) == "skipped" then
       {ok: true, why: "no v1 exists, so the upgrade tier has nothing to test"}
   elif ($e2e | concl) != "success" then
       {ok: false, why: "the candidate was staged but \"e2e tests (upgrade)\" did not pass on it (step: \($e2e | concl)); an upgrade job carried over from an earlier attempt, which tested nothing or another candidate, does not qualify it"}
   elif ($e2e | when) == "" or ($stage | when) == "" or ($e2e | when) < ($stage | when) then
       {ok: false, why: "\"e2e tests (upgrade)\" completed (\($e2e | when)) before this attempt's candidate was staged (\($stage | when)): it tested an older candidate"}
   else
       {ok: true, why: "the upgrade tier passed on the candidate staged in this attempt"}
   end) as $evidence
| {
    build: $bs,
    upgrade: $us,
    verdict:
        (if $bs == "success" and $us == "success" then
             (if $evidence.ok then "true" else "false" end)
         # One failed job decides at once: no need to wait for the other.
         elif ($bs | decided) or ($us | decided) then "false"
         else "pending" end),
    why:
        (if $bs == "success" and $us == "success" then $evidence.why
         elif ($bs | decided) then "the installer build job ended \($bs)"
         elif ($us | decided) then "the installer upgrade job ended \($us)"
         else "" end),
    security: ($sec | concl),
    tier: ($e2e | concl)
}
