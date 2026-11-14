//! Building drupe tool images (`cargo xtask tool`, and the `tool::` tier's
//! test image).
//!
//! The work happens in a Fedora container running `tool-build.sh`: install
//! the packages into a scratch root, collect their commands with every
//! shared library they load, and pack that tree as a signed, verity-protected
//! disk image with `systemd-repart`. Nothing but podman is needed on the
//! host.
//!
//! Everything lands in `<state dir>/tools/`:
//!
//! * `images/<name>.raw`: the image, ready for `drupe install`.
//! * `images/drupe.crt`: the certificate a machine must have in
//!   `/etc/verity.d/` to accept it.
//! * `keys/drupe.key`, `keys/drupe.crt`: this checkout's development signing
//!   key, created by the first build. It never leaves the machine.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::{Workspace, cmd};

/// Same Fedora as the image, so a tool's libraries match what it was tested
/// with (not that it matters at run time: the tool brings its own).
const BUILDER_IMAGE: &str = "quay.io/fedora/fedora:45";
const SCRIPT: &str = include_str!("tool-build.sh");

/// A built tool image and the certificate that verifies it.
#[derive(Debug, Clone)]
pub struct Built {
    pub image: PathBuf,
    pub certificate: PathBuf,
}

/// `<state dir>/tools/images`: what a VM needs to see to install a tool.
pub fn images_dir(ws: &Workspace) -> PathBuf {
    ws.state_dir().join("tools").join("images")
}

/// Where [`build`] puts (or already put) the image called `name`.
pub fn built(ws: &Workspace, name: &str) -> Built {
    let images = images_dir(ws);
    Built {
        image: images.join(format!("{name}.raw")),
        certificate: images.join("drupe.crt"),
    }
}

/// Build the tool `name` from the commands of `packages`.
pub fn build(ws: &Workspace, name: &str, packages: &[String]) -> Result<Built> {
    cmd::require("podman")?;
    let valid = |s: &str| {
        !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
    };
    if !valid(name) {
        bail!("tool name `{name}` must be [A-Za-z0-9._+-]");
    }
    if packages.is_empty() || !packages.iter().all(|p| valid(p)) {
        bail!("package names must be [A-Za-z0-9._+-], and there must be at least one");
    }

    let images = images_dir(ws);
    let keys = ws.state_dir().join("tools").join("keys");
    for dir in [&images, &keys] {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }

    let mut run = Command::new("podman");
    run.args(["run", "--rm", "--interactive", "--security-opt", "label=disable"])
        .args(["--env", &format!("NAME={name}")])
        .args(["--env", &format!("PACKAGES={}", packages.join(" "))])
        .args(["--volume", &format!("{}:/out", images.display())])
        .args(["--volume", &format!("{}:/keys", keys.display())])
        .args([BUILDER_IMAGE, "bash", "-s"])
        .stdin(Stdio::piped());
    let line = cmd::display(&run);
    eprintln!("$ {line} < tool-build.sh");
    let mut child = run.spawn().with_context(|| format!("spawning `{line}`"))?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(SCRIPT.as_bytes())
        .context("sending tool-build.sh to the container")?;
    let status = child.wait().with_context(|| format!("waiting for `{line}`"))?;
    if !status.success() {
        bail!("building the tool image `{name}` failed with {status}");
    }

    let built = built(ws, name);
    fs::copy(keys.join("drupe.crt"), &built.certificate)
        .with_context(|| format!("copying the certificate to {}", built.certificate.display()))?;
    Ok(built)
}
