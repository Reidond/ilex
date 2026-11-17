//! Which checkout are we in, and what does it own?
//!
//! Canonical owners get bounded, owner-qualified resource names. Only an
//! explicitly declared canonical primary checkout reserves the `main` name.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ilex_tooling_core::{CheckoutIdentity, digest};

use crate::git;

/// Local repository every build is tagged into.
pub const IMAGE_REPO: &str = "localhost/ilex";

// `/run/user/0/ilex-vm/<slug>/graphical/vdagent.sock` adds 43 bytes to the
// slug: 48 leaves 16 bytes spare below Linux's 107-byte socket path limit.
// `ilex-<slug>-ephemeral` also fits the 63-character hostname budget.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Root of this checkout (the directory holding `Containerfile`).
    pub root: PathBuf,
    /// `main` for the main checkout, a bounded worktree identity otherwise.
    pub slug: String,
    /// Git's repository-local, unsanitised name for a linked worktree.
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
        let root = std::fs::canonicalize(&root)
            .or_else(|_| std::path::absolute(&root))
            .unwrap_or(root);
        #[cfg(windows)]
        let root = PathBuf::from(
            root.to_string_lossy()
                .strip_prefix(r"\\?\")
                .unwrap_or(&root.to_string_lossy()),
        );
        if let Ok(Some(context)) = crate::source::remote_context(&root) {
            return Self {
                root,
                slug: context.identity.slug,
                worktree: Some("remote-snapshot".into()),
            };
        }
        let worktree = worktree_name(&root).or_else(|| {
            // Unknown gitfiles and missing metadata must never acquire main's
            // namespace. Only a real main checkout has a .git directory.
            (!root.join(".git").is_dir())
                .then(|| format!("checkout-{}", &digest(native_path(&root).as_bytes())[7..23]))
        });
        let slug = allocation(&root, worktree.as_deref())
            .unwrap_or_else(|_| {
                CheckoutIdentity::new(
                    digest(native_path(&root).as_bytes()),
                    native_path(&root),
                    "checkout",
                    false,
                )
            })
            .slug;
        Self {
            root,
            slug,
            worktree,
        }
    }

    /// Native identity survives Windows-to-WSL handoff. The Linux executor
    /// never derives a second owner from the translated mount path.
    pub fn identity(&self) -> Result<CheckoutIdentity> {
        if let Some(context) = crate::source::remote_context(&self.root)? {
            anyhow::ensure!(
                self.slug == context.identity.slug,
                "remote namespace mismatch"
            );
            return Ok(context.identity);
        }
        let mut identity = allocation(&self.root, worktree_name(&self.root).as_deref())?;
        anyhow::ensure!(
            self.slug == identity.slug,
            "checkout resource namespace is stale or belongs to another owner; rediscover this checkout with the current tooling"
        );
        identity.source_revision = git::revision(&self.root)?;
        Ok(identity)
    }

    pub fn source_identity(&self) -> Result<(CheckoutIdentity, ilex_tooling_core::SourceManifest)> {
        let source = crate::source::manifest(self)?;
        let mut identity = self.identity()?;
        identity.source_revision = if source.dirty {
            format!("{}-dirty", source.revision)
        } else {
            source.revision.clone()
        };
        identity.snapshot_digest = Some(source.digest.clone());
        Ok((identity, source))
    }

    /// Tag part of this checkout's image: `main` or `wt-<slug>`.
    pub fn image_tag(&self) -> String {
        if self.slug == "main" {
            "main".into()
        } else {
            format!("wt-{}", self.slug)
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
        vm_runtime_root().join(self.state_slug())
    }

    /// Per-checkout persistent state (disk images, test artifacts):
    /// `$ILEX_STATE_DIR` or `$XDG_CACHE_HOME/ilex` or `~/.cache/ilex`, then `/<slug>`.
    /// Kept outside the checkout so it never lands in a build context and,
    /// on WSL, stays on the Linux filesystem.
    pub fn state_dir(&self) -> PathBuf {
        state_root().join(self.state_slug())
    }

    fn state_slug(&self) -> String {
        if self.slug != "main" {
            return self.slug.clone();
        }
        // Main can change owners only through explicit stale-claim release.
        // Its data paths remain owner-qualified, including --slug observers.
        let owner = if self.root.as_os_str().is_empty() {
            crate::main_claim::MainClaims::system()
                .and_then(|r| r.current())
                .ok()
                .flatten()
                .map(|c| c.owner)
        } else {
            self.identity().ok().map(|id| id.owner)
        };
        owner
            .map(|o| format!("main--{}", &digest(o.as_bytes())[7..39]))
            .unwrap_or_else(|| "unclaimed-main".into())
    }

    /// Mutating/reusing state requires a matching receipt. Never import legacy
    /// nonempty state by its path or by the display name alone.
    pub fn ensure_state(&self) -> Result<PathBuf> {
        let path = self.state_dir();
        crate::ownership::ensure_directory(&path, &self.identity()?)?;
        Ok(path)
    }

    pub fn ensure_runtime(&self) -> Result<PathBuf> {
        let path = self.runtime_dir();
        crate::ownership::ensure_directory(&path, &self.identity()?)?;
        Ok(path)
    }

    pub fn is_implicit_image(&self, image: &str) -> bool {
        let own = self.image_ref();
        image == own || image.strip_prefix(&own).is_some_and(|s| s.starts_with('.'))
    }

    pub fn verify_implicit_image(&self, image: &str) -> Result<()> {
        if self.is_implicit_image(image) {
            crate::ownership::require_image(self, image)?;
        }
        Ok(())
    }
}

