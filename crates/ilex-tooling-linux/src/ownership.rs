//! Ownership fences for local resources. Names are hints; receipts authorize
//! reuse. Unlabelled legacy resources require explicit owner-managed migration.
use crate::{Workspace, cmd};
use anyhow::{Context, Result, ensure};
use ilex_tooling_core::{CheckoutIdentity, SCHEMA_VERSION, check_version, digest};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
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
    let owner = image_owner(image)?.with_context(|| format!("unlabelled legacy image {image}; refusing implicit adoption; run cargo xtask build from the registered primary to rebuild :main, or inspect this checkout's image ID and use cargo xtask identity rebuild-legacy-image --expected-id <id>; a fresh owned tag is also supported"))?;
    ws.identity()?
        .authorize(&owner)
        .with_context(|| format!("image {image} belongs to another checkout"))
}

pub fn check_output(ws: &Workspace, image: &str) -> Result<()> {
    check_output_namespace(ws, image)?;
    if image_exists(image)? {
        require_image(ws, image)?;
    }
    Ok(())
}

fn check_output_namespace(ws: &Workspace, image: &str) -> Result<()> {
    let reserved = [
        "localhost/ilex:main",
        "localhost/ilex-installer:main",
        "localhost/ilex-image-builder:main",
    ];
    ensure!(
        !reserved.iter().any(|main| image == *main
            || image
                .strip_prefix(main)
                .is_some_and(|suffix| suffix.starts_with('.')))
            || ws.identity()?.is_main,
        "main image namespace is reserved for the declared canonical primary checkout"
    );
    Ok(())
}

/// Ownership checks must distinguish a missing image from a storage error.
pub fn image_exists(image: &str) -> Result<bool> {
    let status = Command::new("podman")
        .args(["image", "exists", image])
        .status()?;
    ensure!(
        matches!(status.code(), Some(0 | 1)),
        "cannot inspect image ownership for {image}: {status}"
    );
    Ok(status.success())
}

pub fn image_lock(image: &str) -> Result<File> {
    lock(&image_lock_path(image)?)
}

fn image_lock_path(image: &str) -> Result<PathBuf> {
    crate::require_linux()?;
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        // Image aliases belong to the store, not XDG_RUNTIME_DIR. Two clients
        // with different runtime roots must still serialize the same final tag.
        let uid = fs::metadata("/proc/self")?.uid();
        let store =
            cmd::output(Command::new("podman").args(["info", "--format", "{{.Store.GraphRoot}}"]))?;
        let store = fs::canonicalize(store)?;
        let store_key = digest(store.as_os_str().as_encoded_bytes());
        let key = digest(image.as_bytes());
        Ok(
            PathBuf::from(format!("/tmp/ilex-image-locks-{uid}"))
                .join(&store_key[7..])
                .join(format!("{}.lock", &key[7..])),
        )
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("image store locks for {image} require Linux")
}

#[derive(Serialize, Deserialize)]
struct ImageAliasReceipt {
    schema: u32,
    owner: String,
    image: String,
    id: String,
}

/// A receipt authorizes an immutable alias even when its explicit source has
/// foreign/CI labels. Keep this guard alive while a consumer uses the alias.
/// Existing aliases always require a matching allocation receipt; matching
/// contents or labels alone never authorize adoption or replacement.
pub struct ImageAlias {
    image: String,
    id: String,
    _lease: File,
}

