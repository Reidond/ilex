//! Builder output is data, never a Windows path or allocation instruction.
use anyhow::{Context, Result, ensure};
use ilex_tooling_windows::{
    received::{self, Directory},
    transport,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::PathBuf,
};

pub const MAX_LOG: u64 = 256 * 1024 * 1024;
const MAX_FILE: u64 = 128 * 1024 * 1024 * 1024;
const MAX_TOTAL: u64 = 256 * 1024 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

pub fn manifest(value: &Value) -> Result<Vec<Artifact>> {
    let values = value.as_array().context("remote artifact manifest")?;
    ensure!(values.len() <= 8192, "artifact count limit");
    let mut names = BTreeSet::new();
    let mut total = 0u64;
    let mut artifacts = Vec::new();
    for value in values {
        let artifact: Artifact = serde_json::from_value(value.clone())?;
        received::relative(&artifact.path)?;
        let parts: Vec<_> = artifact.path.split('/').collect();
        ensure!(parts.len() >= 2, "artifact prefix missing");
        let leaf = *parts.last().unwrap();
        ensure!(
            parts
                .iter()
                .all(|part| !part.to_ascii_lowercase().starts_with(".ilex")
                    && !matches!(
                        part.to_ascii_lowercase().as_str(),
                        ".git"
                            | ".ssh"
                            | ".gnupg"
                            | ".env"
                            | ".aws"
                            | ".azure"
                            | ".docker"
                            | ".kube"
                            | ".netrc"
                            | "_netrc"
                            | ".npmrc"
                            | ".pypirc"
                            | ".git-credentials"
                            | "auth.json"
                            | "credentials"
                            | "ilex-secrets"
                    )
                    && !part.to_ascii_lowercase().starts_with(".env.")
                    && ![
                        ".ilex-owner",
                        ".lock",
                        ".partial",
                        ".key",
                        ".pem",
                        ".pfx",
                        ".p12"
                    ]
                    .iter()
                    .any(|suffix| part.to_ascii_lowercase().ends_with(suffix))),
            "reserved artifact metadata/credential name"
        );
        let allowed = match parts[0] {
            "job" => {
                parts.len() == 2
                    && matches!(
                        leaf,
                        "stdout.log"
                            | "stderr.log"
                            | "state.json"
                            | "request.json"
                            | "context.json"
                            | "image.json"
                    )
            }
            "artifacts" => true,
            "iso" => [".iso", ".sha256", ".json", ".md"]
                .iter()
                .any(|s| leaf.ends_with(s)),
            "disks" => {
                parts.len() == 2
                    && [".qcow2", ".raw", ".disk.json"]
                        .iter()
                        .any(|s| leaf.ends_with(s))
            }
            "tools" => {
                parts.len() == 3
                    && ((parts[1] == "images"
                        && (leaf.ends_with(".raw") || leaf.ends_with(".crt")))
                        || (parts[1] == "keys" && leaf.ends_with(".crt")))
            }
            "jobs" => {
                parts.len() == 3
                    && parts[1].len() == 64
                    && parts[1].bytes().all(|b| b.is_ascii_hexdigit())
                    && matches!(leaf, "request.json" | "result.json")
            }
            "exports" => {
                parts.len() == 2 && (leaf.ends_with(".oci.tar") || leaf.ends_with(".sha256"))
            }
            _ => false,
        };
        ensure!(allowed, "unsupported artifact path: {}", artifact.path);
        ensure!(
            artifact.sha256.len() == 71
                && artifact.sha256.starts_with("sha256:")
                && artifact.sha256[7..]
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid artifact checksum"
        );
        ensure!(artifact.bytes <= MAX_FILE, "artifact file size limit");
        if [".json", ".md", ".sha256", ".crt"]
            .iter()
            .any(|suffix| leaf.ends_with(suffix))
        {
            ensure!(
                artifact.bytes <= 32 * 1024 * 1024,
                "artifact metadata size limit"
            );
        }
        if parts[0] == "job" {
            ensure!(
                artifact.bytes
                    <= if leaf.ends_with(".log") {
                        MAX_LOG
                    } else {
                        32 * 1024 * 1024
                    },
                "job artifact size limit"
            );
        }
        total = total
            .checked_add(artifact.bytes)
            .context("artifact size overflow")?;
        ensure!(total <= MAX_TOTAL, "artifact aggregate size limit");
        ensure!(
            names.insert(artifact.path.to_ascii_lowercase()),
            "duplicate artifact path"
        );
        artifacts.push(artifact);
    }
    // Reject file/directory and case aliases before opening even the first file.
    for name in &names {
        for (at, _) in name.match_indices('/') {
            ensure!(
                !names.contains(&name[..at]),
                "artifact file/directory collision"
            );
        }
    }
    Ok(artifacts)
}

