//! Compatibility facade for the portable contracts and existing Linux/WSL paths.
//! Native Windows development VMs live in ilex-tooling-windows. The WSL
//! ephemeral handoff/viewer stay until T7.
pub use ilex_tooling_core as core;
pub use ilex_tooling_linux::fingerprint;
pub use ilex_tooling_linux::main_claim;
pub use ilex_tooling_linux::ownership;
pub use ilex_tooling_linux::{
    IMAGE_REPO, Workspace, bcvk, cmd, config, executor, git, image, installer, machine, provider,
    qemu, qmp, qualification, security, slots, source, tool, viewer, workspace,
};
#[cfg(target_os = "linux")]
pub use ilex_tooling_linux::{display, vdagent};

#[derive(Debug, Clone, Copy)]
pub enum HostBackend {
    Linux,
    Windows,
    Unsupported,
}
pub fn host_backend() -> HostBackend {
    if cfg!(target_os = "linux") {
        HostBackend::Linux
    } else if cfg!(target_os = "windows") {
        HostBackend::Windows
    } else {
        HostBackend::Unsupported
    }
}
