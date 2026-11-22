//! Exercise the real disk CLI with isolated Podman/bcvk fault injection. These
//! fixtures never call the real store or install/boot a guest.
use anyhow::Result;
use ilex_devkit::Workspace;

#[cfg(not(target_os = "linux"))]
pub(super) fn run(_: &Workspace) -> Result<()> {
    anyhow::bail!("disk alias self-check requires Linux")
}

#[cfg(target_os = "linux")]
pub(super) fn run(ws: &Workspace) -> Result<()> {
    use anyhow::{Context, ensure};
    use ilex_devkit::bcvk;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    struct Running(Child);
    impl Drop for Running {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let root = std::env::temp_dir().join(format!(
        "ilex-disk-self-check-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&root)?;
    let _fixture = Fixture(root.clone());
    for dir in ["bin", "store"] {
        fs::create_dir(root.join(dir))?;
    }
    let id = "a".repeat(64);
    let source = format!("sha256:{id}");
    let digest = format!("sha256:{}", "b".repeat(64));
    let alias_file = root.join("alias-id");
    let calls = root.join("calls");
    fs::write(&calls, b"")?;
    fs::write(
        root.join("bin/podman"),
        r#"#!/bin/bash
set -eu
last=${!#}
case "$1 ${2:-}" in
  '--version ') echo 'podman version 0.0.0 (fixture)' ;;
  'info --format') echo info >> "$CHECK_ROOT/calls"; echo "$CHECK_ROOT/store" ;;
  'image exists')
    if [ "$last" = "$CHECK_SOURCE" ]; then exit 0; fi
    if [ "$CHECK_MODE" = exists-error ]; then exit 125; fi
    test -f "$CHECK_ROOT/alias-id" ;;
  'image inspect')
    if [ "$last" = "$CHECK_SOURCE" ]; then
      if [ "$4" = '{{.Digest}}' ]; then echo "$CHECK_DIGEST"; exit 0; fi
      if [ "$CHECK_MODE" = source-error ]; then echo 'injected source inspect error' >&2; exit 125; fi
      if [ "$CHECK_MODE" = source-mismatch ]; then echo wrong; else echo "${CHECK_SOURCE#sha256:}"; fi
    else
      if [ "$CHECK_MODE" = inspect-error ]; then echo 'injected alias inspect error' >&2; exit 125; fi
      cat "$CHECK_ROOT/alias-id"
    fi ;;
  'tag '*) echo tag >> "$CHECK_ROOT/calls"; echo "${CHECK_SOURCE#sha256:}" > "$CHECK_ROOT/alias-id" ;;
  *) echo "unexpected podman command: $*" >&2; exit 125 ;;
