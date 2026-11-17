//! Attach without coupling the native controller to Linux fixture types.
#[cfg(not(target_os = "windows"))]
pub use ilex_devkit::qemu::{VmInfo, find_vm, running_vms};
#[cfg(target_os = "windows")]
pub use ilex_tooling_windows::qemu::VmInfo;
#[cfg(target_os = "windows")]
pub fn running_vms(ws: &ilex_devkit::Workspace) -> Vec<VmInfo> {
    if ws.root.as_os_str().is_empty() {
        ilex_tooling_windows::qemu::running_vms(&ws.runtime_dir(), None)
    } else if let Ok(identity) = ws.identity() {
        ilex_tooling_windows::qemu::running_vms(&ws.runtime_dir(), Some(&identity.owner))
    } else {
        Vec::new()
    }
}
#[cfg(target_os = "windows")]
pub fn find_vm(ws: &ilex_devkit::Workspace, purpose: Option<&str>) -> anyhow::Result<VmInfo> {
    let mut vms = running_vms(ws);
    if let Some(purpose) = purpose {
        vms.retain(|v| v.purpose == purpose);
    }
    anyhow::ensure!(
        vms.len() == 1,
        "expected one running native VM for {}; found {} (choose --vm)",
        ws.slug,
        vms.len()
    );
    Ok(vms.remove(0))
}
