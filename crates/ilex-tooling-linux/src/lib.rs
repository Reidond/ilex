//! Existing Linux backend. The facade preserves public paths during migration.
pub mod bcvk;
pub mod cmd;
#[cfg(target_os = "linux")]
pub mod display;
pub mod executor;
pub mod fingerprint;
pub mod git;
pub mod image;
pub mod machine;
pub mod main_claim;
pub mod ownership;
pub mod provider;
pub mod qualification;
pub mod qemu;
pub mod qmp;
pub mod security;
pub mod slots;
pub mod source;
pub mod tool;
#[cfg(target_os = "linux")]
pub mod vdagent;
pub mod viewer;
pub mod workspace;
pub use ilex_tooling_core::{config, installer};
pub use workspace::{IMAGE_REPO, Workspace};

pub fn require_linux() -> anyhow::Result<()> {
    anyhow::ensure!(
        cfg!(target_os = "linux"),
        "Linux tooling backend requires Linux; on Windows use the existing FedoraLinux-45 WSL path (see CLAUDE.md)"
    );
    Ok(())
}
