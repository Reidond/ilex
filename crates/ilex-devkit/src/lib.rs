//! Compatibility facade for the portable contracts and existing Linux/WSL paths.
//! Native Windows hosting arrives in later PRs; the WSL handoff/viewer stay.
pub use ilex_tooling_core as core;
pub use ilex_tooling_linux::fingerprint;
pub use ilex_tooling_linux::ownership;
pub use ilex_tooling_linux::main_claim;
pub use ilex_tooling_linux::{
    IMAGE_REPO, Workspace, bcvk, cmd, config, executor, git, image, installer, machine, provider,
    qemu, qmp, slots, source, tool, viewer, workspace,
};
#[cfg(target_os = "linux")]
pub use ilex_tooling_linux::{display, vdagent};

#[derive(Debug, Clone, Copy)]
pub enum HostBackend {
    Linux,
    WslCompatibility,
    Unsupported,
}
pub fn host_backend() -> HostBackend {
    if cfg!(target_os = "linux") {
        HostBackend::Linux
    } else if cfg!(target_os = "windows") {
        HostBackend::WslCompatibility
    } else {
        HostBackend::Unsupported
    }
}
