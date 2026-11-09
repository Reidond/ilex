//! Git queries for labels and fingerprints.
//!
//! On a Windows machine the stack runs in WSL2 against the checkout under
//! `/mnt/c`. A Claude worktree created by Windows git has a `.git` file that
//! says `gitdir: C:/...`, which Linux git can't follow, so [`git`] points
//! `GIT_DIR` at the `/mnt/<drive>/...` translation of it.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::cmd;

/// A `git` command rooted at `root`, with the WSL gitdir fix applied.
pub fn git(root: &Path) -> Command {
    let mut c = Command::new("git");
    c.current_dir(root);
    if let Some(gitdir) = wsl_gitdir(root) {
        c.env("GIT_DIR", gitdir)
            .env("GIT_WORK_TREE", root)
            // Windows checkouts carry no exec bits; don't report mode changes.
            .args(["-c", "core.fileMode=false"]);
    }
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
    let status = cmd::output(git(root).args(["status", "--porcelain", "--untracked-files=normal"]))?;
    Ok(!status.is_empty())
}

/// `<sha>` or `<sha>-dirty`, for `org.opencontainers.image.revision`.
pub fn revision(root: &Path) -> Result<String> {
    let sha = head_sha(root)?;
    Ok(if is_dirty(root)? { format!("{sha}-dirty") } else { sha })
}

/// Git tree hash of `path` (relative to the repo root) at `HEAD`.
pub fn tree_hash(root: &Path, path: &str) -> Result<String> {
    cmd::output(git(root).args(["rev-parse", &format!("HEAD:{path}")]))
}

fn wsl_gitdir(root: &Path) -> Option<PathBuf> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let dotgit = std::fs::read_to_string(root.join(".git")).ok()?;
    translate_windows_path(dotgit.strip_prefix("gitdir:")?.trim())
}

/// `C:/Users/x` or `C:\Users\x` → `/mnt/c/Users/x`.
fn translate_windows_path(p: &str) -> Option<PathBuf> {
    let b = p.as_bytes();
    if b.len() < 3 || !b[0].is_ascii_alphabetic() || b[1] != b':' || !matches!(b[2], b'/' | b'\\') {
        return None;
    }
    let drive = (b[0] as char).to_ascii_lowercase();
    Some(PathBuf::from(format!("/mnt/{drive}{}", p[2..].replace('\\', "/"))))
}