pub fn collect(
    directory: &Directory,
    value: &Value,
    mut chunk: impl FnMut(&str, u64) -> Result<Vec<u8>>,
) -> Result<BTreeMap<String, PathBuf>> {
    let artifacts = manifest(value)?;
    let mut delivered = BTreeMap::new();
    for artifact in artifacts {
        ensure!(
            transport::available_space(&directory.path)?
                >= artifact.bytes + 30 * 1024 * 1024 * 1024,
            "artifact transfer requires its size plus 30 GiB free on Windows"
        );
        let mut output = directory.create(&artifact.path)?;
        let mut offset = 0u64;
        while offset < artifact.bytes {
            let data = chunk(&artifact.path, offset)?;
            ensure!(
                !data.is_empty()
                    && data.len() <= ilex_devkit::core::CHUNK_BYTES
                    && data.len() as u64 <= artifact.bytes - offset,
                "artifact chunk exceeds declared size"
            );
            output.file.write_all(&data)?;
            offset += data.len() as u64;
        }
        output.file.sync_all()?;
        ensure!(
            received::sha256(&mut output.file)? == artifact.sha256,
            "artifact checksum mismatch; incomplete receive retained"
        );
        delivered.insert(artifact.path.clone(), directory.path.join(&artifact.path));
    }
    Ok(delivered)
}

