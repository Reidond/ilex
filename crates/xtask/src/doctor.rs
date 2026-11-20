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
mod build;

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Create two disposable Git checkouts, including a T3-shaped worktree
    #[arg(long)]
    pub self_check: bool,
    /// Linux: actual Podman builds, source races and ownership rejection
    #[arg(long)]
    pub self_check_build: bool,
    /// Report real local image/source fingerprints for a same-tree comparison
    #[arg(long)]
    pub self_check_fingerprint: Option<String>,
    #[arg(long)]
    pub json: bool,
    /// Require a capability, e.g. firmware_enforcement, persistent_tpm, screenshot
    #[arg(long)]
    pub require: Option<String>,
    /// Explicit guest architecture; unsupported requests fail before mutation
    #[arg(long, value_parser = ["x86_64", "aarch64"])]
    pub arch: Option<String>,
}

pub fn run(ws: &Workspace, args: Args, entry: crate::EntryPoint) -> Result<()> {
    let fingerprints = args
        .self_check_fingerprint
        .as_deref()
        .map(|image| fingerprint_report(ws, image))
        .transpose()?;
    let builds = if args.self_check_build {
        Some(build::run(ws, entry)?)
    } else {
        None
    };
    let capabilities = crate::info::capabilities(ws)?;
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
                "capabilities": capabilities, "self_check": evidence, "build_self_check": builds,
                "fingerprint_self_check": fingerprints,
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
        if let Some(builds) = builds {
            println!("{}", serde_json::to_string_pretty(&builds)?);
        }
        if let Some(fingerprints) = fingerprints {
            println!("{}", serde_json::to_string_pretty(&fingerprints)?);
        }
    }
    Ok(())
}

