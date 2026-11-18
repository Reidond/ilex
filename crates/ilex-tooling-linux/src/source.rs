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
                Some(".git" | "target" | ".ilex-state")
            )
        }) {
            continue;
        }
        ensure!(
            !path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s == ".env" || s.ends_with(".key")),
            "private-named source input {relative}; remove it before building; it cannot be silently omitted from provenance"
        );
        let absolute = ws.root.join(path);
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
        let (kind, bytes) = if meta.is_symlink() || mode == Some("120000") {
            let target = if meta.is_symlink() {
                fs::read_link(&absolute)?.to_string_lossy().into_owned()
            } else {
                fs::read_to_string(&absolute)?
            };
            let resolved = absolute.parent().unwrap().join(&target);
            ensure!(
                !Path::new(&target).is_absolute()
                    && fs::canonicalize(&resolved)?.starts_with(&canonical_root),
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
                    executable: mode == Some("100755"),
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
