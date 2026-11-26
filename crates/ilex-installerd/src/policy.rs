//! The signature check that comes before anything is written.
//!
//! `bootc install to-filesystem --enforce-container-sigpolicy` verifies the
//! image against the installer's `policy.json` when it pulls, which is after the
//! disk has been repartitioned. The person was promised that an unsigned image
//! or one signed with another key is refused with nothing written, so the
//! pinned reference goes through the same policy engine first: `skopeo copy`
//! (containers-image, the code bootc itself calls) checks the policy and the
//! signatures *before* it fetches a layer, so the copy is stopped as soon as it
//! starts one, and what it fetched is thrown away. A rejection is final; a
//! network failure says so and is not taken for a rejection.

use crate::{
    exec::{self, Flow, Job},
    scratch::Scratch,
};
use ilex_installer_model::plan::{ErrorCode, PlanError, PlannedImage};
use std::{path::Path, time::Duration};

const TIMEOUT: Duration = Duration::from_secs(120);

/// What the policy engine said about an image.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The policy accepts it: `skopeo` went on to fetch layers.
    Accepted,
    /// The policy refused it (no signature, a signature that does not verify,
    /// or an image the policy does not allow), with the engine's own words.
    Rejected(String),
    /// Nothing was decided: the registry or the network failed.
    Unreachable(String),
}

/// Read `skopeo copy`'s output as it comes and decide as early as the words
/// allow. containers-image checks the policy and the signatures before it
/// prints anything, and then says `Getting image source signatures`; only a
/// layer or the config fetched after that line, with no error before it, is
/// acceptance. A "rejected" error is a refusal; any other failure is not a
/// verdict. Output is merged (standard output and error): an error line ends
/// every chance of acceptance, however many `Copying blob` lines follow it.
pub struct Judge {
    verdict: Option<Verdict>,
    /// The policy engine has let the image through and the copy has begun.
    signatures: bool,
    /// An error line was printed.
    errored: bool,
    last_error: String,
}

impl Judge {
    pub fn new() -> Self {
        Self {
            verdict: None,
            signatures: false,
            errored: false,
            last_error: String::new(),
        }
    }

    pub fn line(&mut self, text: &str) {
        if self.verdict.is_some() {
            return;
        }
        let lower = text.to_ascii_lowercase();
        if lower.contains("rejected") || lower.contains("no signature exists") {
            self.verdict = Some(Verdict::Rejected(text.to_owned()));
        } else if lower.starts_with("error")
            || lower.contains("fatal")
            || lower.contains("level=error")
        {
            self.errored = true;
            self.last_error = text.to_owned();
        } else if lower.starts_with("getting image source signatures") {
            self.signatures = true;
        } else if (lower.starts_with("copying blob") || lower.starts_with("copying config"))
            && self.signatures
            && !self.errored
        {
            self.verdict = Some(Verdict::Accepted);
        }
    }

    pub fn decided(&self) -> bool {
        self.verdict.is_some()
    }

    /// The verdict once the tool has ended: a decision already made stands;
    /// else a failure is the registry's (or the network's), and a success
    /// accepts the image only if the tool said the copy had begun and printed
    /// no error. Anything else is no verdict, which never lets an image in.
    pub fn finish(self, success: bool) -> Verdict {
        match self.verdict {
            Some(verdict) => verdict,
            None if success && self.signatures && !self.errored => Verdict::Accepted,
            None if success => Verdict::Unreachable(if self.last_error.is_empty() {
                "skopeo finished without saying that the policy accepted the image".into()
            } else {
                self.last_error
            }),
            None => Verdict::Unreachable(if self.last_error.is_empty() {
                "skopeo failed without saying why".into()
            } else {
                self.last_error
            }),
        }
    }
}

impl Default for Judge {
    fn default() -> Self {
        Self::new()
    }
}

/// Ask the policy engine about `image.source` (a `repository@sha256:...`
/// reference). Fetches a few kilobytes at most: nothing is kept.
pub fn verify(image: &PlannedImage, run_dir: &Path) -> Result<(), PlanError> {
    let internal = |why: String| PlanError::new(ErrorCode::Internal, Some("composition"), why);
    let scratch = Scratch::new(run_dir, "policy").map_err(|e| internal(format!("{e:#}")))?;
    let destination = format!("dir:{}", scratch.path().join("copy").display());
    let source = format!("docker://{}", image.source);
    let mut judge = Judge::new();
    let outcome = exec::run_job(
        &Job {
            program: "skopeo",
            args: &["copy", "--retry-times", "2", &source, &destination],
            env: &[("TMPDIR", "/var/tmp"), ("HOME", "/root")],
            stdin: None,
            timeout: TIMEOUT,
        },
        &mut |line| {
            judge.line(line);
            // A decision is all this run is for: stop before more is fetched.
            if judge.decided() {
                Flow::Stop
            } else {
                Flow::Continue
            }
        },
        &mut || {},
    );
    // Whatever the copy fetched before it was stopped goes with the scratch
    // directory (a tree made by another program: removed recursively).
    let _ = std::fs::remove_dir_all(scratch.path().join("copy"));
    let verdict = match outcome {
        Ok(exit) => judge.finish(exit.success()),
        Err(e) => return Err(internal(format!("{e:#}"))),
    };
    match verdict {
        Verdict::Accepted => Ok(()),
        Verdict::Rejected(why) => Err(PlanError::new(
            ErrorCode::Signature,
            Some("composition"),
            format!(
                "{} is not accepted by the installer's signature policy; nothing was written: {}",
                image.source,
                exec::clean_line(why.as_bytes())
            ),
        )),
        Verdict::Unreachable(why) => Err(PlanError::new(
            ErrorCode::Registry,
            Some("composition"),
            format!(
                "could not check the signature of {}: {}",
                image.source,
                exec::clean_line(why.as_bytes())
            ),
        )),
    }
}
