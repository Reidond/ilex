//! Compatibility re-export of the backend's QMP transport.
#[cfg(not(target_os = "windows"))]
pub use ilex_devkit::qmp::Qmp;
#[cfg(target_os = "windows")]
pub use ilex_tooling_windows::qmp::Qmp;
