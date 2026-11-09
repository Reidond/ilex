//! Keep machines that track `ghcr.io/reidond/ilex` on signed images.
//!
//! The image ships a `policy.json` that demands a cosign signature for
//! `ghcr.io/reidond/ilex` (plus `registries.d` and the public key), but bootc
//! only consults that policy for a deployment whose origin says so
//! (`bootc switch --enforce-container-sigpolicy`). A machine installed from
//! the ISO, or switched with a plain `bootc switch`, tracks ghcr *unverified*:
//! kickstart's `bootc` command has no way to pass the flag.
//!
//! `ilex-agent enforce-sigpolicy`, run at every boot by `ilex-sigpolicy.service`,
//! rewrites the origin in place (`bootc switch --mutate-in-place`: no
//! download, no new deployment, no reboot; /sysroot is remounted writable in a
//! private mount namespace for the call) when, and only when, the booted
//! deployment tracks this repository through a registry without enforcement.
//! Anything else (ephemeral VMs and containers with no deployment, test
//! images in containers-storage, other registries, machines already
//! enforcing) is left alone, so running it again is always safe.

use std::process::{Command, ExitCode};

use serde_json::Value;

/// Run under `unshare --mount` with the image reference as `$1`.
const SWITCH_SCRIPT: &str = concat!(
    "mount -o remount,rw /sysroot && ",
    "exec bootc switch --mutate-in-place --enforce-container-sigpolicy \"$1\""
);

/// The repository whose images are signed with the key in `policy.json`.
pub const REPOSITORY: &str = "ghcr.io/reidond/ilex";

/// What to do about the booted deployment.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// No booted deployment: an ephemeral VM, a container or a live system.
    NoDeployment,
    /// Tracks something that isn't `REPOSITORY` over the registry transport.
    Foreign(String),
    /// Tracks `REPOSITORY` and already verifies (container policy or an ostree
    /// remote that someone chose on purpose).
    Enforced,
    /// Tracks `REPOSITORY` with no verification: switch this reference in
    /// place with enforcement on.
    Enforce(String),
}

/// Decide from `bootc status --format=json`. Reads only the booted
/// deployment's image reference: `{image, transport, signature?}`.
pub fn decide(status: &Value) -> Decision {
    let booted = &status["status"]["booted"];
    if booted.is_null() {
        return Decision::NoDeployment;
    }
    let reference = &booted["image"]["image"];
    let (Some(image), Some(transport)) =
        (reference["image"].as_str(), reference["transport"].as_str())
    else {
        return Decision::Foreign("the booted deployment is not a container image".into());
    };
    if transport != "registry" {
        return Decision::Foreign(format!("{image} over {transport}, not a registry"));
    }
    if !is_ilex_repository(image) {
        return Decision::Foreign(format!("{image} is not in {REPOSITORY}"));
    }
    // Absent or "insecure" is no verification; "containerPolicy" and
    // {"ostreeRemote": ...} both verify.
    match &reference["signature"] {
        Value::Null => Decision::Enforce(image.to_string()),
        Value::String(s) if s == "insecure" => Decision::Enforce(image.to_string()),
        _ => Decision::Enforced,
    }
}

/// `ghcr.io/reidond/ilex`, optionally with `:tag` and/or `@digest`; not
/// `ghcr.io/reidond/ilex-iso` or anything else sharing the prefix.
fn is_ilex_repository(image: &str) -> bool {
    match image.strip_prefix(REPOSITORY) {
        Some(rest) => rest.is_empty() || rest.starts_with(':') || rest.starts_with('@'),
        None => false,
    }
}

/// `ilex-agent enforce-sigpolicy [--dry-run]`.
pub fn run(dry_run: bool) -> ExitCode {
    match enforce(dry_run) {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("ilex-agent: enforce-sigpolicy: {why}");
            ExitCode::FAILURE
        }
    }
}