esac
"#,
    )?;
    fs::write(
        root.join("bin/bcvk"),
        r#"#!/bin/bash
set -eu
echo bcvk >> "$CHECK_ROOT/calls"
if [ "$CHECK_MODE" = gated ]; then
  touch "$CHECK_ROOT/started"
  deadline=$((SECONDS + 20))
  until test -f "$CHECK_ROOT/release"; do
    if (( SECONDS >= deadline )); then exit 1; fi
    sleep 0.05
  done
fi
qemu-img create -q -f qcow2 "${!#}" 1M
"#,
    )?;
    for name in ["podman", "bcvk"] {
        fs::set_permissions(
            root.join("bin").join(name),
            fs::Permissions::from_mode(0o700),
        )?;
    }
    let exe = std::env::current_exe()?;
    let mut paths = vec![root.join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").context("PATH missing")?,
    ));
    let path = std::env::join_paths(paths)?;
    let command = |mode: &str, out: &str, runtime: &str| {
        let mut c = Command::new(&exe);
        c.current_dir(&ws.root)
            .arg("--ilexctl-in-checkout")
            .args(["disk", "--image", &source, "--size", "1M", "--out"])
            .arg(root.join(out))
            .env("ILEX_ROOT", &ws.root)
            .env("ILEX_STATE_DIR", root.join("state"))
            .env("XDG_RUNTIME_DIR", root.join(runtime))
            .env("PATH", &path)
            .env("CHECK_ROOT", &root)
            .env("CHECK_SOURCE", &source)
            .env("CHECK_DIGEST", &digest)
            .env("CHECK_MODE", mode)
            .env_remove("ILEX_SHELLS")
            .env_remove("ILEX_MACHINE")
            .env_remove("ILEX_IMAGE");
        c
    };
    let reject = |mode: &str, message: &str| -> Result<()> {
        fs::write(&calls, b"")?;
        let before = fs::read(&alias_file).ok();
        let out = bcvk::capture(
            &mut command(mode, "disk.qcow2", "run"),
            Duration::from_secs(20),
        )?;
        ensure!(
            !out.success() && out.stderr.contains(message),
            "wrong {mode} rejection: {out:?}"
        );
        ensure!(
            !fs::read_to_string(&calls)?
                .lines()
                .any(|line| line == "tag" || line == "bcvk"),
            "{mode} authorized a mutation or installer"
        );
        ensure!(
            fs::read(&alias_file).ok() == before,
            "{mode} changed existing alias"
        );
        Ok(())
    };
    reject("source-error", "injected source inspect error")?;
    reject("source-mismatch", "ID mismatch")?;
    // Even identical contents without an allocation receipt must be refused.
    fs::write(&alias_file, &id)?;
    reject("normal", "no allocation receipt")?;
    fs::remove_file(&alias_file)?;
    let baseline = bcvk::capture(
        &mut command("normal", "disk.qcow2", "run"),
        Duration::from_secs(20),
    )?;
    ensure!(baseline.success(), "fresh allocation failed: {baseline:?}");
    let manifest: ilex_devkit::core::DiskArtifact =
        serde_json::from_slice(&fs::read(root.join("disk.qcow2.disk.json"))?)?;
    ensure!(
        manifest.format == ilex_devkit::core::DiskFormat::Qcow2
            && manifest.virtual_bytes == 1024 * 1024
            && manifest.image.immutable_id == id
            && manifest.image.registry_digest.as_deref() == Some(digest.as_str())
            && manifest.location.sha256 == super::hash_file(&root.join("disk.qcow2"))?,
        "wrong fixture disk manifest"
    );
    let receipts = root.join("state").join(&ws.slug).join("image-aliases");
    let receipt_path = fs::read_dir(&receipts)?
        .next()
        .context("allocation receipt missing")??
        .path();
    let receipt = fs::read(&receipt_path)?;
    let value: serde_json::Value = serde_json::from_slice(&receipt)?;
    ensure!(
        value["owner"] == ws.identity()?.owner && value["id"] == id,
        "wrong allocation receipt"
    );
    // The alias now has a valid receipt: this reaches the exact inspect=125
    // branch that previously fell through to tag + bcvk + success.
    reject("inspect-error", "injected alias inspect error")?;
    reject("exists-error", "cannot inspect image ownership")?;
    reject("source-error", "injected source inspect error")?;
    ensure!(
        fs::read(&receipt_path)? == receipt,
        "inspection error changed allocation receipt"
    );
    fs::write(&alias_file, "b".repeat(64))?;
    reject("normal", "ID mismatch")?;
    fs::write(&alias_file, &id)?;
    let mut foreign = value;
    foreign["owner"] = "another-owner".into();
    fs::write(&receipt_path, serde_json::to_vec(&foreign)?)?;
    reject("normal", "resource belongs to owner")?;
    fs::write(&receipt_path, &receipt)?;
    fs::write(&calls, b"")?;
    let reused = bcvk::capture(
        &mut command("normal", "second.qcow2", "other-run"),
        Duration::from_secs(20),
    )?;
    ensure!(reused.success(), "receipted reuse failed: {reused:?}");
    ensure!(
        !fs::read_to_string(&calls)?
            .lines()
            .any(|line| line == "tag"),
        "reuse retagged alias"
    );
    ensure!(
        fs::read(&receipt_path)? == receipt,
        "reuse changed allocation receipt"
    );

    // Distinct output disks and runtime roots must still share one image lease,
    // held through the installer, not merely through allocation.
    fs::write(&calls, b"")?;
    let mut first = Running(
        command("gated", "first.qcow2", "run-a")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let wait = |ready: &dyn Fn() -> Result<bool>| -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !ready()? {
            ensure!(
                Instant::now() < deadline,
                "alias lease self-check timed out"
            );
            thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    };
    wait(&|| Ok(root.join("started").exists()))?;
    let mut second = Running(
        command("normal", "last.qcow2", "run-b")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    wait(&|| {
        Ok(fs::read_to_string(&calls)?
            .lines()
            .filter(|line| *line == "info")
            .count()
            == 2)
    })?;
    ensure!(
        second.0.try_wait()?.is_none(),
        "second output did not wait for alias lease"
    );
    ensure!(
        fs::read_to_string(&calls)?
            .lines()
            .filter(|line| *line == "bcvk")
            .count()
            == 1,
        "installers overlapped across outputs"
    );
    fs::write(root.join("release"), b"release")?;
    wait(&|| Ok(root.join("last.qcow2").exists()))?;
    ensure!(
        first.0.wait()?.success() && second.0.wait()?.success(),
        "leased installations failed"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "self_check": "passed", "inspect_exit_125_rejected_before_tag_or_bcvk": true,
            "exists_exit_125_rejected": true, "source_validated_before_tag": true,
            "unreceipted_same_id_rejected": true, "foreign_receipt_and_wrong_id_rejected": true,
            "receipted_reuse_without_retag": true, "different_outputs_and_runtime_roots_serialized": true,
            "standalone_qcow2_manifest_validated": true,
            "backend": "disposable CLI fault injection; no real images or guests"
        }))?
    );
    Ok(())
}
