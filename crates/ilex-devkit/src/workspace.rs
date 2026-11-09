//! Which checkout are we in, and what does it own?
//!
//! The main checkout owns `localhost/ilex:main` and VMs named `ilex-main-*`.
//! A Claude Code worktree at `.claude/worktrees/<name>/` owns
//! `localhost/ilex:wt-<name>` and VMs named `ilex-<name>-*`, so parallel
//! sessions never overwrite each other's images or VMs.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Local repository every build is tagged into.
pub const IMAGE_REPO: &str = "localhost/ilex";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Root of this checkout (the directory holding `Containerfile`).
    pub root: PathBuf,
    /// `main` for the main checkout, the sanitized worktree name otherwise.
    pub slug: String,
    /// Worktree name when running inside `.claude/worktrees/<name>/`.
    pub worktree: Option<String>,
}

impl Workspace {
    /// Find the checkout from `ILEX_ROOT` or by walking up from the current
    /// directory. Works from `cargo xtask` and from `cargo test`, whose
    /// working directory is the crate's own directory.
    pub fn discover() -> Result<Self> {
        if let Some(root) = env::var_os("ILEX_ROOT") {
            return Ok(Self::from_root(PathBuf::from(root)));
        }
        let cwd = env::current_dir().context("reading current directory")?;
        for dir in cwd.ancestors() {
            if dir.join("Containerfile").is_file() && dir.join("Cargo.toml").is_file() {
                return Ok(Self::from_root(dir.to_path_buf()));
            }
        }
        bail!(
            "no ilex checkout above {} (looked for Containerfile + Cargo.toml; set ILEX_ROOT to override)",
            cwd.display()
        )
    }

    pub fn from_root(root: PathBuf) -> Self {
        let worktree = worktree_name(&root);
        let slug = match &worktree {
            Some(name) => sanitize(name),
            None => "main".to_string(),
        };
        Self { root, slug, worktree }
    }

    /// Tag part of this checkout's image: `main` or `wt-<name>`.
    pub fn image_tag(&self) -> String {
        match &self.worktree {
            Some(_) => format!("wt-{}", self.slug),
            None => "main".to_string(),
        }
    }

    /// Full local image reference, e.g. `localhost/ilex:wt-foo`.
    pub fn image_ref(&self) -> String {
        format!("{IMAGE_REPO}:{}", self.image_tag())
    }

    /// Turn a user-supplied tag or reference into a full image reference:
    /// `candidate` becomes `localhost/ilex:candidate`, anything with a `/` or
    /// `:` is taken as-is.
    pub fn resolve_image(&self, tag_or_ref: Option<&str>) -> String {
        match tag_or_ref {
            None => self.image_ref(),
            Some(s) if s.contains('/') || s.contains(':') || s.contains('@') => s.to_string(),
            Some(tag) => format!("{IMAGE_REPO}:{tag}"),
        }
    }

    /// Name for a VM this checkout owns, e.g. `ilex-main-ephemeral`.
    pub fn vm_name(&self, purpose: &str) -> String {
        format!("ilex-{}-{}", self.slug, purpose)
    }

    /// Prefix shared by every VM this checkout owns (`ilex-<slug>-`).
    pub fn vm_prefix(&self) -> String {
        format!("ilex-{}-", self.slug)
    }

    /// Per-checkout runtime directory for sockets:
    /// `$XDG_RUNTIME_DIR/ilex-vm/<slug>/` (QMP, VNC, ...).
    pub fn runtime_dir(&self) -> PathBuf {
        vm_runtime_root().join(&self.slug)
    }

    /// Per-checkout persistent state (disk images, test artifacts):
    /// `$ILEX_STATE_DIR` or `$XDG_CACHE_HOME/ilex` or `~/.cache/ilex`, then `/<slug>`.
    /// Kept outside the checkout so it never lands in a build context and,
    /// on WSL, stays on the Linux filesystem.
    pub fn state_dir(&self) -> PathBuf {
        state_root().join(&self.slug)
    }
}

/// Machine-wide runtime root shared by every checkout: `$XDG_RUNTIME_DIR/ilex-vm`.
/// VM slot locks live directly in here.
pub fn vm_runtime_root() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("ilex-vm");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::metadata("/proc/self") {
            let run_user = PathBuf::from(format!("/run/user/{}", meta.uid()));
            if run_user.is_dir() {
                return run_user.join("ilex-vm");
            }
            return env::temp_dir().join(format!("ilex-vm-{}", meta.uid()));
        }
    }
    env::temp_dir().join("ilex-vm")
}

fn state_root() -> PathBuf {
    if let Some(dir) = env::var_os("ILEX_STATE_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(dir) = env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("ilex");
    }
    if let Some(home) = env::var_os("HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(home).join(".cache").join("ilex");
    }
    env::temp_dir().join("ilex-state")
}

/// `.../.claude/worktrees/<name>` → `Some(name)`.
fn worktree_name(root: &Path) -> Option<String> {
    let parts: Vec<String> = root
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts.windows(3).rev().find_map(|w| {
        (w[0] == ".claude" && w[1] == "worktrees").then(|| w[2].clone())
    })
}

/// Lowercase, `[a-z0-9-]` only, no leading/trailing/double dashes — safe in
/// image tags, VM names and file names.
fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() { "wt".to_string() } else { out }
}
