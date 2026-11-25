//! Private scratch directories: made fresh for one call, owned by the helper's
//! user, mode 0700, never a symlink, removed (empty) when the call ends. A
//! directory the helper mounts something at is never a predictable path that
//! another process, or another run of the helper, could also be using.

use crate::sys;
use anyhow::{Context, Result, bail, ensure};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// The user the helper runs as (`/proc/self` belongs to it).
fn euid() -> Result<u32> {
    Ok(fs::metadata("/proc/self")?.uid())
}

/// `dir` must be a real directory, ours, that nobody else can write to.
fn check_parent(dir: &Path) -> Result<()> {
    let meta =
        fs::symlink_metadata(dir).with_context(|| format!("looking at {}", dir.display()))?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "{} is not a plain directory",
        dir.display()
    );
    ensure!(
        meta.uid() == euid()?,
        "{} belongs to another user",
        dir.display()
    );
    ensure!(
        meta.permissions().mode() & 0o022 == 0,
        "{} is writable by others",
        dir.display()
    );
    Ok(())
}

/// A directory made for one call and removed when it is dropped. Directories
/// made inside it with [`Scratch::subdir`] are removed first; only empty ones
/// are ever removed (never recursively: if something were still mounted in
/// one, removal would fail rather than delete what is mounted there).
pub struct Scratch {
    path: PathBuf,
    made: Vec<PathBuf>,
}

impl Scratch {
    /// `<parent>/<prefix>-<random>`, created with mode 0700. `parent` is
    /// created (0700) when missing and must be ours and not writable by
    /// others.
    pub fn new(parent: &Path, prefix: &str) -> Result<Self> {
        if !parent.exists() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        check_parent(parent)?;
        for _ in 0..16 {
            let name = format!("{prefix}-{}", sys::hex(&sys::random_bytes::<8>()?));
            let path = parent.join(name);
            // mkdir never follows a symlink at the last component and fails
            // if anything is there.
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let scratch = Self {
                        path,
                        made: Vec::new(),
                    };
                    scratch.verify(&scratch.path)?;
                    return Ok(scratch);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => {
                    return Err(e).with_context(|| format!("creating {}", path.display()));
                }
            }
        }
        bail!("no unused scratch name under {}", parent.display())
    }

    /// What `mkdir` made is what was asked for: ours, 0700, no link.
    fn verify(&self, path: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            meta.is_dir()
                && !meta.file_type().is_symlink()
                && meta.uid() == euid()?
                && meta.permissions().mode() & 0o777 == 0o700,
            "{} is not the private directory that was just made",
            path.display()
        );
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// A new private directory inside this one (a mount point).
    pub fn subdir(&mut self, name: &str) -> Result<PathBuf> {
        ensure!(
            !name.is_empty() && !name.contains('/') && name != "." && name != "..",
            "bad scratch name {name:?}"
        );
        let path = self.path.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        self.verify(&path)?;
        self.made.push(path.clone());
        Ok(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        for path in self.made.iter().rev() {
            let _ = fs::remove_dir(path);
        }
        let _ = fs::remove_dir(&self.path);
    }
}
