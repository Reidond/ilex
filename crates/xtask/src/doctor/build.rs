//! Real local-store regressions, deliberately cheap recipes rather than VM mocks.
use anyhow::{Result, bail};
use ilex_devkit::Workspace;

#[cfg(not(target_os = "linux"))]
pub(super) fn run(_: &Workspace) -> Result<serde_json::Value> {
    bail!("build self-check requires the Linux Podman backend")
}

#[cfg(target_os = "linux")]
pub(super) fn run(ws: &Workspace) -> Result<serde_json::Value> {
    use anyhow::{Context, ensure};
    use ilex_devkit::{bcvk, cmd, git, ownership};
    use std::{
        fs,
        path::Path,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    struct Images {
        tags: Vec<String>,
        ids: Vec<String>,
    }
    impl Drop for Images {
        fn drop(&mut self) {
            for tag in &self.tags {
                if bcvk::image_id(tag).is_ok_and(|id| self.ids.contains(&id)) {
                    let _ = Command::new("podman")
                        .args(["untag", tag, tag])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }
    fn command(ws: &Workspace, root: &Path, sub: &str) -> Command {
        // Each fixture runs the sources in its own checkout.
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let mut c = Command::new(cargo);
        c.current_dir(&ws.root)
            .env("CARGO_TARGET_DIR", root.join("target").join(&ws.slug))
            .env("ILEX_ROOT", &ws.root)
            .env("ILEX_STATE_DIR", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env_remove("ILEX_IMAGE")
            .env_remove("ILEX_MACHINE")
            .env_remove("ILEX_SHELLS")
            .env_remove("ILEX_VM_SLUG")
            .env_remove("ILEX_VM")
            .arg("ilexctl")
            .arg(sub);
        if sub == "vm" {
            c.arg("run");
        }
        c
    }
    fn expect_rejection(c: &mut Command, sub: &str, reason: &str) -> Result<()> {
        let out = c.output()?;
        let stderr = String::from_utf8_lossy(&out.stderr);
        ensure!(
            out.status.code() != Some(2),
            "self-check {sub} probe has invalid CLI usage: {stderr}"
        );
        ensure!(
            out.status.code() == Some(1) && stderr.contains(reason),
            "self-check {sub} probe expected rejection ({reason}), got {}: {stderr}",
            out.status
        );
        Ok(())
    }
    fn build(
        ws: &Workspace,
        root: &Path,
        recipe: &Path,
        gate: &Path,
        log: &Path,
    ) -> Result<Child> {
        let mut c = command(ws, root, "build");
        c.args(["--no-pull", "--no-host-kache", "--", "--no-cache", "--file"])
            .arg(recipe)
            .arg("--volume")
            .arg(format!("{}:/gate:rw", gate.display()));
        let file = fs::File::create(log)?;
        Ok(c.stdout(file.try_clone()?).stderr(file).spawn()?)
    }
    fn gated(child: &mut Child, gate: &Path, edit: impl FnOnce() -> Result<()>) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !gate.join("started").exists() {
            ensure!(
                child.try_wait()?.is_none(),
                "build exited before source-race gate"
            );
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("build gate timed out");
            }
            thread::sleep(Duration::from_millis(100));
        }
        let edited = edit();
        fs::write(gate.join("release"), b"release")?;
        edited?;
        ensure!(!child.wait()?.success(), "changed source was accepted");
        Ok(())
    }
    let root = std::env::temp_dir().join(format!(
        "ilex-build-self-check-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&root)?;
    let _fixture = super::Fixture(root.clone());
    let common = cmd::output(git::git(&ws.root).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))?;
    let head = git::head_sha(&ws.root)?;
    let mut checkouts = Vec::new();
    for name in ["a", "b"] {
        let primary = root.join(name);
        cmd::run(
            Command::new("git")
                .args(["clone", "--quiet", "--no-hardlinks", "--no-checkout"])
                .arg(&common)
                .arg(&primary),
        )?;
        cmd::run(
            git::git(&primary)
                .args(["checkout", "--quiet", "--detach"])
                .arg(&head),
        )?;
        let linked = root.join(format!("{name}-linked/.t3/worktrees/ilex/same-name"));
        cmd::run(
            git::git(&primary)
                .args(["worktree", "add", "--quiet", "--detach"])
                .arg(&linked)
                .arg("HEAD"),
        )?;
        checkouts.push(Workspace::from_root(linked));
    }
    let a = &checkouts[0];
    let b = &checkouts[1];
    ensure!(
        a.worktree == b.worktree && a.slug != b.slug,
        "two clones share a resource namespace"
    );
    let recipe = root.join("Containerfile");
    fs::write(
        &recipe,
        "FROM quay.io/fedora/fedora:45\nCOPY packages.toml /source-input\nRUN if test -f /gate/hold; then touch /gate/started; until test -f /gate/release; do sleep 0.2; done; fi\n",
    )?;
    let gate = root.join("gate");
    fs::create_dir(&gate)?;
    let mut images = Images {
        tags: vec![a.image_ref(), b.image_ref()],
        ids: Vec::new(),
    };
    for fixture in [a, b] {
        let log = root.join(format!("{}.log", fixture.slug));
        ensure!(
            build(fixture, &root, &recipe, &gate, &log)?
                .wait()?
                .success(),
            "baseline build failed: {}",
            fs::read_to_string(log)?
        );
        ownership::require_image(fixture, &fixture.image_ref())?;
        images.ids.push(bcvk::image_id(&fixture.image_ref())?);
    }
    let accepted = bcvk::image_id(&a.image_ref())?;
    let accepted_digest = cmd::output(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{index .Config.Labels \"ilex.source-digest\"}}",
        &a.image_ref(),
    ]))?;
    let tracked = a.root.join("packages.toml");
    let original = fs::read(&tracked)?;
    let mut race_logs = Vec::new();
    for inventory_error in [false, true] {
        for name in ["started", "release"] {
            let _ = fs::remove_file(gate.join(name));
        }
        fs::write(gate.join("hold"), b"hold")?;
        let log = root.join(if inventory_error {
            "inventory-error.log"
        } else {
            "source-edit.log"
        });
        let mut child = build(a, &root, &recipe, &gate, &log)?;
        gated(&mut child, &gate, || {
            if inventory_error {
                fs::write(
                    a.root.join("rootfs/self-check-private.key"),
                    b"not a secret",
                )?;
            } else {
                let mut changed = original.clone();
                changed.extend_from_slice(b"\n# edited while Podman runs\n");
                fs::write(&tracked, changed)?;
            }
            Ok(())
        })?;
        let output = fs::read_to_string(&log)?;
        ensure!(
            output.contains(if inventory_error {
                "private-named source input"
            } else {
                "source changed while Podman"
            }),
            "wrong rejection: {output}"
        );
        ensure!(
            bcvk::image_id(&a.image_ref())? == accepted,
            "rejected build replaced accepted final image"
        );
        let job_tag = output
            .split_whitespace()
            .find(|s| s.starts_with("localhost/ilex-job:"))
            .context("job tag missing from build log")?;
        ensure!(
            !bcvk::image_exists(job_tag),
            "rejected job tag was retained: {job_tag}"
        );
        race_logs.push(serde_json::json!({"case": if inventory_error {"inventory_failure"} else {"tracked_edit"}, "rejected": true, "final_id": accepted, "temporary_alias_removed": true}));
        fs::write(&tracked, &original)?;
        let _ = fs::remove_file(a.root.join("rootfs/self-check-private.key"));
    }
    // Simulate a collision/foreign alias even though owner-qualified allocation
    // makes it unnecessary in normal operation. Selection labels still match.
    cmd::run(Command::new("podman").args(["tag", &a.image_ref(), &b.image_ref()]))?;
    let mut rejected = Vec::new();
    for sub in ["test", "disk", "vm", "build"] {
        let mut c = command(b, &root, sub);
        if sub == "test" {
            c.arg("ephemeral::");
        }
        if sub == "build" {
            c.arg("--no-pull");
        }
        expect_rejection(&mut c, sub, "another checkout")?;
        ensure!(
            bcvk::image_id(&b.image_ref())? == accepted,
            "{sub} replaced foreign image"
        );
        rejected.push(sub);
    }
    cmd::run(Command::new("podman").args(["tag", &images.ids[1], &b.image_ref()]))?;
    // A deliberately unlabelled legacy fixture must also be refused.
    let legacy_recipe = root.join("legacy.Containerfile");
    fs::write(&legacy_recipe, "FROM scratch\nLABEL fixture=unlabelled\n")?;
    cmd::run(
        Command::new("podman")
            .args(["build", "--quiet", "--file"])
            .arg(&legacy_recipe)
            .args(["--tag", &b.image_ref()])
            .arg(&root)
            .stdout(Stdio::from(std::io::stderr())),
    )?;
    images.ids.push(bcvk::image_id(&b.image_ref())?);
    expect_rejection(
        command(b, &root, "build").arg("--no-pull"),
        "build",
        "unlabelled legacy image",
    )?;
    let legacy_id = bcvk::image_id(&b.image_ref())?;
    let rebuilt = command(b, &root, "identity")
        .args([
            "rebuild-legacy-image",
            "--expected-id",
            &legacy_id,
            "--no-pull",
            "--",
            "--file",
        ])
        .arg(&recipe)
        .arg("--no-cache")
        .arg("--volume")
        .arg(format!("{}:/gate:rw", gate.display()))
        .output()?;
    ensure!(
        rebuilt.status.success(),
        "explicit legacy rebuild failed: {}",
        String::from_utf8_lossy(&rebuilt.stderr)
    );
    ownership::require_image(b, &b.image_ref())?;
    images.ids.push(bcvk::image_id(&b.image_ref())?);
    let receipt_path = root
        .join("state")
        .join(&b.slug)
        .join("legacy-images")
        .join(format!("{legacy_id}.json"));
    let receipt: serde_json::Value = serde_json::from_slice(&fs::read(receipt_path)?)?;
    let backup = receipt["backup"]
        .as_str()
        .context("legacy backup receipt missing")?;
    ensure!(
        bcvk::image_id(backup)? == legacy_id && bcvk::image_id(&b.image_ref())? != legacy_id,
        "legacy image was adopted instead of rebuilt"
    );
    images.tags.push(backup.into());
    ensure!(
        ownership::vm_lease(b, &a.vm_name("foreign")).is_err(),
        "bcvk name from another checkout accepted"
    );
    ensure!(
        ownership::check_output(b, "localhost/ilex:main").is_err(),
        "noncanonical checkout can allocate reserved main output"
    );
    let disk = root.join("legacy-cache.qcow2");
    fs::write(&disk, b"unlabelled")?;
    ensure!(
        ownership::claim_file(b, &disk).is_err(),
        "legacy disk adopted"
    );
    let marker = disk.with_file_name("legacy-cache.qcow2.ilex-owner");
    fs::write(
        marker,
        serde_json::to_vec(&serde_json::json!({"schema": 1, "owner": a.identity()?.owner}))?,
    )?;
    ensure!(
        ownership::claim_file(b, &disk).is_err(),
        "foreign disk adopted"
    );
    Ok(
        serde_json::json!({"result": "passed", "a": a.identity()?, "b": b.identity()?, "same_worktree_name": a.worktree, "accepted_id": accepted, "accepted_source_digest": accepted_digest, "source_races": race_logs, "foreign_image_rejected_by": rejected, "unlabelled_image_rejected": true, "explicit_legacy_rebuilt_with_backup": true, "foreign_bcvk_name_rejected": true, "foreign_and_legacy_disk_rejected": true, "cleanup": "only fixture tags untagged; fixture clones/state removed"}),
    )
}
