//! Ownership fences for local resources. Names are hints; receipts authorize
//! reuse. Unlabelled legacy resources require explicit owner-managed migration.
use crate::{Workspace, cmd};
use anyhow::{Context, Result, ensure};
use ilex_tooling_core::{CheckoutIdentity, SCHEMA_VERSION, check_version, digest};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Serialize, Deserialize)]
struct Receipt {
    schema: u32,
    owner: String,
}

pub fn lock(path: &Path) -> Result<File> {
    fs::create_dir_all(path.parent().context("lock has no parent")?)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

pub fn ensure_directory(path: &Path, identity: &CheckoutIdentity) -> Result<()> {
    let parent = path.parent().context("resource directory has no parent")?;
    let key = digest(path.to_string_lossy().as_bytes());
    let _lock = lock(&parent.join(format!(".owner-{}.lock", &key[7..23])))?;
    if path.exists() {
        ensure!(
            !fs::symlink_metadata(path)?.is_symlink(),
            "resource directory is a symlink: {}",
            path.display()
        );
    }
    let marker = path.join(".ilex-owner.json");
    if marker.exists() {
        let receipt: Receipt = serde_json::from_slice(&fs::read(&marker)?)?;
        check_version(receipt.schema)?;
        identity.authorize(&receipt.owner)?;
        return Ok(());
    }
    if path.exists() {
        ensure!(
            fs::read_dir(path)?.next().is_none(),
            "unlabelled legacy state at {}; refusing adoption; use fresh state or explicitly inspect/migrate only your own data",
            path.display()
        );
    } else {
        fs::create_dir_all(path)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&marker)?;
    file.write_all(&serde_json::to_vec(&Receipt {
        schema: SCHEMA_VERSION,
        owner: identity.owner.clone(),
    })?)?;
    file.sync_all()?;
    Ok(())
}

pub fn image_owner(image: &str) -> Result<Option<String>> {
    let labels = cmd::output(Command::new("podman").args([
        "image",
        "inspect",
        "--format",
        "{{json .Config.Labels}}",
        image,
    ]))?;
    let labels: serde_json::Value = serde_json::from_str(&labels)?;
    Ok(labels["ilex.checkout-owner"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

pub fn require_image(ws: &Workspace, image: &str) -> Result<()> {
    let owner = image_owner(image)?.with_context(|| format!("unlabelled legacy image {image}; refusing adoption or replacement; inspect/migrate only your own image explicitly, or use a fresh owned tag"))?;
    ws.identity()?
        .authorize(&owner)
        .with_context(|| format!("image {image} belongs to another checkout"))
}

pub fn check_output(ws: &Workspace, image: &str) -> Result<()> {
    if crate::bcvk::image_exists(image) {
        require_image(ws, image)?;
    }
    Ok(())
}

/// Call under the resource's writer lock. A pre-existing file needs a receipt;
/// a missing file may be allocated by this owner before its first writer starts.
pub fn claim_file(ws: &Workspace, path: &Path) -> Result<()> {
    let marker = path.with_file_name(format!(
        "{}.ilex-owner",
        path.file_name()
            .context("resource filename missing")?
            .to_string_lossy()
    ));
    if marker.exists() {
        let receipt: Receipt = serde_json::from_slice(&fs::read(&marker)?)?;
        check_version(receipt.schema)?;
        ws.identity()?.authorize(&receipt.owner)?;
    } else {
        ensure!(
            !path.exists(),
            "unlabelled legacy disk/cache {}; refusing adoption or replacement; choose a fresh output or explicitly migrate your own data",
            path.display()
        );
        fs::write(
            marker,
            serde_json::to_vec(&Receipt {
                schema: SCHEMA_VERSION,
                owner: ws.identity()?.owner,
            })?,
        )?;
    }
    Ok(())
}

/// One final-tag writer across all checkout owners in this local store.
/// Only this job's temporary alias is removed on any error, including a failed
/// post-build inventory. An earlier accepted final alias remains untouched.
pub struct ImageTransaction {
    final_ref: String,
    temporary: String,
    owner: String,
    _lock: File,
}
impl ImageTransaction {
    pub fn new(ws: &Workspace, final_ref: &str) -> Result<Self> {
        ws.ensure_state()?;
        let key = digest(final_ref.as_bytes());
        let file = lock(
            &crate::workspace::vm_runtime_root()
                .join("image-locks")
                .join(format!("{}.lock", &key[7..])),
        )?;
        check_output(ws, final_ref)?;
        let owner = ws.identity()?.owner;
        let nonce = digest(
            format!(
                "{owner}:{}:{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
            )
            .as_bytes(),
        );
        let temporary = format!("localhost/ilex-job:{}", &nonce[7..]);
        ensure!(
            !crate::bcvk::image_exists(&temporary),
            "temporary image alias already exists"
        );
        Ok(Self {
            final_ref: final_ref.into(),
            temporary,
            owner,
            _lock: file,
        })
    }
    pub fn temporary(&self) -> &str {
        &self.temporary
    }
    pub fn promote(&self, ws: &Workspace) -> Result<String> {
        let actual = image_owner(&self.temporary)?.context("temporary image has no owner label")?;
        ensure!(
            actual == self.owner,
            "temporary image owner was overridden; refusing promotion"
        );
        check_output(ws, &self.final_ref)?;
        cmd::run(Command::new("podman").args(["tag", &self.temporary, &self.final_ref]))?;
        Ok(self.final_ref.clone())
    }
}
impl Drop for ImageTransaction {
    fn drop(&mut self) {
        let _ = Command::new("podman")
            .args(["untag", &self.temporary, &self.temporary])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Named VM operations keep a checkout receipt and an exclusive lifetime
/// lease outside the runtime directory that QEMU recreates for each boot.
pub fn vm_lease(ws: &Workspace, name: &str) -> Result<File> {
    ensure!(
        name.starts_with(&ws.vm_prefix()) && !name.contains(['/', '\\']),
        "VM name {name} is outside this checkout's owner-qualified namespace"
    );
    let root = ws.ensure_runtime()?;
    let key = digest(name.as_bytes());
    let path = root.join("leases").join(format!("{}.lock", &key[7..]));
    fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    file.try_lock()
        .with_context(|| format!("{name} is already operated by another launcher"))?;
    Ok(file)
}

pub fn validate_build_args(args: &[String]) -> Result<()> {
    for arg in args {
        ensure!(
            !["--tag", "--manifest", "--output", "--iidfile"]
                .iter()
                .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
                && !(arg.starts_with('-')
                    && !arg.starts_with("--")
                    && (arg.contains('t') || arg.starts_with("-o"))),
            "build output flags are reserved for transactional promotion: {arg}"
        );
    }
    Ok(())
}
