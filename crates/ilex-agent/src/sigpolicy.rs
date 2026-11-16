//! Keep machines that track `ghcr.io/reidond/ilex` on signed images.
//!
//! The image ships a `policy.json` that demands a cosign signature for
//! `ghcr.io/reidond/ilex` (plus `registries.d` and the public key). Unverified
//! pulls still apply that policy; origin enforcement also refuses a singleton
//! insecureAcceptAnything default in the selected policy. An ISO install, or a
//! plain `bootc switch`, tracks ghcr *unverified*: kickstart's `bootc`
//! command has no way to pass `--enforce-container-sigpolicy`.
//!
//! `ilex-agent enforce-sigpolicy`, run at every boot by `ilex-sigpolicy.service`,
//! rewrites the origin in place (`bootc switch --mutate-in-place`: no
//! download, no new deployment, no reboot; /sysroot is remounted writable in a
//! private mount namespace for the call) when, and only when, the booted
//! deployment tracks this repository through a registry without enforcement.
//! Anything else (ephemeral VMs and containers with no deployment, test
//! images in containers-storage, other registries, machines already
//! enforcing) is left alone, so running it again is always safe.

use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Deserialize;
use serde_json::Value;

/// Run under `unshare --mount` with the image reference as `$1`.
const SWITCH_SCRIPT: &str = concat!(
    "mount -o remount,rw /sysroot && ",
    "exec bootc switch --mutate-in-place --enforce-container-sigpolicy \"$1\""
);

/// The repository whose images are signed with the key in `policy.json`.
pub const REPOSITORY: &str = "ghcr.io/reidond/ilex";

/// Match the resolver in Fedora 45's bootc 1.17.1 and 1.17.4 source RPMs:
/// crates/ostree-ext/src/container/skopeo.rs::resolve_policy_path. bootc opens
/// all paths relative to /, including relative environment overrides.
fn open_policy() -> Result<(PathBuf, File), String> {
    let rooted = |path| Path::new("/").join(path);
    if let Some(path) = env::var_os("CONTAINERS_POLICY_JSON").filter(|p| !p.is_empty()) {
        let path = rooted(path);
        let file = File::open(&path)
            .map_err(|e| format!("opening CONTAINERS_POLICY_JSON={}: {e}", path.display()))?;
        return Ok((path, file));
    }
    let config = env::var_os("XDG_CONFIG_HOME")
        .map(rooted)
        .or_else(|| env::var_os("HOME").map(|home| rooted(home).join(".config")));
    let user = config.map(|path| path.join("containers/policy.json"));
    for path in user.into_iter().chain([
        PathBuf::from("/etc/containers/policy.json"),
        PathBuf::from("/usr/share/containers/policy.json"),
    ]) {
        // Like bootc, fall through on any open error for non-override paths.
        if let Ok(file) = File::open(&path) {
            return Ok((path, file));
        }
    }
    Err(
        "no containers policy.json found in CONTAINERS_POLICY_JSON, user config or system paths"
            .into(),
    )
}

/// Enforcing origins cannot pull with a singleton insecureAcceptAnything
/// default in the selected policy, even with a repository signature rule.
pub fn check_policy() -> Result<(), String> {
    let (path, file) = open_policy()?;
    let policy: ContainerPolicy =
        serde_json::from_reader(file).map_err(|e| format!("parsing {}: {e}", path.display()))?;
    if policy.is_default_insecure() {
        return Err(format!(
            "{}: default is insecureAcceptAnything; refusing enforcement because bootc would refuse every update",
            path.display()
        ));
    }
    Ok(())
}

#[derive(Deserialize)]
struct PolicyEntry {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct ContainerPolicy {
    default: Option<Vec<PolicyEntry>>,
}

impl ContainerPolicy {
    /// Mirror bootc's predicate: requirements are ANDed, so only a singleton
    /// insecureAcceptAnything default triggers its enforcing-pull ban.
    fn is_default_insecure(&self) -> bool {
        matches!(
            self.default.as_deref(),
            Some([requirement]) if requirement.kind == "insecureAcceptAnything"
        )
    }
}

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
        Decision::Enforced => {
            check_policy()?;
            println!("{REPOSITORY} is already verified against policy.json");
        }
        Decision::Enforce(image) => {
            check_policy()?;
            if dry_run {
                println!("would enforce signatures for {image} (dry run)");
                return Ok(());
            }
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

/// For `--self-check`: the decision table and unsafe policy defaults,
/// over canned `bootc status` output and policy documents,
/// since the interesting machines (installed, tracking ghcr) can't be
/// reached from an ephemeral VM.
pub fn check_decisions() -> Result<(), String> {
    use Decision::*;
    // (image, transport, signature as raw JSON) of the booted deployment.
    type Booted<'a> = (&'a str, &'a str, Option<&'a str>);
    let cases: [(&str, Option<Booted>, Decision); 14] = [
        (
            "installer composition",
            Some(("ghcr.io/reidond/ilex:generic.gnome", "registry", None)),
            Enforce("ghcr.io/reidond/ilex:generic.gnome".into()),
        ),
        (
            "bare base composition",
            Some(("ghcr.io/reidond/ilex:base", "registry", None)),
            Enforce("ghcr.io/reidond/ilex:base".into()),
        ),
        ("ephemeral VM (no deployment)", None, NoDeployment),
        (
            "ms-7c35 machine, unverified",
            Some(("ghcr.io/reidond/ilex:ms-7c35.gnome", "registry", None)),
            Enforce("ghcr.io/reidond/ilex:ms-7c35.gnome".into()),
        ),
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
                "ghcr.io/reidond/ilex:generic.gnome",
                "registry",
                Some("\"insecure\""),
            )),
            Enforce("ghcr.io/reidond/ilex:generic.gnome".into()),
        ),
        (
            "ghcr, container policy",
            Some((
                "ghcr.io/reidond/ilex:generic.gnome",
                "registry",
                Some("\"containerPolicy\""),
            )),
            Enforced,
        ),
        (
            "ghcr, ostree remote",
            Some((
                "ghcr.io/reidond/ilex:generic.gnome",
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
            Some((
                "ghcr.io/reidond/ilex:generic.gnome",
                "containers-storage",
                None,
            )),
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
    for (policy, insecure) in [
        (r#"{"default":[{"type":"reject"}]}"#, false),
        (
            r#"{"default":[{"type":"sigstoreSigned","keyPath":"/etc/pki/containers/ilex.pub"}]}"#,
            false,
        ),
        (r#"{"default":[{"type":"insecureAcceptAnything"}]}"#, true),
        (
            r#"{"default":[{"type":"reject"},{"type":"insecureAcceptAnything"}]}"#,
            false,
        ),
        (
            r#"{"default":[{"type":"insecureAcceptAnything"},{"type":"sigstoreSigned","keyPath":"/etc/pki/containers/ilex.pub"}]}"#,
            false,
        ),
        (
            r#"{"default":[{"type":"insecureAcceptAnything"},{"type":"insecureAcceptAnything"}]}"#,
            false,
        ),
        (r#"{"default":[]}"#, false),
        (r#"{"default":null}"#, false),
        (r#"{}"#, false),
    ] {
        let value: ContainerPolicy = serde_json::from_str(policy).map_err(|e| e.to_string())?;
        if value.is_default_insecure() != insecure {
            return Err(format!("unexpected enforcement safety for {policy}"));
        }
    }
    Ok(())
}