fn allocation(root: &Path, worktree: Option<&str>) -> Result<CheckoutIdentity> {
    let common = crate::cmd::probe(git::git(root).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]))
    .context("identifying the canonical Git repository")?;
    let common = std::fs::canonicalize(&common).unwrap_or_else(|_| PathBuf::from(common));
    let repository_id = digest(native_path(&common).as_bytes());
    let origin = crate::cmd::probe(git::git(root).args(["config", "--get", "remote.origin.url"]));
    let mut is_main = false;
    if origin
        .as_deref()
        .is_some_and(crate::main_claim::canonical_origin)
        && common.file_name().is_some_and(|n| n == ".git")
        && let Some(primary) = common.parent()
        && primary.join(".git").is_dir()
    {
        // Even a linked-worktree call registers Git's actual primary. No
        // working-tree files or local Git configuration are changed.
        let candidate = CheckoutIdentity::new(
            repository_id.clone(),
            native_path(primary),
            "primary",
            false,
        );
        let allocated =
            match crate::main_claim::MainClaims::system()?.allocate(candidate.clone(), primary) {
                Ok(allocated) => allocated,
                // Metadata/build commands also run unprivileged (including CI).
                // Without access to machine state, keep the collision-safe owner
                // namespace. Never guess main or require a privilege prompt.
                Err(error)
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
                    }) =>
                {
                    candidate
                }
                Err(error) => return Err(error),
            };
        is_main =
            worktree.is_none() && native_path(root) == allocated.checkout_id && allocated.is_main;
    }
    let name = worktree
        .or_else(|| root.file_name().and_then(|s| s.to_str()))
        .unwrap_or("checkout");
    Ok(CheckoutIdentity::new(
        repository_id,
        native_path(root),
        name,
        is_main,
    ))
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
        bail!(
            "image tag is {} bytes, exceeding the 128-byte limit; use a shorter selection or explicit image tag",
            tag.len()
        );
    }
    Ok(())
}

/// Machine-wide runtime root shared by every checkout: `$XDG_RUNTIME_DIR/ilex-vm`.
/// VM slot locks live directly in here.
pub fn vm_runtime_root() -> PathBuf {
    if let Some(dir) = env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("ilex-vm");
    }
    #[cfg(target_os = "linux")]
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
    #[cfg(target_os = "windows")]
    if let Some(dir) = env::var_os("LOCALAPPDATA") {
        return PathBuf::from(dir).join("ilex").join("checkouts");
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

/// Canonical native path key. WSL translation belongs to this compatibility
/// backend, never to core. Windows path identity is case-insensitive.
pub(crate) fn native_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let path = path.strip_prefix("//?/").unwrap_or(&path);
    if let Some(rest) = path.strip_prefix("/mnt/") {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b'/' {
            return format!("{}:/{}", &rest[..1], &rest[2..]).to_ascii_lowercase();
        }
    }
    if path.as_bytes().get(1) == Some(&b':') {
        path.to_ascii_lowercase()
    } else {
        path.to_string()
    }
}
