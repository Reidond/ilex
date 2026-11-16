//! Observable checkout/ownership checks, without creating or adopting images.
use anyhow::{Result, ensure};
use ilex_devkit::{
    Workspace, cmd,
    core::{self, Architecture, Capability},
    git,
};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Create two disposable Git checkouts, including a T3-shaped worktree
    #[arg(long)]
    pub self_check: bool,
    #[arg(long)]
    pub json: bool,
    /// Require a capability, e.g. firmware_enforcement, persistent_tpm, screenshot
    #[arg(long)]
    pub require: Option<String>,
    /// Explicit guest architecture; unsupported requests fail before mutation
    #[arg(long, value_parser = ["x86_64", "aarch64"])]
    pub arch: Option<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let capabilities = ilex_devkit::provider::capabilities("none", (1280, 800));
    if let Some(feature) = args.require {
        let feature: Capability = serde_json::from_value(serde_json::Value::String(feature))?;
        capabilities.require(feature)?;
    }
    if let Some(arch) = args.arch {
        ilex_devkit::provider::require_arch(if arch == "aarch64" {
            Architecture::Aarch64
        } else {
            Architecture::X86_64
        })?;
    }
    let evidence = if args.self_check {
        Some(self_check(ws)?)
    } else {
        None
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": core::SCHEMA_VERSION, "identity": ws.identity()?,
                "capabilities": capabilities, "self_check": evidence,
            }))?
        );
    } else {
        println!(
            "backend: {:?}; host architecture: {:?}",
            ilex_devkit::host_backend(),
            Architecture::host()?
        );
        println!("{}", serde_json::to_string_pretty(&capabilities)?);
        if let Some(evidence) = evidence {
            println!("{}", serde_json::to_string_pretty(&evidence)?);
        }
    }
    Ok(())
}

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn self_check(ws: &Workspace) -> Result<serde_json::Value> {
    let dir = std::env::temp_dir().join(format!(
        "ilex-contracts-self-check-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    // All resources live under this newly created fixture; Drop removes only it.
    fs::create_dir(&dir)?;
    let _fixture = Fixture(dir.clone());
    let main = dir.join("main checkout");
    let linked = dir.join(".t3/worktrees/ilex/main");
    let common = cmd::output(git::git(&ws.root).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))?;
    let head = git::head_sha(&ws.root)?;
    cmd::run(
        Command::new("git")
            .args(["clone", "--quiet", "--no-hardlinks", "--no-checkout"])
            .arg(common)
            .arg(&main),
    )?;
    cmd::run(
        Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["checkout", "--quiet", "--detach"])
            .arg(head),
    )?;
    cmd::run(
        Command::new("git")
            .arg("-C")
            .arg(&main)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&linked)
            .arg("HEAD"),
    )?;
    let a = Workspace::from_root(main.clone());
    let b = Workspace::from_root(linked.clone());
    let (clean, clean_source) = b.source_identity()?;
    let tracked = linked.join("image.toml");
    let mut bytes = fs::read(&tracked)?;
    bytes.extend_from_slice(b"\n# dirty self-check input\n");
    fs::write(&tracked, bytes)?;
    fs::write(
        linked.join("self-check-untracked.txt"),
        b"untracked input\n",
    )?;
    fs::remove_file(linked.join("packages.toml"))?;
    let (dirty, source) = b.source_identity()?;
    let main_identity = a.identity()?;
    ensure!(
        a.slug == "main" && b.slug != "main",
        "T3 worktree acquired the reserved main slug"
    );
    ensure!(
        a.image_ref() != b.image_ref()
            && a.vm_prefix() != b.vm_prefix()
            && a.state_dir() != b.state_dir(),
        "checkout resource collision"
    );
    ensure!(
        main_identity.owner != dirty.owner && main_identity.repository_id == dirty.repository_id,
        "checkout owner collision or repository identity mismatch"
    );
    ensure!(
        clean.owner == dirty.owner && source.dirty && clean_source.digest != source.digest,
        "dirty bytes did not change source provenance"
    );
    ensure!(
        b.source_identity()?.0 == dirty,
        "identity/digest is nondeterministic"
    );
    ensure!(
        source
            .entries
            .iter()
            .any(|e| e.path == "self-check-untracked.txt"),
        "untracked input missing"
    );
    ensure!(
        source
            .entries
            .iter()
            .any(|e| e.path == "packages.toml" && matches!(e.kind, core::SourceEntryKind::Deleted)),
        "tracked deletion missing"
    );
    ensure!(
        main_identity.authorize(&dirty.owner).is_err(),
        "foreign owner was accepted"
    );
    ensure!(
        core::check_version(core::SCHEMA_VERSION + 1).is_err(),
        "unknown schema accepted"
    );
    let rejection = ilex_devkit::provider::capabilities("none", (1280, 800))
        .require(Capability::FirmwareEnforcement)
        .unwrap_err()
        .to_string();
    let result = serde_json::json!({"result": "passed", "main": main_identity, "t3": dirty,
        "main_image": a.image_ref(), "t3_image": b.image_ref(), "clean_digest": clean_source.digest,
        "dirty_digest": source.digest, "unsupported_capability": rejection,
        "foreign_owner_rejected": true, "unknown_schema_rejected": true,
        "cleanup": "fixture removed on return; no image or VM created"});
    // Ensure the source checkout remains readable; no source mutation above.
    let _ = git::head_sha(&ws.root)?;
    fs::remove_dir_all(&dir)?;
    ensure!(!dir.exists(), "self-check fixture cleanup failed");
    Ok(result)
}