fn enforce(dry_run: bool) -> Result<(), String> {
    match decide(&bootc_status()?) {
        Decision::NoDeployment => {
            println!("no booted deployment (not an installed bootc system): nothing to do");
        }
        Decision::Foreign(why) => println!("not an ilex registry image ({why}): nothing to do"),
        Decision::Enforced => println!("{REPOSITORY} is already verified against policy.json"),
        Decision::Enforce(image) if dry_run => {
            println!("would enforce signatures for {image} (dry run)");
        }
        Decision::Enforce(image) => {
            println!("{image} is tracked without signature verification: enforcing");
            // /sysroot is mounted read-only on a running system, which bootc
            // does not work around for --mutate-in-place ("Writing origin: Read-only
            // file system"). Remount it writable in a private mount namespace of
            // our own, so nothing else ever sees it writable.
            let out = Command::new("unshare")
                .args(["--mount", "--propagation", "private", "sh", "-c"])
                .arg(SWITCH_SCRIPT)
                .args(["ilex-sigpolicy", &image])
                .output()
                .map_err(|e| format!("running unshare: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "`bootc switch --mutate-in-place --enforce-container-sigpolicy {image}` (in a writable /sysroot namespace) failed ({}): {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            // Trust, but check what bootc recorded.
            match decide(&bootc_status()?) {
                Decision::Enforced => println!("{image} now verifies signatures"),
                other => return Err(format!("bootc did not record enforcement: {other:?}")),
            }
        }
    }
    Ok(())
}

fn bootc_status() -> Result<Value, String> {
    let out = Command::new("bootc")
        .args(["status", "--format=json"])
        .output()
        .map_err(|e| format!("running bootc: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "`bootc status` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("parsing `bootc status`: {e}"))
}

/// For `--self-check`: the decision table, over canned `bootc status` output,
/// since the interesting machines (installed, tracking ghcr) can't be
/// reached from an ephemeral VM.
pub fn check_decisions() -> Result<(), String> {
    use Decision::*;
    // (image, transport, signature as raw JSON) of the booted deployment.
    type Booted<'a> = (&'a str, &'a str, Option<&'a str>);
    let cases: [(&str, Option<Booted>, Decision); 11] = [
        ("ephemeral VM (no deployment)", None, NoDeployment),
        (
            "ghcr unverified",
            Some(("ghcr.io/reidond/ilex:latest", "registry", None)),
            Enforce("ghcr.io/reidond/ilex:latest".into()),
        ),
        (
            "ghcr by digest, unverified",
            Some(("ghcr.io/reidond/ilex@sha256:00", "registry", None)),
            Enforce("ghcr.io/reidond/ilex@sha256:00".into()),
        ),
        (
            "ghcr without a tag, unverified",
            Some(("ghcr.io/reidond/ilex", "registry", None)),
            Enforce("ghcr.io/reidond/ilex".into()),
        ),
        (
            "ghcr, signature explicitly insecure",
            Some((
                "ghcr.io/reidond/ilex:latest",
                "registry",
                Some("\"insecure\""),
            )),
            Enforce("ghcr.io/reidond/ilex:latest".into()),
        ),
        (
            "ghcr, container policy",
            Some((
                "ghcr.io/reidond/ilex:latest",
                "registry",
                Some("\"containerPolicy\""),
            )),
            Enforced,
        ),
        (
            "ghcr, ostree remote",
            Some((
                "ghcr.io/reidond/ilex:latest",
                "registry",
                Some("{\"ostreeRemote\":\"r\"}"),
            )),
            Enforced,
        ),
        (
            "local test image",
            Some(("localhost/ilex:wt-x", "containers-storage", None)),
            Foreign(String::new()),
        ),
        (
            "ghcr name over containers-storage",
            Some(("ghcr.io/reidond/ilex:latest", "containers-storage", None)),
            Foreign(String::new()),
        ),
        (
            "the ISO repository",
            Some(("ghcr.io/reidond/ilex-iso:latest", "registry", None)),
            Foreign(String::new()),
        ),
        (
            "another registry",
            Some(("quay.io/fedora/fedora-bootc:45", "registry", None)),
            Foreign(String::new()),
        ),
    ];
    for (name, booted, want) in cases {
        let signature =
            |s: Option<&str>| s.map_or(String::new(), |s| format!(",\"signature\":{s}"));
        let json = match booted {
            None => r#"{"status":{"booted":null}}"#.to_string(),
            Some((image, transport, sig)) => format!(
                r#"{{"status":{{"booted":{{"image":{{"image":{{"image":"{image}","transport":"{transport}"{}}}}}}}}}}}"#,
                signature(sig)
            ),
        };
        let status: Value = serde_json::from_str(&json).map_err(|e| format!("{name}: {e}"))?;
        let got = decide(&status);
        // `Foreign` carries a human-readable reason; only the kind matters.
        let same = match (&got, &want) {
            (Foreign(_), Foreign(_)) => true,
            _ => got == want,
        };
        if !same {
            return Err(format!("{name}: decided {got:?}, expected {want:?}"));
        }
    }
    Ok(())
}
