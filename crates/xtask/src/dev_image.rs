//! A separate, explicitly unpublished development layer. No production overlay
//! or publication catalogue includes these files. It works on any machine image.
use anyhow::{Context, Result, ensure};
use ilex_devkit::{Workspace, bcvk, cmd, core::digest, ownership::ImageTransaction};
use std::{fs, process::Command};

pub fn reference(ws: &Workspace, base: &str) -> String {
    format!("{}.dev-{}", ws.image_ref(), &digest(base.as_bytes())[7..23])
}

pub fn ensure(ws: &Workspace, base: &str) -> Result<String> {
    ws.verify_implicit_image(base)?;
    let base_id = bcvk::image_id(base)?;
    let files = [
        "Containerfile",
        "provision-ssh",
        "sshd.conf",
        "sshd.service.conf",
        "session.py",
    ];
    let mut inputs = base_id.into_bytes();
    inputs.extend(fs::read(ws.root.join("build/image_tree.py"))?);
    for file in files {
        inputs.extend(file.as_bytes());
        inputs.extend(fs::read(ws.root.join("dev-session").join(file))?);
    }
    let key = digest(&inputs);
    let image = reference(ws, base);
    let transaction = ImageTransaction::new(ws, &image)?;
    if cmd::probe(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{index .Labels \"ilex.dev-key\"}}",
        &image,
    ]))
    .as_deref()
        == Some(&key)
    {
        return Ok(image);
    }
    let identity = ws.identity()?;
    let source = ilex_devkit::source::manifest(ws)?;
    cmd::run(
        Command::new("podman")
            .args(["build", "--pull=never", "--build-arg"])
            .arg(format!("BASE={base}"))
            .arg("--build-context")
            .arg(format!("tooling={}", ws.root.join("build").display()))
            .arg("--label")
            .arg(format!("ilex.dev-key={key}"))
            .arg("--label")
            .arg(format!("ilex.checkout-owner={}", identity.owner))
            .arg("--label")
            .arg(format!("ilex.source-digest={}", source.digest))
            .args(["--tag", transaction.temporary()])
            .arg(ws.root.join("dev-session")),
    )?;
    ensure!(
        bcvk::image_exists(transaction.temporary()),
        "development build produced no image"
    );
    transaction
        .promote(ws)
        .context("promoting development composition")?;
    Ok(image)
}
