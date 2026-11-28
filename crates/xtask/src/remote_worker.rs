//! Linux endpoint for one typed, versioned builder job.
use crate::remote::{Command, Job};
use anyhow::{Result, ensure};
use ilex_devkit::core::SnapshotContext;
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
    let ws = crate::checkout::workspace()?;
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
            let (disk, image) = crate::disk::ensure(&ws, &a)?;
            crate::disk::write_metadata(&ws, &a, &disk, &image)?;
            println!("{}", disk.display());
            Ok(())
        }
        Command::Iso(_) => {
            // The Windows client refuses these first; a sender that does not
            // must not get an ISO it cannot collect.
            anyhow::bail!("{}", crate::iso::REMOTE_UNSUPPORTED)
        }
        Command::Tool(a) => crate::tool::run(&ws, a),
        Command::Test(a) => {
            ensure!(
                !a.filters.iter().any(|f| crate::iso::needs_iso(f)),
                "{}",
                crate::iso::REMOTE_UNSUPPORTED
            );
            crate::test::run(&ws, a)
        }
        Command::Vm(a) => crate::vm::run_remote(&ws, a),
        Command::ImportKache { .. } => {
            anyhow::bail!("cache import is handled by the builder runner")
        }
        #[cfg(target_os = "windows")]
        Command::Oemdrv { .. } => {
            anyhow::bail!("OEMDRV creation is handled by the builder runner")
        }
        Command::Export { image, output } => {
            ensure!(output.is_none(), "export destination must stay on Windows");
            let image = ws.resolve_image(image.as_deref());
            ilex_devkit::ownership::require_image(&ws, &image)?;
            let immutable = ilex_devkit::bcvk::image_id(&image)?;
            let directory = ws.ensure_state()?.join("exports");
            std::fs::create_dir_all(&directory)?;
            let temporary = directory.join(format!("{}.partial", job.job));
            ilex_devkit::cmd::run(
                std::process::Command::new("podman")
                    .args(["save", "--format", "oci-archive", "--output"])
                    .arg(&temporary)
                    .arg(&immutable),
            )?;
            let checksum = crate::disk::hash_file(&temporary)?;
            let archive = directory.join(format!("{}.oci.tar", &checksum[7..]));
            if archive.exists() {
                ensure!(
                    crate::disk::hash_file(&archive)? == checksum,
                    "archive cache checksum mismatch"
                );
                std::fs::remove_file(&temporary)?;
            } else {
                std::fs::rename(&temporary, &archive)?;
            }
            std::fs::write(archive.with_extension("sha256"), format!("{checksum}\n"))?;
            println!("{}", archive.display());
            Ok(())
        }
        Command::SelfCheck {
            hold_seconds,
            allocation_image,
            fingerprint_image,
        } => {
            use std::process::Command;
            if let Some(image) = fingerprint_image {
                ilex_devkit::ownership::require_image(&ws, &image)?;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&crate::doctor::fingerprint_report(&ws, &image)?)?
                );
                return Ok(());
            }
            if let Some(image) = allocation_image {
                ilex_devkit::ownership::require_image(&ws, &image)?;
                let opts = ilex_devkit::bcvk::EphemeralOpts {
                    allocation_self_check: true,
                    ..Default::default()
                };
                let _vm = ilex_devkit::bcvk::EphemeralVm::boot(
                    &ws.vm_name("allocation-self-check"),
                    &image,
                    &opts,
                )?;
                anyhow::bail!("allocation self-check unexpectedly passed its cancellation gate");
            }
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
