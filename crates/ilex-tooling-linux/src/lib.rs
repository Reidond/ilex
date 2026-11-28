//! Existing Linux backend. The facade preserves public paths during migration.
pub mod bcvk;
pub mod cmd;
pub mod executor;
pub mod fingerprint;
pub mod git;
pub mod image;
mod image_crates_guard;
pub mod machine;
pub mod main_claim;
pub mod ownership;
pub mod provider;
pub mod qemu;
pub mod qmp;
pub mod qualification;
pub mod security;
pub mod slots;
pub mod source;
pub mod tool;
pub mod workspace;
pub use ilex_tooling_core::{config, installer};
pub use workspace::{IMAGE_REPO, Workspace};

pub fn require_linux() -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "this operation requires the Linux tooling backend"
    );
    Ok(())
}
