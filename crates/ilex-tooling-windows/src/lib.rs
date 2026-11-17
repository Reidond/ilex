//! Windows-specific acquisition and provisioning. No WSL or machine-wide setup.
#[cfg(target_os = "windows")]
pub mod builder;
#[cfg(target_os = "windows")]
mod host;
#[cfg(target_os = "windows")]
pub mod runtime;
#[cfg(target_os = "windows")]
pub mod transport;

#[cfg(not(target_os = "windows"))]
pub fn unsupported() -> anyhow::Result<()> {
    anyhow::bail!(
        "runtime and builder commands require a Windows x86_64 host; native Linux execution remains local"
    )
}
