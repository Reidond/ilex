//! Build a machine layer on an existing image, shared by xtask and config::.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::{Workspace, bcvk, cmd, config, workspace::check_tag_length};

/// Automatically created layers always belong to this checkout. Explicit bases
/// get a stable reference hash, including digest refs and other checkouts' tags.
pub fn image_ref(ws: &Workspace, base: &str, name: &str) -> Result<String> {
    config::validate_machine_name(name)?;
    let own = ws.image_ref();
    let prefix = if !base.contains('@')
        && (base == own
            || base
                .strip_prefix(&own)
                .is_some_and(|s| s.starts_with(".s-")))
    {
        base.to_owned()
    } else {
        let hash = format!("{:x}", Sha256::digest(base.as_bytes()));
        format!("{own}.b-{}", &hash[..16])
    };
    checked_output(format!("{prefix}.m-{name}"))
}

fn checked_output(image: String) -> Result<String> {
    check_tag_length(&image)?;
    if image.contains('@') {
        bail!("machine output must be a tag, not a digest reference");
    }
    Ok(image)
}

/// Read the actual image's machine, also for explicit references from CI.
pub fn image_machine(image: &str) -> Result<String> {
    if !bcvk::image_exists(image) {
        bail!("image {image} not found in local container storage; build or pull it first");
    }
    let name = cmd::output(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{index .Labels \"ilex.machine\"}}",
        image,
    ]))?;
    if name.is_empty() || name == "<no value>" {
        bail!("image {image} has no ilex.machine label; rebuild it with cargo xtask build");
    }
    config::validate_machine_name(&name)?;
    Ok(name)
}

/// Hash paths, entry types, modes, link targets and bytes, without following links.
/// Length framing makes distinct path/content boundaries unambiguous.
fn hash_tree(hash: &mut Sha256, root: &Path, path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let frame = |hash: &mut Sha256, bytes: &[u8]| {
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    frame(
        hash,
        path.strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/")
            .as_bytes(),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        hash.update(meta.permissions().mode().to_le_bytes());
    }
    if meta.is_symlink() {
        hash.update(b"link");
        frame(hash, fs::read_link(path)?.to_string_lossy().as_bytes());
    } else if meta.is_dir() {
        hash.update(b"dir");
        let mut entries = fs::read_dir(path)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        for entry in entries {
            hash_tree(hash, root, &entry)?;
        }
    } else if meta.is_file() {
        hash.update(b"file");
        frame(hash, &fs::read(path)?);
    } else {
        bail!("unsupported machine input {}", path.display());
    }
    Ok(())
}

pub fn build(
    ws: &Workspace,
    base: &str,
    directory: &Path,
    output: Option<&str>,
    podman_args: &[String],
) -> Result<String> {
    let name = directory
        .file_name()
        .and_then(|s| s.to_str())
        .context("machine directory must have a UTF-8 basename")?;
    config::validate_machine_name(name)?;
    if name == "generic" {
        bail!("generic is the base image, not a machine layer");
    }
    let image = match output {
        Some(tag) => checked_output(ws.resolve_image(Some(tag)))?,
        None => image_ref(ws, base, name)?,
    };
    if image == base {
        bail!("machine output must differ from its base image");
    }
    let base_machine = image_machine(base)?;
    if base_machine != "generic" {
        bail!(
            "cannot build a machine layer on {base}: base machine is `{base_machine}`; machines do not stack"
        );
    }
    let base_id = bcvk::image_id(base).with_context(|| format!("resolving machine base {base}"))?;
    let mut inputs = Sha256::new();
    inputs.update(b"ilex-machine-inputs v1\n");
    hash_tree(&mut inputs, directory, directory)?;
    for relative in [
        "machines/Containerfile",
        "build/compose.py",
        "build/image_tree.py",
        "build/install-packages.sh",
        "crates/ilex-devkit/src/machine.rs",
    ] {
        hash_tree(&mut inputs, &ws.root, &ws.root.join(relative))?;
    }
    for argument in podman_args {
        inputs.update((argument.len() as u64).to_le_bytes());
        inputs.update(argument.as_bytes());
    }
    let inputs = format!("sha256:{:x}", inputs.finalize());
    let key = format!("{base_id}-{inputs}");
    let current = cmd::probe(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{index .Labels \"ilex.machine-key\"}}|{{index .Labels \"ilex.machine\"}}",
        &image,
    ]));
    if current.as_deref() == Some(&format!("{key}|{name}")) && podman_args.is_empty() {
        eprintln!("machine {name} layer is up to date: {image}");
        return Ok(image);
    }
    // All shell/design labels are inherited from the actual base, never from
    // the checkout's defaults when --on selects a previously published image.
    let mut build = Command::new("podman");
    build
        .current_dir(&ws.root)
        .args([
            "build",
            "--layers",
            "--squash",
            "--file",
            "machines/Containerfile",
            "--tag",
            &image,
        ])
        .args([
            "--build-arg",
            &format!("BASE={base_id}"),
            "--build-arg",
            &format!("MACHINE={name}"),
        ])
        .args([
            "--build-context",
            &format!("machine={}", directory.display()),
        ])
        .args([
            "--label",
            &format!("ilex.machine={name}"),
            "--label",
            &format!("ilex.machine-key={key}"),
        ])
        .args(["--label", &format!("ilex.machine-inputs={inputs}")])
        .args(["--unsetlabel", "ilex.fingerprint"])
        .args(podman_args)
        .arg("build");
    cmd::run(build.stdout(std::io::stderr()))?;
    Ok(image)
}
