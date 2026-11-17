//! Local source provenance. This does not stage or transfer remote snapshots;
//! that executor is T3. Git modes and working bytes include dirty input.
use crate::{Workspace, git};
use anyhow::{Context, Result, ensure};
use ilex_tooling_core::{SCHEMA_VERSION, SourceEntry, SourceEntryKind, SourceManifest, digest};
use std::{
    collections::BTreeMap,
    fs,
    path::{Component, Path},
};

fn git_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = git::git(root).args(args).output()?;
    ensure!(
        out.status.success(),
        "git source inventory failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(out.stdout)
}

pub fn manifest(ws: &Workspace) -> Result<SourceManifest> {
    if let Some(context) = remote_context(&ws.root)? {
        verify_materialized(&ws.root, &context.source)?;
        return Ok(context.source);
    }
    let first = inventory(ws)?;
    let second = inventory(ws)?;
    ensure!(
        first.digest == second.digest
            && first.revision == second.revision
            && first.dirty == second.dirty,
        "source bytes changed during inventory; retry the command"
    );
    Ok(second)
}

pub fn remote_context(root: &Path) -> Result<Option<ilex_tooling_core::SnapshotContext>> {
    let Some(path) = std::env::var_os("ILEX_REMOTE_CONTEXT") else {
        return Ok(None);
    };
    let context: ilex_tooling_core::SnapshotContext = serde_json::from_slice(&fs::read(path)?)?;
    context.validate()?;
    if fs::canonicalize(root)? != fs::canonicalize(&context.root)? {
        return Ok(None);
    }
    Ok(Some(context))
}

pub fn verify_materialized(root: &Path, source: &SourceManifest) -> Result<()> {
    source.validate()?;
    let others = git_bytes(root, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for path in others.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let name = std::str::from_utf8(path)?;
        ensure!(
            source.entries.iter().any(|e| e.path == name),
            "new source input appeared after staging: {name}"
        );
    }
    for entry in &source.entries {
        let path = root.join(&entry.path);
        // A symlink parent is never a directory into which source can be written.
        for parent in path
            .parent()
            .unwrap()
            .ancestors()
            .take_while(|p| *p != root)
        {
            if let Ok(meta) = fs::symlink_metadata(parent) {
                ensure!(
                    !meta.is_symlink(),
                    "symlink parent in source: {}",
                    entry.path
                );
            }
        }
        let bytes = match &entry.kind {
            SourceEntryKind::Deleted => {
                ensure!(
                    !path.try_exists()? && fs::symlink_metadata(&path).is_err(),
                    "deleted source reappeared: {}",
                    entry.path
                );
                continue;
            }
            SourceEntryKind::Symlink { target } => {
                ensure!(
                    fs::symlink_metadata(&path)?.is_symlink()
                        && fs::read_link(&path)? == Path::new(target),
                    "source link changed: {}",
                    entry.path
                );
                target.as_bytes().to_vec()
            }
            SourceEntryKind::File { executable } => {
                let meta = fs::symlink_metadata(&path)?;
                ensure!(
                    meta.is_file() && !meta.is_symlink(),
                    "source type changed: {}",
                    entry.path
                );
                #[cfg(target_os = "linux")]
                {
                    use std::os::unix::fs::PermissionsExt;
                    ensure!(
                        (meta.permissions().mode() & 0o111 != 0) == *executable,
                        "source mode changed: {}",
                        entry.path
                    );
                }
                #[cfg(not(target_os = "linux"))]
                let _ = executable;
                fs::read(&path)?
            }
        };
        ensure!(
            bytes.len() as u64 == entry.bytes
                && entry.sha256.as_deref() == Some(digest(&bytes).as_str()),
            "source bytes changed: {}",
            entry.path
        );
    }
    Ok(())
}

