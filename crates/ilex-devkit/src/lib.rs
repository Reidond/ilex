//! Compatibility facade for the portable contracts and Linux backend.
pub use ilex_installer_model as installer_model;
pub use ilex_tooling_core as core;
pub use ilex_tooling_linux::fingerprint;
pub use ilex_tooling_linux::main_claim;
pub use ilex_tooling_linux::ownership;
pub use ilex_tooling_linux::{
    IMAGE_REPO, Workspace, bcvk, cmd, config, executor, git, image, installer, machine, provider,
    qemu, qmp, qualification, security, slots, source, tool, workspace,
};

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