pub(crate) fn fingerprint_report(ws: &Workspace, image: &str) -> Result<serde_json::Value> {
    ensure!(
        cfg!(target_os = "linux"),
        "fingerprint self-check requires Linux"
    );
    let selection = crate::rechunk::image_selection(image)?;
    let source = ilex_devkit::source::manifest(ws)?;
    let root = ilex_devkit::fingerprint::root_inputs(ws, &selection)?;
    let rechunk = crate::rechunk::fingerprint(ws, image)?;
    // Independently reproduce the pre-T3 encoding for a clean source.
    let clean_legacy = if !source.dirty {
        use sha2::{Digest, Sha256};
        let mut legacy = Sha256::new();
        let mut current = Sha256::new();
        for path in ilex_devkit::fingerprint::selection_inputs(&selection) {
            legacy.update(format!(
                "tree {path} {}\n",
                git::tree_hash(&ws.root, &path)?
            ));
        }
        ilex_devkit::fingerprint::hash_inputs(
            ws,
            &mut current,
            ilex_devkit::fingerprint::selection_inputs(&selection),
        )?;
        ensure!(
            legacy.finalize() == current.finalize(),
            "clean source encoding changed"
        );
        true
    } else {
        false
    };
    Ok(
        serde_json::json!({"source_digest":source.digest,"dirty":source.dirty,"revision":source.revision,"selection":selection,"image_id":ilex_devkit::bcvk::image_id(image)?,"root_inputs":root,"rechunk_fingerprint":rechunk,"clean_legacy_encoding_matches":clean_legacy}),
    )
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
    let other = dir.join("second clone");
    cmd::run(
        Command::new("git")
            .args(["clone", "--quiet", "--no-hardlinks"])
            .arg(&main)
            .arg(&other),
    )?;
    let other_linked = dir.join("other/.t3/worktrees/ilex/main");
    cmd::run(
        git::git(&other)
            .args(["worktree", "add", "--quiet", "--detach"])
            .arg(&other_linked)
            .arg("HEAD"),
    )?;
    let c = Workspace::from_root(other_linked);
    let other_main = Workspace::from_root(other);
    ensure!(
        a.slug != "main" && other_main.slug != "main",
        "ordinary clone claimed canonical main"
    );
    ensure!(
        a.image_ref() != other_main.image_ref(),
        "two clones' primary checkouts collide"
    );
    ensure!(
        b.worktree == c.worktree && b.slug != c.slug && b.identity()?.owner != c.identity()?.owner,
        "same-named linked worktrees across clones collide"
    );
    ensure!(
        b.vm_prefix() != c.vm_prefix() && b.state_dir() != c.state_dir(),
        "same-name VM/state allocation collides"
    );
    let registry = ilex_devkit::main_claim::MainClaims::at(dir.join("main-claims"));
    let allocated = registry.allocate(a.identity()?, &main)?;
    let allocated_other = registry.allocate(other_main.identity()?, &other_main.root)?;
    ensure!(
        allocated.is_main && allocated.slug == "main" && !allocated_other.is_main,
        "automatic first-primary claim collided"
    );
    ensure!(
        registry.release_stale(&allocated.owner).is_err(),
        "live primary claim released"
    );
    ensure!(
        registry
            .release_stale(&other_main.identity()?.owner)
            .is_err(),
        "wrong owner released claim"
    );
    let racing_registry = dir.join("racing-claims");
    let contenders = [
        (a.identity()?, main.clone()),
        (other_main.identity()?, other_main.root.clone()),
    ];
    let raced = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let (id, path) = contenders[i % 2].clone();
                let registry = racing_registry.clone();
                scope.spawn(move || {
                    ilex_devkit::main_claim::MainClaims::at(registry).allocate(id, &path)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("claim worker panicked"))
            .collect::<Result<Vec<_>>>()
    })?;
    let winner = ilex_devkit::main_claim::MainClaims::at(racing_registry)
        .current()?
        .expect("claim winner missing");
    ensure!(
        raced
            .iter()
            .all(|id| id.is_main == (id.owner == winner.owner)),
        "concurrent first-primary claims collided"
    );
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
        allocated.slug == "main" && b.slug != "main" && !b.identity()?.is_main,
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
        "second_clone": c.identity()?, "same_worktree_name": b.worktree,
        "second_primary": other_main.identity()?,
        "main_image": a.image_ref(), "t3_image": b.image_ref(), "clean_digest": clean_source.digest,
        "dirty_digest": source.digest, "unsupported_capability": rejection,
        "foreign_owner_rejected": true, "unknown_schema_rejected": true,
        "cleanup": "fixture removed on return; no image or VM created"});
    let relocated = dir.join("relocated/main");
    fs::create_dir_all(relocated.parent().unwrap())?;
    cmd::run(
        git::git(&main)
            .args(["worktree", "move"])
            .arg(&linked)
            .arg(&relocated),
    )?;
    let moved = Workspace::from_root(relocated);
    ensure!(
        moved.worktree == b.worktree
            && moved.slug != b.slug
            && moved.identity()?.owner != dirty.owner,
        "reused worktree name at another canonical path collides"
    );
    let state = dir.join("owned-state");
    ilex_devkit::ownership::ensure_directory(&state, &dirty)?;
    ensure!(
        ilex_devkit::ownership::ensure_directory(&state, &c.identity()?).is_err(),
        "foreign tool/disk state adopted"
    );
    let legacy = dir.join("legacy-state");
    fs::create_dir(&legacy)?;
    fs::write(legacy.join("cached-tool.raw"), b"unlabelled")?;
    ensure!(
        ilex_devkit::ownership::ensure_directory(&legacy, &dirty).is_err(),
        "unlabelled state adopted"
    );
    let mut result = result;
    result["relocated_worktree"] = serde_json::to_value(moved.identity()?)?;
    result["foreign_and_unlabelled_state_rejected"] = true.into();
    result["automatic_main_claim"] = serde_json::to_value(&allocated)?;
    result["later_primary"] = serde_json::to_value(&allocated_other)?;
    ensure!(
        main.starts_with(&dir),
        "fixture primary escaped self-check root"
    );
    fs::remove_dir_all(&main)?;
    registry.release_stale(&allocated.owner)?;
    let reassigned = registry.allocate(other_main.identity()?, &other_main.root)?;
    ensure!(
        reassigned.is_main && reassigned.owner != allocated.owner,
        "stale-claim release did not permit reassignment"
    );
    result["stale_claim_release_and_reassignment"] = true.into();
    result["concurrent_main_claims_have_one_owner"] = true.into();
    // Ensure the source checkout remains readable; no source mutation above.
    let _ = git::head_sha(&ws.root)?;
    fs::remove_dir_all(&dir)?;
    ensure!(!dir.exists(), "self-check fixture cleanup failed");
    Ok(result)
}