fn inventory(ws: &Workspace) -> Result<SourceManifest> {
    // These additional build contexts include ignored files in Podman COPY.
    let ignored = git_bytes(
        &ws.root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
            "--",
            "rootfs",
            "crates",
            "build",
            "shells",
            "users",
            "machines",
            "installer",
        ],
    )?;
    ensure!(
        ignored.is_empty(),
        "ignored files exist in build inputs; remove them before building (source inventory cannot attest them)"
    );
    let before = git_bytes(
        &ws.root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let index = git_bytes(&ws.root, &["ls-files", "--stage", "-z"])?;
    let mut modes = BTreeMap::new();
    for entry in index.split(|&b| b == 0).filter(|b| !b.is_empty()) {
        let entry = std::str::from_utf8(entry).context("source paths must be UTF-8")?;
        let (header, path) = entry.split_once('\t').context("invalid git index entry")?;
        let mut fields = header.split_whitespace();
        let mode = fields.next().context("missing git file mode")?;
        ensure!(fields.nth(1) == Some("0"), "unmerged index entry {path}");
        ensure!(
            mode != "160000",
            "submodule {path} is not a supported source input"
        );
        modes.insert(path.to_string(), mode.to_string());
    }
    let files = git_bytes(
        &ws.root,
        &[
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ],
    )?;
    let mut paths = files
        .split(|&b| b == 0)
        .filter(|b| !b.is_empty())
        .map(|b| std::str::from_utf8(b).map(str::to_owned))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    // Staged deletions disappear from the index. Keep their HEAD names so both
    // staged and unstaged deletions are explicit in the transport manifest.
    let head_files = git_bytes(&ws.root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?;
    for path in head_files.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        paths.push(std::str::from_utf8(path)?.to_owned());
    }
    paths.sort();
    paths.dedup();
    let canonical_root = fs::canonicalize(&ws.root)?;
    let mut entries = Vec::new();
    for relative in paths {
        let path = Path::new(&relative);
        ensure!(
            path.components().all(|c| matches!(c, Component::Normal(_))),
            "unsafe source path {relative:?}"
        );
        // No source discovery follows private state, Git internals or credentials.
        if path.components().take(1).any(|c| {
            matches!(
                c.as_os_str().to_str(),
                Some(".git" | "target" | ".ilex-state" | ".claude" | ".t3")
            )
        }) {
            continue;
        }
        ensure!(
            !path
                .components()
                .any(|p| p.as_os_str().to_str().is_some_and(|s| matches!(
                    s,
                    ".env" | ".ssh" | ".gnupg" | "ilex-secrets" | "credentials"
                ) || s.starts_with(".env.")
                    || [".key", ".pem", ".p12", ".pfx"]
                        .iter()
                        .any(|e| s.ends_with(e)))),
            "private-named source input {relative}; remove it before building; it cannot be silently omitted from provenance"
        );
        let absolute = ws.root.join(path);
        for ancestor in absolute
            .parent()
            .unwrap()
            .ancestors()
            .take_while(|p| *p != ws.root)
        {
            if let Ok(meta) = fs::symlink_metadata(ancestor) {
                ensure!(
                    !is_reparse(&meta) && !meta.is_symlink(),
                    "source has symlink/reparse parent: {relative}"
                );
            }
        }
        let parent = absolute.parent().context("source file has no parent")?;
        let parent = parent
            .ancestors()
            .find(|p| p.exists())
            .context("source has no existing ancestor")?;
        let parent = fs::canonicalize(parent)?;
        ensure!(
            parent.starts_with(&canonical_root),
            "source parent escapes checkout: {relative}"
        );
        let mode = modes.get(&relative).map(String::as_str);
        let meta = match fs::symlink_metadata(&absolute) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                entries.push(SourceEntry {
                    path: relative,
                    kind: SourceEntryKind::Deleted,
                    sha256: None,
                    bytes: 0,
                });
                continue;
            }
            Err(err) => return Err(err.into()),
        };
        ensure!(
            !is_reparse(&meta) || meta.is_symlink(),
            "unsupported source reparse point: {relative}"
        );
        let (kind, bytes) = if meta.is_symlink() || mode == Some("120000") {
            let target = if meta.is_symlink() {
                fs::read_link(&absolute)?.to_string_lossy().into_owned()
            } else {
                fs::read_to_string(&absolute)?
            };
            let mut depth = relative.split('/').count() - 1;
            ensure!(
                !target.contains(['\\', ':']) && !target.starts_with('/'),
                "source symlink escapes checkout: {relative}"
            );
            for part in target.split('/') {
                if part == ".." {
                    ensure!(depth > 0, "source symlink escapes checkout: {relative}");
                    depth -= 1;
                } else if !part.is_empty() && part != "." {
                    depth += 1;
                }
            }
            let resolved = absolute.parent().unwrap().join(&target);
            ensure!(
                !Path::new(&target).is_absolute()
                    && fs::canonicalize(&resolved).with_context(|| format!("resolving source symlink {relative}; its target must exist inside the checkout"))?.starts_with(&canonical_root),
                "source symlink escapes checkout: {relative}"
            );
            (
                SourceEntryKind::Symlink {
                    target: target.clone(),
                },
                target.into_bytes(),
            )
        } else {
            ensure!(meta.is_file(), "unsupported source entry {relative}");
            (
                SourceEntryKind::File {
                    executable: executable(&meta, mode),
                },
                fs::read(&absolute)?,
            )
        };
        entries.push(SourceEntry {
            path: relative,
            kind,
            sha256: Some(digest(&bytes)),
            bytes: bytes.len() as u64,
        });
    }
    let after = git_bytes(
        &ws.root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    ensure!(
        before == after,
        "source changed during inventory; retry the command"
    );
    let revision = git::head_sha(&ws.root)?;
    let digest = digest(&serde_json::to_vec(&entries)?);
    Ok(SourceManifest {
        schema: SCHEMA_VERSION,
        revision,
        dirty: !before.is_empty(),
        digest,
        entries,
    })
}

fn is_reparse(meta: &fs::Metadata) -> bool {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = meta;
        false
    }
}

fn executable(meta: &fs::Metadata, mode: Option<&str>) -> bool {
    if let Some(mode) = mode {
        return mode == "100755";
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = meta;
        false
    }
}
