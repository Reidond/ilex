//! Registry and CI plumbing shared by `changed`, `publish` and `iso`:
//! where images are published, how we authenticate, the CI-only guard and
//! `$GITHUB_OUTPUT`.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use ilex_devkit::cmd;

/// Image label holding the content fingerprint written by `xtask rechunk`.
pub const FINGERPRINT_LABEL: &str = "ilex.fingerprint";

/// Where machines get their updates from. ghcr needs lowercase names.
const DEFAULT_REGISTRY: &str = "ghcr.io";

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// True when running under CI (`CI=true` or `GITHUB_ACTIONS=true`).
pub fn is_ci() -> bool {
    ["CI", "GITHUB_ACTIONS"]
        .iter()
        .any(|n| matches!(std::env::var(n).as_deref(), Ok("true" | "1")))
}

/// Registry owner, lowercased: `$GITHUB_REPOSITORY_OWNER`.
fn owner() -> Option<String> {
    env_nonempty("GITHUB_REPOSITORY_OWNER").map(|o| o.to_ascii_lowercase())
}

/// `ghcr.io/<owner>/<name>` unless `explicit` is given.
pub fn repository(explicit: Option<&str>, name: &str) -> Result<String> {
    if let Some(r) = explicit {
        return Ok(r.trim_end_matches('/').to_string());
    }
    let Some(owner) = owner() else {
        bail!(
            "don't know which registry repository to use: GITHUB_REPOSITORY_OWNER is not set \
             (pass --repository, e.g. localhost:5000/{name})"
        );
    };
    Ok(format!("{DEFAULT_REGISTRY}/{owner}/{name}"))
}

/// Host part of `host[:port]/path`.
pub fn registry_host(repository: &str) -> &str {
    repository.split('/').next().unwrap_or(repository)
}

/// A plain-HTTP registry on this machine (local testing).
pub fn is_local_registry(repository: &str) -> bool {
    let host = registry_host(repository);
    host == "localhost"
        || host.starts_with("localhost:")
        || host == "127.0.0.1"
        || host.starts_with("127.0.0.1:")
        || host.starts_with("[::1]")
}

/// Publishing is CI-only. The one escape hatch is `--allow-local-registry`
/// together with a registry on this machine, so it can never reach ghcr.
pub fn require_ci(what: &str, repository: &str, allow_local_registry: bool) -> Result<()> {
    if is_ci() {
        return Ok(());
    }
    if allow_local_registry {
        if is_local_registry(repository) {
            eprintln!(
                "xtask: not in CI; {what} to the local registry {repository} (--allow-local-registry)"
            );
            return Ok(());
        }
        bail!(
            "--allow-local-registry only works with a registry on this machine \
             (localhost:PORT/...), not {repository}"
        );
    }
    bail!(
        "`{what}` only runs in CI (CI=true or GITHUB_ACTIONS=true); never publish from a dev \
         machine. To test against a throwaway local registry pass --allow-local-registry \
         --repository localhost:5000/ilex"
    )
}

/// Credentials for the registry, as a private auth file shared by podman,
/// skopeo and cosign. The token comes from `REGISTRY_TOKEN` and never
/// appears on a command line.
pub struct Auth {
    dir: PathBuf,
}

impl Auth {
    /// Log in to `repository`'s registry if `REGISTRY_TOKEN` is set.
    pub fn login(repository: &str) -> Result<Option<Auth>> {
        let Some(token) = env_nonempty("REGISTRY_TOKEN") else {
            return Ok(None);
        };
        let user = env_nonempty("REGISTRY_USER")
            .or_else(|| env_nonempty("GITHUB_ACTOR"))
            .unwrap_or_else(|| "token".to_string());

        let base = env_nonempty("RUNNER_TEMP")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("ilex-auth-{}", std::process::id()));
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        let auth = Auth { dir };

        let mut login = Command::new("podman");
        login
            .arg("login")
            .arg("--authfile")
            .arg(auth.authfile())
            .args(["--username", &user, "--password-stdin"]);
        if is_local_registry(repository) {
            login.arg("--tls-verify=false");
        }
        login.arg(registry_host(repository));
        eprintln!("$ {} (token on stdin)", cmd::display(&login));
        let mut child = login
            .stdin(Stdio::piped())
            .spawn()
            .context("spawning `podman login`")?;
        child
            .stdin
            .take()
            .context("podman login stdin")?
            .write_all(token.as_bytes())?;
        let status = child.wait()?;
        if !status.success() {
            bail!(
                "`podman login {}` failed with {status}",
                registry_host(repository)
            );
        }
        Ok(Some(auth))
    }

    fn authfile(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// Point a command at this auth file (podman/skopeo read
    /// `REGISTRY_AUTH_FILE`, cosign reads `DOCKER_CONFIG`).
    pub fn apply(&self, cmd: &mut Command) {
        cmd.env("REGISTRY_AUTH_FILE", self.authfile())
            .env("DOCKER_CONFIG", &self.dir);
    }
}

impl Drop for Auth {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Apply `auth` to `cmd` when there is one.
pub fn with_auth<'a>(auth: &Option<Auth>, cmd: &'a mut Command) -> &'a mut Command {
    if let Some(a) = auth {
        a.apply(cmd);
    }
    cmd
}

/// A finished command with both streams captured.
pub struct Captured {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Run to completion capturing stdout and stderr; a non-zero exit is not an
/// error here — the caller decides.
pub fn capture(cmd: &mut Command) -> Result<Captured> {
    let line = cmd::display(cmd);
    eprintln!("$ {line}");
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("spawning `{line}`"))?;
    Ok(Captured {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

/// Append `key=value` to `$GITHUB_OUTPUT` (when set) and echo it.
pub fn github_output(key: &str, value: &str) -> Result<()> {
    println!("{key}={value}");
    if let Some(path) = env_nonempty("GITHUB_OUTPUT") {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
            .with_context(|| format!("opening $GITHUB_OUTPUT ({path})"))?;
        writeln!(f, "{key}={value}")?;
    }
    Ok(())
}

/// Today's UTC date as `YYYYMMDD`, for the dated tags.
pub fn utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    format!("{y:04}{m:02}{d:02}")
}

/// Days since 1970-01-01 to (year, month, day); Howard Hinnant's algorithm.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `sha256:<hex>` check for a digest string.
pub fn is_digest(s: &str) -> bool {
    s.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
}