impl ImageAlias {
    pub fn for_image_id(ws: &Workspace, image: &str, source: &str) -> Result<Self> {
        ensure!(
            image.starts_with(&format!("{}-", ws.image_ref())),
            "image alias is outside this checkout's namespace"
        );
        let digest = source
            .strip_prefix("sha256:")
            .context("expected an immutable image ID")?;
        ensure!(
            digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid source image ID"
        );
        ws.ensure_state()?;
        let lock_path = image_lock_path(image)?;
        let lease = lock(&lock_path)?;
        // Bind receipts to the same local store/UID namespace as the lease,
        // even if the checkout is used with more than one Podman store.
        let key = ilex_tooling_core::digest(lock_path.as_os_str().as_encoded_bytes());
        let receipt_path = ws
            .state_dir()
            .join("image-aliases")
            .join(format!("{}.json", &key[7..]));
        let expected = ImageAliasReceipt {
            schema: SCHEMA_VERSION,
            owner: ws.identity()?.owner,
            image: image.into(),
            id: digest.to_ascii_lowercase(),
        };
        let exists = image_exists(image)?;
        let has_receipt = receipt_path.try_exists()?;
        if has_receipt {
            let receipt: ImageAliasReceipt = serde_json::from_slice(&fs::read(&receipt_path)?)?;
            check_version(receipt.schema)?;
            ws.identity()?.authorize(&receipt.owner)?;
            ensure!(
                receipt.image == expected.image && receipt.id == expected.id,
                "image alias allocation receipt does not match requested alias/source"
            );
        } else {
            ensure!(
                !exists,
                "existing image alias {image} has no allocation receipt; refusing adoption or replacement"
            );
        }
        let alias = Self {
            image: image.into(),
            id: expected.id.clone(),
            _lease: lease,
        };
        if exists {
            alias.verify()?;
        }
        // Validate the immutable input before any promotion. An inspect/store
        // error is fatal, never permission to overwrite an existing alias.
        alias.verify_reference(source)?;
        if !exists {
            cmd::run(Command::new("podman").args(["tag", source, image]))?;
            alias.verify()?;
            if !has_receipt {
                fs::create_dir_all(receipt_path.parent().unwrap())?;
                let mut receipt = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&receipt_path)?;
                receipt.write_all(&serde_json::to_vec_pretty(&expected)?)?;
                receipt.sync_all()?;
            }
        }
        Ok(alias)
    }

    pub fn reference(&self) -> &str {
        &self.image
    }

    pub fn verify(&self) -> Result<()> {
        self.verify_reference(&self.image)
    }

    fn verify_reference(&self, reference: &str) -> Result<()> {
        let actual = crate::bcvk::image_id(reference)?;
        ensure!(
            actual
                .strip_prefix("sha256:")
                .unwrap_or(&actual)
                .eq_ignore_ascii_case(&self.id),
            "image alias/source ID mismatch for {reference}"
        );
        Ok(())
    }
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
    legacy_id: Option<String>,
    _lock: File,
}
impl ImageTransaction {
    pub fn new(ws: &Workspace, final_ref: &str) -> Result<Self> {
        Self::begin(ws, final_ref, None)
    }
    pub fn rebuild_legacy(ws: &Workspace, final_ref: &str, expected_id: &str) -> Result<Self> {
        ensure!(
            final_ref == ws.image_ref(),
            "legacy rebuild must target this checkout's default image"
        );
        Self::begin(ws, final_ref, Some(expected_id))
    }
    fn begin(ws: &Workspace, final_ref: &str, expected_id: Option<&str>) -> Result<Self> {
        check_output_namespace(ws, final_ref)?;
        ws.ensure_state()?;
        let file = image_lock(final_ref)?;
        let identity = ws.identity()?;
        let primary_output = identity.is_main
            && (ws.is_implicit_image(final_ref)
                || [
                    "localhost/ilex-installer:main",
                    "localhost/ilex-image-builder:main",
                ]
                .contains(&final_ref));
        let legacy_id = if image_exists(final_ref)?
            && image_owner(final_ref)?.is_none()
            && (expected_id.is_some() || primary_output)
        {
            let id = crate::bcvk::image_id(final_ref)?;
            if let Some(expected) = expected_id {
                ensure!(id == expected, "legacy image changed; refusing replacement");
            }
            eprintln!(
                "unlabelled legacy {final_ref} ({id}): rebuilding from owned source; keeping the old tag until validation"
            );
            Some(id)
        } else {
            check_output(ws, final_ref)?;
            ensure!(
                expected_id.is_none(),
                "explicit legacy rebuild requires an existing unlabelled image"
            );
            None
        };
        let owner = identity.owner;
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
            legacy_id,
            _lock: file,
        })
    }
    pub fn temporary(&self) -> &str {
        &self.temporary
    }
    pub fn needs_legacy_rebuild(&self) -> bool {
        self.legacy_id.is_some()
    }
    pub fn promote(&self, ws: &Workspace) -> Result<String> {
        let actual = image_owner(&self.temporary)?.context("temporary image has no owner label")?;
        ensure!(
            actual == self.owner,
            "temporary image owner was overridden; refusing promotion"
        );
        if let Some(legacy) = &self.legacy_id {
            ensure!(
                image_owner(&self.final_ref)?.is_none()
                    && crate::bcvk::image_id(&self.final_ref)? == *legacy,
                "legacy final image changed; refusing promotion"
            );
            let backup = format!(
                "localhost/ilex-legacy:{}.{legacy}",
                self.owner.replace('-', "")
            );
            if image_exists(&backup)? {
                ensure!(
                    crate::bcvk::image_id(&backup)? == *legacy,
                    "legacy backup alias collision"
                );
            }
            cmd::run(Command::new("podman").args(["tag", legacy, &backup]))?;
            let dir = ws.ensure_state()?.join("legacy-images");
            fs::create_dir_all(&dir)?;
            fs::write(
                dir.join(format!("{legacy}.json")),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "schema": SCHEMA_VERSION, "owner": self.owner, "previous_id": legacy, "backup": backup, "replacement": self.final_ref,
                }))?,
            )?;
            eprintln!(
                "preserved legacy image as {backup}; only the validated rebuild will become {}",
                self.final_ref
            );
        } else {
            check_output(ws, &self.final_ref)?;
        }
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
    ws.ensure_runtime()?;
    // A different XDG runtime root cannot establish a second lease on a host
    // container/domain name. Persistent receipts still authorize its namespace.
    crate::require_linux()?;
    #[cfg(target_os = "linux")]
    let root = {
        use std::os::unix::fs::MetadataExt;
        std::path::PathBuf::from(format!(
            "/tmp/ilex-vm-leases-{}",
            fs::metadata("/proc/self")?.uid()
        ))
    };
    #[cfg(not(target_os = "linux"))]
    let root = ws.runtime_dir();
    let key = digest(name.as_bytes());
    let path = root.join(format!("{}.lock", &key[7..]));
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

/// An exact created ID is the cancellation authority, never a VM name prefix.
/// Local Linux launchers keep their existing lifetime guards unchanged.
pub fn record_remote_resource(ws: &Workspace, provider: &str, id: &str, name: &str) -> Result<()> {
    let Some(directory) = std::env::var_os("ILEX_JOB_RESOURCE_DIR") else {
        return Ok(());
    };
    let context = crate::source::remote_context(&ws.root)?
        .context("remote resource has no source context")?;
    let directory = std::path::PathBuf::from(directory);
    let key = digest(format!("{provider}:{id}").as_bytes());
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{}.json", &key[7..]));
    let temporary = path.with_extension("partial");
    fs::write(
        &temporary,
        serde_json::to_vec(&serde_json::json!({
            "schema": 1, "owner": context.identity.owner, "provider": provider,
            "id": id, "name": name, "source_digest": context.source.digest,
        }))?,
    )?;
    fs::rename(temporary, path)?;
    Ok(())
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