pub fn self_check(ws: &ilex_devkit::Workspace) -> Result<()> {
    use std::{fs, process::Command};
    let state = ws.ensure_state()?;
    let error = super::client::protect_output(ws, &state.join("destinations.json"))
        .expect_err("accepted explicit state output");
    ensure!(
        error.to_string().contains("tooling state/control"),
        "state-path check did not run"
    );
    let root = std::env::temp_dir().join(format!(
        "ilex-receiver-self-check-{}",
        super::client::job_uuid()?
    ));
    fs::create_dir(&root)?;
    let control = root.join("control");
    fs::create_dir(&control)?;
    let sentinel = control.join("destinations.json");
    fs::write(&sentinel, b"locally authored sentinel")?;
    fs::create_dir(root.join("received"))?;
    fs::hard_link(&sentinel, root.join("received/hardlink"))?;
    let directory = Directory::open(&root.join("received"))?;
    let bytes = b"verified artifact\n";
    let good = json!({"path":"artifacts/valid.log","sha256":ilex_devkit::core::digest(bytes),"bytes":bytes.len()});
    let mut refused = 0;
    for path in [
        "job/destinations.json",
        "remote/jobs/redirect.json",
        "artifacts/../destinations.json",
        "/absolute",
        "C:/absolute",
        "artifacts/a:ads",
        "artifacts/a\\b",
        "artifacts/CON.txt",
        "artifacts/CON .txt",
        "artifacts/com1 .json",
        "artifacts/a.",
        "artifacts//empty",
        "iso/.ilex-owner.json",
        "job/result.json",
        "artifacts/.git/config",
        "artifacts/auth.json",
    ] {
        let mut evil = good.clone();
        evil["path"] = json!(path);
        ensure!(
            collect(&directory, &json!([good, evil]), |_, _| anyhow::bail!(
                "invalid manifest reached fetch"
            ))
            .is_err(),
            "accepted malicious artifact {path}"
        );
        ensure!(
            !directory.path.join("artifacts/valid.log").exists(),
            "manifest was not validated in full before writing"
        );
        refused += 1;
    }
    for evil in [
        json!({"path":"artifacts/link","sha256":ilex_devkit::core::digest(b""),"bytes":0,"kind":"symlink"}),
        json!({"path":"artifacts/huge","sha256":ilex_devkit::core::digest(b""),"bytes":MAX_FILE + 1}),
    ] {
        ensure!(
            manifest(&json!([evil])).is_err(),
            "accepted link/oversized artifact"
        );
        refused += 1;
    }
    ensure!(
        manifest(&json!([good, good])).is_err(),
        "accepted duplicate artifact"
    );
    let mut other = good.clone();
    other["path"] = json!("artifacts/VALID.log");
    ensure!(
        manifest(&json!([good, other])).is_err(),
        "accepted case alias"
    );
    let mut child = good.clone();
    child["path"] = json!("artifacts/valid.log/child");
    ensure!(
        manifest(&json!([good, child])).is_err(),
        "accepted file/directory alias"
    );
    ensure!(
        manifest(&json!(vec![good.clone(); 8193])).is_err(),
        "accepted artifact count over limit"
    );
    let large: Vec<_> = (0..3).map(|i| json!({"path":format!("artifacts/large-{i}"),"sha256":ilex_devkit::core::digest(b""),"bytes":MAX_FILE})).collect();
    ensure!(
        manifest(&json!(large)).is_err(),
        "accepted aggregate size over limit"
    );
    for (path, size) in [
        ("job/stdout.log", MAX_LOG + 1),
        ("artifacts/meta.json", 32 * 1024 * 1024 + 1),
    ] {
        ensure!(
            manifest(&json!([{"path":path,"bytes":size,"sha256":ilex_devkit::core::digest(b"")}]))
                .is_err(),
            "accepted oversized log/metadata"
        );
    }
    collect(&directory, &json!([good]), |_, _| Ok(bytes.to_vec()))?;
    let certificate = json!({"path":"tools/images/drupe.crt","sha256":ilex_devkit::core::digest(bytes),"bytes":bytes.len()});
    collect(&directory, &json!([certificate]), |_, _| Ok(bytes.to_vec()))?;
    for value in [b"first".as_slice(), b"second".as_slice()] {
        let mut log = directory.append("logs/stdout.log")?;
        log.file.write_all(value)?;
        log.file.sync_all()?;
    }
    ensure!(
        fs::read(directory.path.join("logs/stdout.log"))? == b"firstsecond",
        "native append stream mismatch"
    );
    ensure!(
        fs::read(directory.path.join("artifacts/valid.log"))? == bytes,
        "positive receiver mismatch"
    );
    let overrun = directory.child("overrun")?;
    ensure!(
        collect(&overrun, &json!([good]), |_, _| Ok(vec![
            0;
            bytes.len() + 1
        ]))
        .is_err(),
        "accepted oversized chunk"
    );
    let link = directory.path.join("artifacts/junction");
    let status = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path $env:ILEX_CHECK_LINK -Target $env:ILEX_CHECK_TARGET | Out-Null"])
        .env("ILEX_CHECK_LINK", &link).env("ILEX_CHECK_TARGET", &control).status()?;
    ensure!(status.success(), "creating junction fault fixture");
    let mut redirect = good.clone();
    redirect["path"] = json!("artifacts/junction/injected.log");
    ensure!(
        collect(&directory, &json!([redirect]), |_, _| Ok(bytes.to_vec())).is_err(),
        "receiver followed junction"
    );
    ensure!(
        !control.join("injected.log").exists(),
        "junction redirected a receiver write"
    );
    fs::remove_dir(&link)?;
    ensure!(
        directory.append("hardlink").is_err(),
        "opened hard link for writing"
    );
    let held = directory.child("held")?;
    let target = Directory::open(&control)?;
    let reparse_race = received::self_check_reparse_race(&held, &target)?;
    drop(target);
    ensure!(
        fs::rename(&held.path, directory.path.join("moved")).is_err(),
        "directory could be replaced while handles were held"
    );
    drop(held);
    ensure!(
        fs::read(&sentinel)? == b"locally authored sentinel",
        "control metadata changed"
    );
    let mut original = directory.read("artifacts/valid.log")?;
    let mut copy = directory.create("copy.partial")?;
    std::io::copy(&mut original.file, &mut copy.file)?;
    copy.file.sync_all()?;
    received::rename(&copy, "copy.log")?;
    drop(copy);
    ensure!(
        fs::read(directory.path.join("copy.log"))? == bytes,
        "handle rename mismatch"
    );
    let mut replacement = directory.create("replacement.partial")?;
    replacement.file.write_all(b"replacement\n")?;
    replacement.file.sync_all()?;
    received::rename(&replacement, "copy.log")?;
    drop(replacement);
    ensure!(
        fs::read(directory.path.join("copy.log"))? == b"replacement\n",
        "existing output replacement mismatch"
    );
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"invalid_manifests_refused":refused,"complete_manifest_validation":true,"positive_receive":true,"native_append_stream":true,"atomic_existing_replacement":true,"chunk_limit":true,"count_and_aggregate_limits":true,"log_and_metadata_limits":true,"duplicate_case_and_directory_aliases":true,"receiver_junction_refused":true,"in_place_reparse_race":reparse_race,"hardlink_refused":true,"held_directory_cannot_be_replaced":true,"control_sentinel_unchanged":true,"handle_rename":true,"explicit_state_output_refused":true,"root":root})
        )?
    );
    drop(original);
    drop(overrun);
    drop(directory);
    let resolved = fs::canonicalize(&root)?;
    let temporary = fs::canonicalize(std::env::temp_dir())?;
    ensure!(
        resolved.parent() == Some(temporary.as_path()) && resolved.file_name() == root.file_name(),
        "self-check cleanup target escaped its temporary directory"
    );
    fs::remove_dir_all(resolved)?;
    Ok(())
}
