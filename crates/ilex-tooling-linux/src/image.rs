//! Local Podman image recipe, preserving the existing build and cache rules.
use crate::{Workspace, cmd};
use anyhow::{Context, Result, bail};
use ilex_tooling_core::{BuildRequest, ImageBuildOptions};
use std::{path::PathBuf, process::Command};
const KACHE_HOST_MOUNT: &str = "/kache-host";
pub const BASE_IMAGE: &str = "quay.io/fedora/fedora-bootc:45";
pub const BUILDER_IMAGE: &str = "quay.io/fedora/fedora:45";
// Keep headless Flatpak for preinstall cleanup after a shell-free switch.
// Required libraries and regenerated SELinux policy exceed 128 MiB;
// recommendations stay in shells and app payloads remain in /var.
const BASE_BUDGET: u64 = 256 * 1024 * 1024;
pub fn build(
    ws: &Workspace,
    request: &BuildRequest,
    options: &ImageBuildOptions,
) -> Result<String> {
    let image = options.output.clone();
    let selection = request.selection.clone();
    let bare_base = selection.as_ref().is_some_and(|s| s.shells.is_empty());
    if !options.no_pull {
        for base in [BASE_IMAGE, BUILDER_IMAGE] {
            cmd::run(
                Command::new("podman")
                    .args(["pull", "--quiet", base])
                    .stdout(std::io::stderr()),
            )?;
        }
    }
    let base_digest = image_digest(BASE_IMAGE)
        .with_context(|| format!("resolving the digest of {BASE_IMAGE}"))?;
    let base_size = if bare_base {
        Some(image_size(BASE_IMAGE)?)
    } else {
        None
    };
    let revision = request.checkout.source_revision.clone();
    let identity = &request.checkout;
    let source = &request.source;
    let root_inputs = selection
        .as_ref()
        .map(|s| crate::fingerprint::root_inputs(ws, s))
        .transpose()?;

    let mut build = Command::new("podman");
    build
        .current_dir(&ws.root)
        .args([
            "build",
            "--layers",
            "--file",
            "Containerfile",
            "--tag",
            &image,
        ])
        .args(["--label", &format!("ilex.base-digest={base_digest}")])
        .args([
            "--label",
            &format!("ilex.checkout-owner={}", identity.owner),
        ])
        .args(["--label", &format!("ilex.source-digest={}", source.digest)])
        .args(["--label", "ilex.architecture=x86_64"])
        .args([
            "--label",
            &format!("org.opencontainers.image.revision={revision}"),
        ])
        .args(["--label", "org.opencontainers.image.title=ilex"])
        .args([
            "--label",
            "org.opencontainers.image.source=https://github.com/Reidond/ilex",
        ]);
    if let Some(inputs) = root_inputs {
        build.args(["--label", &format!("ilex.root-inputs={inputs}")]);
    }
    if let Some(selection) = &selection {
        build.args([
            "--build-arg",
            &format!(
                "ILEX_SHELLS={}",
                if selection.shells.is_empty() {
                    "none".into()
                } else {
                    selection.shells.join(",")
                }
            ),
        ]);
        build.args([
            "--build-arg",
            &format!(
                "ILEX_DEFAULT_SHELL={}",
                selection.default_shell.as_deref().unwrap_or("")
            ),
        ]);
        for (key, value) in selection.labels() {
            build.args(["--label", &format!("{key}={value}")]);
        }
    }
    if let Some(store) = host_kache_store().filter(|_| !options.no_host_kache) {
        // The Windows builder validates this selection before it supplies
        // the checkout-owned copy. Let Podman label that copy for SELinux.
        let mount_options = if std::env::var_os("ILEX_KACHE_IMPORT").is_some()
            && crate::source::remote_context(&ws.root)?.is_some()
        {
            "ro,z"
        } else {
            "ro"
        };
        build.args([
            "--volume",
            &format!("{}:{KACHE_HOST_MOUNT}:{mount_options}", store.display()),
        ]);
    }
    crate::ownership::validate_build_args(&options.podman_args)?;
    build
        .args(&options.podman_args)
        .args([
            "--label",
            &format!("ilex.checkout-owner={}", identity.owner),
        ])
        .args(["--label", &format!("ilex.source-digest={}", source.digest)])
        .arg(".");
    cmd::run(build.stdout(std::io::stderr()))?;
    if let Some(base_size) = base_size {
        let size = image_size(&image)?;
        let growth = size.saturating_sub(base_size);
        if growth > BASE_BUDGET {
            // Name the tag twice: `podman untag image` alone removes all its
            // tags. A rejected build must not be reused by the next test run.
            cmd::run(Command::new("podman").args(["untag", &image, &image]))
                .with_context(|| format!("removing over-budget base tag {image}"))?;
            bail!(
                "base size budget exceeded: {image} is {size} bytes ({:.2} MiB), \
                 fedora-bootc is {base_size} bytes ({:.2} MiB); \
                 growth {:.2} MiB exceeds the 256 MiB budget. \
                 Move packages to a shell or machine, or justify a budget change in a commit",
                size as f64 / 1048576.0,
                base_size as f64 / 1048576.0,
                growth as f64 / 1048576.0
            );
        }
        eprintln!(
            "base size: {:.2} MiB, fedora-bootc {:.2} MiB, growth {:.2} MiB (budget 256 MiB)",
            size as f64 / 1048576.0,
            base_size as f64 / 1048576.0,
            growth as f64 / 1048576.0
        );
    }
    eprintln!("built {image} (base {base_digest}, revision {revision})");
    Ok(image)
}

fn image_size(image: &str) -> Result<u64> {
    cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Size}}", image]))?
        .parse()
        .with_context(|| format!("reading the size of {image}"))
}

/// This machine's kache store, if it has one a build can read: the directory
/// kache itself uses (`KACHE_CACHE_DIR`, else `$XDG_CACHE_HOME/kache`, else
/// `~/.cache/kache`), which counts once it holds an `index.db`.
fn host_kache_store() -> Option<PathBuf> {
    let dir = [
        std::env::var_os("KACHE_CACHE_DIR").map(PathBuf::from),
        std::env::var_os("XDG_CACHE_HOME").map(|d| PathBuf::from(d).join("kache")),
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache").join("kache")),
    ]
    .into_iter()
    .flatten()
    .find(|d| !d.as_os_str().is_empty())?;
    dir.join("index.db").is_file().then_some(dir)
}

/// Digest of a local image, e.g. `sha256:…`.
pub fn image_digest(image: &str) -> Result<String> {
    cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Digest}}", image]))
}

/// True if `image` exists in local container storage.
pub fn image_exists(image: &str) -> bool {
    Command::new("podman")
        .args(["image", "exists", image])
        .status()
        .is_ok_and(|s| s.success())
}
