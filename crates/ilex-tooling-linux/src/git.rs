//! Git queries for labels and fingerprints.

use std::path::Path;
use std::process::Command;

use anyhow::Result;

use crate::cmd;

/// A `git` command rooted at `root`, using the native checkout.
pub fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(root);
    c
}

/// Full SHA of `HEAD`.
pub fn head_sha(root: &Path) -> Result<String> {
    cmd::output(git(root).args(["rev-parse", "HEAD"]))
}

/// Short SHA of `HEAD` (7 characters).
pub fn short_sha(root: &Path) -> Result<String> {
    cmd::output(git(root).args(["rev-parse", "--short=7", "HEAD"]))
}

/// True when tracked files differ from `HEAD` or untracked files exist.
pub fn is_dirty(root: &Path) -> Result<bool> {
    let status =
        cmd::output(git(root).args(["status", "--porcelain", "--untracked-files=normal"]))?;
    Ok(!status.is_empty())
}

/// `<sha>` or `<sha>-dirty`, for `org.opencontainers.image.revision`.
pub fn revision(root: &Path) -> Result<String> {
    if let Some(context) = crate::source::remote_context(root)? {
        return Ok(context.identity.source_revision);
    }
    let sha = head_sha(root)?;
    Ok(if is_dirty(root)? {
        format!("{sha}-dirty")
    } else {
        sha
    })
}

/// Git tree hash of `path` (relative to the repo root) at `HEAD`.
pub fn tree_hash(root: &Path, path: &str) -> Result<String> {
    cmd::output(git(root).args(["rev-parse", &format!("HEAD:{path}")]))
}

/// Read a gitfile target to identify linked worktrees.
pub(crate) fn dotgit_target(root: &Path) -> Option<String> {
    let dotgit = std::fs::read_to_string(root.join(".git")).ok()?;
    let target = dotgit.strip_prefix("gitdir:")?.trim();
    (!target.is_empty()).then(|| target.to_string())
}
