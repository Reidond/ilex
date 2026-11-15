//! Which checkout are we in, and what does it own?
//!
//! The main checkout owns `localhost/ilex:main` and VMs named `ilex-main-*`.
//! A linked git worktree (including Claude Code and T3 worktrees) owns
//! `localhost/ilex:wt-<slug>` and VMs named `ilex-<slug>-*`, so parallel
//! sessions never overwrite each other's images or VMs.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::git;

/// Local repository every build is tagged into.
pub const IMAGE_REPO: &str = "localhost/ilex";

// `/run/user/0/ilex-vm/<slug>/graphical/vdagent.sock` adds 43 bytes to the
// slug: 48 leaves 16 bytes spare below Linux's 107-byte socket path limit.
// `ilex-<slug>-ephemeral` also fits the 63-character hostname budget.
const MAX_SLUG_LEN: usize = 48;
const HASH_HEX_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Root of this checkout (the directory holding `Containerfile`).
    pub root: PathBuf,
    /// `main` for the main checkout, a bounded worktree identity otherwise.
    pub slug: String,
    /// Git's unique, unsanitised name for a linked worktree.
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
        let root = std::path::absolute(&root).unwrap_or(root);
        let worktree = worktree_name(&root);
        let slug = match &worktree {
            Some(name) => sanitize(name),
            None => "main".to_string(),
        };
        Self { root, slug, worktree }
    }

    /// Tag part of this checkout's image: `main` or `wt-<slug>`.
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

    /// A non-default shell list gets one `.s-<name>` per shell, in order;
    /// an empty list gets `.s-none`. Slugs have no dots (hashed slugs may
    /// contain `--`), so another checkout cannot own this variant's tag.
    /// `None` keeps the default image. VM names and state paths stay unchanged.
    /// The caller validates the selection and the resulting tag length.
    pub fn variant_ref(&self, shells: Option<&[String]>) -> String {
        let mut image = self.image_ref();
        if let Some(shells) = shells {
            if shells.is_empty() {
                image.push_str(".s-none");
            } else {
                for shell in shells {
                    image.push_str(".s-");
                    image.push_str(shell);
                }
            }
        }
        image
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

/// Image tags have a 128-byte limit. A maximum-length worktree tag uses 51
/// bytes (`wt-` + 48); shell suffixes and later machine suffixes share the
/// remaining 77. Reject overflow rather than truncate and risk a collision.
/// Also usable after appending a derived image suffix. Digest refs have no tag.
pub fn check_tag_length(image: &str) -> Result<()> {
    if !image.contains('@')
        && let Some((_, tag)) = image.rsplit('/').next().unwrap_or(image).rsplit_once(':')
        && tag.len() > 128
    {
        bail!("image tag is {} bytes, exceeding the 128-byte limit; use a shorter selection or explicit image tag", tag.len());
    }
    Ok(())
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

/// Git keeps linked worktree names unique under `.git/worktrees/<name>`.
/// Split both path separators: a Windows-created `.git` file is also read
/// in WSL. Other gitfiles (submodules, separate git dirs) are not worktrees.
fn worktree_name(root: &Path) -> Option<String> {
    let target = git::dotgit_target(root)?;
    let mut parts = target.trim_end_matches(['/', '\\']).rsplit(['/', '\\']);
    let name = parts.next()?;
    (parts.next()? == "worktrees").then(|| name.to_string())
}

/// Keep short, valid names stable. Hash the original git name when changing
/// it, shortening it, or escaping the main checkout's reserved identity.
/// Plain slugs have no double dashes; hashed slugs use `--` before the hash,
/// so the two forms cannot overlap. Both are bounded ASCII without leading
/// or trailing dashes.
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
    let out = out.trim_matches('-');
    let out = match out {
        "" => "wt",
        "main" => "wt-main",
        out => out,
    };
    if out == name && out.len() <= MAX_SLUG_LEN {
        return out.to_string();
    }
    let readable_len = out.len().min(MAX_SLUG_LEN - 2 - HASH_HEX_LEN);
    let readable = out[..readable_len].trim_end_matches('-');
    let hash = format!("{:x}", Sha256::digest(name.as_bytes()));
    format!("{readable}--{}", &hash[..HASH_HEX_LEN])
}
