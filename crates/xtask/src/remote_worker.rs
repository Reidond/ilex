//! Linux endpoint for one typed, versioned builder job.
use crate::remote::{Command, Job};
use anyhow::{Context, Result, ensure};
use ilex_devkit::{Workspace, core::SnapshotContext};
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    #[arg(long)]
    pub request: PathBuf,
}

pub fn run(args: Args) -> Result<()> {
    ilex_devkit::provider::require_arch(ilex_devkit::core::Architecture::X86_64)?;
    let job: Job = serde_json::from_slice(&std::fs::read(&args.request)?)?;
    ilex_devkit::core::check_version(job.schema)?;
    let ws = Workspace::discover()?;
    let context = ilex_devkit::source::remote_context(&ws.root)?
        .ok_or_else(|| anyhow::anyhow!("remote worker requires an installed snapshot context"))?;
    let expected = SnapshotContext {
        schema: 1,
        root: ws.root.clone(),
        identity: job.snapshot.identity,
        source: job.snapshot.source,
    };
    ensure!(
        serde_json::to_value(&context)? == serde_json::to_value(&expected)?,
        "remote request/context mismatch"
    );
    ilex_devkit::source::verify_materialized(&ws.root, &context.source)?;
    match job.command {
        Command::Build(a) => {
            let image = crate::build::build(&ws, &a)?;
            let immutable = ilex_devkit::bcvk::image_id(&image)?;
            let labels = ilex_devkit::cmd::output(std::process::Command::new("podman").args([
                "image",
                "inspect",
                "--format",
                "{{json .Config.Labels}}",
                &image,
            ]))?;
            let labels: serde_json::Value = serde_json::from_str(&labels)?;
            ensure!(
                labels["ilex.checkout-owner"] == context.identity.owner
                    && labels["ilex.source-digest"] == context.source.digest,
                "built image provenance mismatch"
            );
            std::fs::write(
                args.request.with_file_name("image.json"),
                serde_json::to_vec(
                    &serde_json::json!({"reference":image,"immutable_id":immutable,"labels":labels}),
                )?,
            )?;
            println!("{image}");
            Ok(())
        }
        Command::Rechunk(a) => crate::rechunk::run(&ws, a),
        Command::Disk(a) => {
            let disk = crate::disk::ensure(&ws, &a)?;
            let resolved = a.selection.resolve(&ws, a.image.as_deref(), None)?;
            let info = ilex_devkit::cmd::output(
                std::process::Command::new("qemu-img")
                    .args(["info", "--output=json"])
                    .arg(&disk),
            )?;
            let info: serde_json::Value = serde_json::from_str(&info)?;
            let format = match a.format {
                crate::disk::Format::Qcow2 => "qcow2",
                crate::disk::Format::Raw => "raw",
            };
            ensure!(
                info["format"] == format && info["virtual-size"].as_u64().is_some_and(|s| s > 0),
                "installed disk format/size mismatch"
            );
            ensure!(
                info["backing-filename"].is_null(),
                "installed base disk must be standalone"
            );
            use ilex_devkit::core::{self, BuildExecutor};
            let executor = ilex_devkit::executor::LocalBuildExecutor::new(ws.clone())?
                .probe()?
                .executor_id;
            let checksum = hash_file(&disk)?;
            let metadata = core::DiskArtifact {
                schema: 1, architecture: core::Architecture::X86_64,
                format: if a.format == crate::disk::Format::Qcow2 { core::DiskFormat::Qcow2 } else { core::DiskFormat::Raw },
                virtual_bytes: info["virtual-size"].as_u64().unwrap(),
                allocated_bytes: info["actual-size"].as_u64().context("disk allocation size missing")?,
                sector_bytes: 512,
                image: core::ImageIdentity { executor_id: executor.clone(), local_reference: resolved.image.clone(),
                    immutable_id: ilex_devkit::bcvk::image_id(&resolved.image)?,
                    registry_digest: Some(ilex_devkit::image::image_digest(&resolved.image)?).filter(|s| !s.is_empty()),
                    architecture: core::Architecture::X86_64 },
                install_recipe_hash: core::digest(serde_json::to_vec(&serde_json::json!({"bcvk":"0.22.0","format":format,"size":a.size,"firmware":"uefi"}))?.as_slice()),
                firmware: core::Firmware::Uefi,
                // The base remains unenrolled. The existing QEMU fixture injects
                // its public key through systemd credentials at each test boot.
                provisioning_hash: core::digest(b"unenrolled-base;qemu-systemd-ssh-credentials-v1"),
                location: core::ArtifactLocation { executor_id: executor, path: disk.clone(), sha256: checksum },
            };
            std::fs::write(
                disk.with_file_name(format!(
                    "{}.disk.json",
                    disk.file_name().unwrap().to_string_lossy()
                )),
                serde_json::to_vec_pretty(&metadata)?,
            )?;
            println!("{}", disk.display());
            Ok(())
        }
        Command::Iso(a) => crate::iso::run(&ws, a),
        Command::Tool(a) => crate::tool::run(&ws, a),
        Command::Test(a) => crate::test::run(&ws, a),
        Command::SelfCheck { hold_seconds } => {
            use std::process::Command;
            ilex_devkit::cmd::run(Command::new("flock").args([
                "--wait",
                "10800",
                "/run/lock/ilex-remote-smoke-admission.lock",
                "/usr/local/libexec/ilex-builder-smoke",
            ]))?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "schema": 1, "identity": ws.identity()?, "source": context.source,
                    "image": ws.image_ref(), "state_dir": ws.state_dir(), "nested_kvm_boot": true,
                }))?
            );
            std::thread::sleep(std::time::Duration::from_secs(hold_seconds.min(3600)));
            Ok(())
        }
    }
}

fn hash_file(path: &std::path::Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut hash = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buffer = [0; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}
