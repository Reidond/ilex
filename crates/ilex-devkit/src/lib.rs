//! Shared host-side plumbing for ilex's tooling: `xtask`, the `ilex-e2e`
//! harness and `ilex-vmctl` all agree on the checkout's slug, the image tag
//! and VM names derived from it, where per-worktree state lives, and the
//! machine-wide VM slot cap.

pub mod cmd;
pub mod git;
pub mod slots;
pub mod workspace;

pub use workspace::{IMAGE_REPO, Workspace};
