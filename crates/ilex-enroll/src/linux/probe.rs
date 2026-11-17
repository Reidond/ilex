use super::*;
use serde::Serialize;
use std::{collections::BTreeMap, os::unix::fs::FileTypeExt};

#[derive(Serialize)]
pub struct Probe {
    pub schema: u32,
    pub secure_boot: Option<bool>,
    pub setup_mode: Option<bool>,
    pub tpm2: bool,
    pub pcr7: Option<String>,
    pub replay: Option<crate::pcr::Pcr7Replay>,
    pub tools: BTreeMap<String, String>,
    pub verdict: &'static str,
    pub reason: &'static str,
}
fn efi(name: &str) -> Option<bool> {
    let data = fs::read(format!(
        "/sys/firmware/efi/efivars/{name}-8be4df61-93ca-11d2-aa0d-00e098032b8c"
    ))
    .ok()?;
    (data.len() == 5 && data[4] <= 1).then(|| data[4] == 1)
}
pub fn tool_versions(root: &Path) -> Result<BTreeMap<String, String>> {
    // Query target RPM metadata rather than executing an untrusted target binary.
    let mut versions = BTreeMap::new();
    for package in ["systemd", "systemd-udev", "cryptsetup-libs", "cryptsetup"] {
        let out = run(Command::new("/usr/bin/rpm").arg("--root").arg(root).args([
            "-q",
            "--qf",
            "%{VERSION}-%{RELEASE}.%{ARCH}",
            package,
        ]))?;
        versions.insert(package.into(), String::from_utf8(out.stdout)?.trim().into());
    }
    Ok(versions)
}
pub fn probe() -> Probe {
    let mut result = Probe {
        schema: 1,
        secure_boot: efi("SecureBoot"),
        setup_mode: efi("SetupMode"),
        tpm2: fs::read_to_string("/sys/class/tpm/tpm0/tpm_version_major")
            .is_ok_and(|s| s.trim() == "2")
            && fs::metadata("/dev/tpmrm0").is_ok_and(|m| m.file_type().is_char_device()),
        pcr7: None,
        replay: None,
        tools: tool_versions(Path::new("/")).unwrap_or_default(),
        verdict: "unavailable",
        reason: "secure-boot-off",
    };
    if result.secure_boot != Some(true) || result.setup_mode != Some(false) {
        return result;
    }
    result.reason = "no-tpm";
    if !result.tpm2 {
        return result;
    }
    result.reason = "pcr7-unmeasured";
    if let Ok(pcr) = fs::read_to_string("/sys/class/tpm/tpm0/pcr-sha256/7") {
        let pcr = pcr.trim().to_ascii_lowercase();
        if pcr.len() == 64
            && pcr.bytes().all(|b| b.is_ascii_hexdigit())
            && pcr.bytes().any(|b| b != b'0')
        {
            result.pcr7 = Some(pcr);
        }
    }
    result.replay = fs::read("/sys/kernel/security/tpm0/binary_bios_measurements")
        .ok()
        .and_then(|log| crate::pcr::replay_pcr7(&log).ok());
    if result.pcr7.is_none() || result.replay.as_ref().map(|r| &r.sha256) != result.pcr7.as_ref() {
        return result;
    }
    result.reason = "tool-mismatch";
    if result.tools.len() != 4
        || result.tools.iter().any(|(name, version)| {
            version
                != match name.as_str() {
                    "systemd" | "systemd-udev" => "262.1-1.fc45.x86_64",
                    "cryptsetup" | "cryptsetup-libs" => "2.8.8-1.fc45.x86_64",
                    _ => "unqualified",
                }
        })
    {
        return result;
    }
    result.verdict = "eligible";
    result.reason = "pcr7-sha256";
    result
}
