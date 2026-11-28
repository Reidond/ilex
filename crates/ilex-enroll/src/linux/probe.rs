use super::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, os::unix::fs::FileTypeExt};

const TOOL_FILES: &[&str] = &[
    "usr/bin/ilex-enroll",
    "usr/bin/systemd-cryptenroll",
    "usr/bin/systemd-cryptsetup",
    "usr/sbin/cryptsetup",
    "usr/lib64/cryptsetup/libcryptsetup-token-systemd-tpm2.so",
    "usr/lib64/libcryptsetup.so.12",
    "usr/lib64/libtss2-esys.so.0",
];

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
    let path = root.join("usr/lib/ilex/security/tools.json");
    if path.exists() {
        // image-builder deliberately omits the live ISO's RPM database. Bind
        // its build-time versions to the actual shipped executables/libraries.
        // The same check reads target files without executing target programs.
        let manifest: Value = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(manifest["schema"] == 1, "unsupported tool metadata");
        ensure!(
            manifest["sha256"]
                .as_object()
                .is_some_and(|m| m.len() == TOOL_FILES.len()),
            "incomplete tool metadata"
        );
        for file in TOOL_FILES {
            ensure!(
                manifest["sha256"][*file] == file_hash(&root.join(file))?,
                "tool metadata does not match installed files"
            );
        }
        return serde_json::from_value(manifest["tools"].clone()).context("invalid tool versions");
    }
    rpm_versions(root)
}
fn rpm_versions(root: &Path) -> Result<BTreeMap<String, String>> {
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
fn file_hash(path: &Path) -> Result<String> {
    let mut hash = Sha256::new();
    std::io::copy(&mut fs::File::open(path)?, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}
/// Build-time metadata only. No TPM or credential is accessed. Generate after
/// installing the final helper and packages, before image-builder drops RPM DB.
pub fn tool_manifest() -> Result<Value> {
    let root = Path::new("/");
    let tools = rpm_versions(root)?;
    let mut packages = tools.clone();
    for package in ["anaconda-core", "shim-x64", "grub2-efi-x64", "dracut"] {
        if let Ok(output) = run(Command::new("/usr/bin/rpm").args([
            "-q",
            "--qf",
            "%{VERSION}-%{RELEASE}.%{ARCH}",
            package,
        ])) {
            packages.insert(
                package.into(),
                String::from_utf8(output.stdout)?.trim().into(),
            );
        }
    }
    let mut hashes = BTreeMap::new();
    for file in TOOL_FILES {
        hashes.insert(*file, file_hash(&root.join(file))?);
    }
    Ok(serde_json::json!({"schema":1, "tools":tools, "packages":packages, "sha256":hashes}))
}
/// The package versions the enrollment was qualified with. A version joins the
/// list only with a qualification run that enrolled and unlocked with it:
/// cryptsetup 2.8.9 (what Fedora's bootc image carries from 2026-11-27) by
/// `iso::next_security`, on KVM with enrolled OVMF and a persistent swtpm.
fn qualified(package: &str, version: &str) -> bool {
    match package {
        "systemd" | "systemd-udev" => version == "262.1-1.fc45.x86_64",
        "cryptsetup" | "cryptsetup-libs" => {
            matches!(version, "2.8.8-1.fc45.x86_64" | "2.8.9-1.fc45.x86_64")
        }
        _ => false,
    }
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
        || result
            .tools
            .iter()
            .any(|(name, version)| !qualified(name, version))
    {
        return result;
    }
    result.verdict = "eligible";
    result.reason = "pcr7-sha256";
    result
}
