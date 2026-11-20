//! Persistent canonical-repository name allocation for the local host.
use anyhow::{Context, Result, ensure};
use ilex_tooling_core::{CheckoutIdentity, SCHEMA_VERSION, check_version, digest};
use serde::{Deserialize, Serialize};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

pub const CANONICAL_REPOSITORY: &str = "github.com/reidond/ilex";

pub fn canonical_origin(origin: &str) -> bool {
    let origin = origin
        .trim()
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .to_ascii_lowercase();
    [
        "https://github.com/reidond/ilex",
        "http://github.com/reidond/ilex",
        "git@github.com:reidond/ilex",
        "ssh://git@github.com/reidond/ilex",
        "ssh://github.com/reidond/ilex",
    ]
    .contains(&origin.as_str())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainClaim {
    pub schema: u32,
    pub repository: String,
    pub owner: String,
    pub repository_id: String,
    pub checkout_id: String,
    pub checkout: String,
}

pub struct MainClaims {
    root: PathBuf,
}
impl MainClaims {
    /// Explicit roots isolate observable self-checks from the machine registry.
    pub fn at(root: PathBuf) -> Self {
        Self { root }
    }
    pub fn system() -> Result<Self> {
        static ROOT: OnceLock<PathBuf> = OnceLock::new();
        if let Some(root) = ROOT.get() {
            return Ok(Self::at(root.clone()));
        }
        let root = if cfg!(windows) {
            PathBuf::from(env::var_os("ProgramData").context("ProgramData is missing")?)
                .join("ilex/tooling/main-claims")
        } else {
            PathBuf::from("/var/lib/ilex-tooling/main-claims")
        };
        let _ = ROOT.set(root.clone());
        Ok(Self::at(root))
    }
    pub fn path(&self) -> &Path {
        &self.root
    }
    fn records(&self) -> PathBuf {
        self.root
            .join(&digest(CANONICAL_REPOSITORY.as_bytes())[7..])
    }
    fn read_claim(&self, path: &Path) -> Result<Option<MainClaim>> {
        match fs::read(path) {
            Ok(bytes) => {
                let claim: MainClaim = serde_json::from_slice(&bytes)
                    .context("invalid main claim; refusing reassignment")?;
                check_version(claim.schema)?;
                ensure!(
                    claim.repository == CANONICAL_REPOSITORY,
                    "main claim repository mismatch"
                );
                let derived = CheckoutIdentity::new(
                    claim.repository_id.clone(),
                    claim.checkout_id.clone(),
                    "primary",
                    false,
                );
                ensure!(
                    derived.owner == claim.owner
                        && crate::workspace::native_path(Path::new(&claim.checkout))
                            == claim.checkout_id,
                    "main claim identity mismatch; refusing reassignment"
                );
                Ok(Some(claim))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
    fn current_slot(&self) -> Result<(u64, Option<MainClaim>)> {
        for generation in 0..u64::MAX {
            let record = self.records().join(format!("{generation:020}.json"));
            let Some(claim) = self.read_claim(&record)? else {
                return Ok((generation, None));
            };
            let released = self
                .records()
                .join(format!("{generation:020}.released.json"));
            match fs::read(released) {
                Ok(bytes) => {
                    let receipt: serde_json::Value = serde_json::from_slice(&bytes)?;
                    ensure!(
                        receipt["schema"] == SCHEMA_VERSION
                            && receipt["owner"].as_str() == Some(claim.owner.as_str())
                            && receipt["record_sha256"].as_str()
                                == Some(digest(&fs::read(&record)?).as_str()),
                        "invalid main release receipt; refusing reassignment"
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok((generation, Some(claim)));
                }
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("main claim generation exhausted")
    }
    pub fn current(&self) -> Result<Option<MainClaim>> {
        Ok(self.current_slot()?.1)
    }

    // An immutable generation is published fully written with an atomic hard
    // link. Releases append a receipt; they never unlink a newer
    // claimant's file, even if two operators release concurrently.
    fn publish(&self, destination: &Path, bytes: &[u8]) -> Result<bool> {
        fs::create_dir_all(self.records())?;
        let proposal = self.records().join(format!(
            "proposal-{}-{}.json",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&proposal)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        let linked = fs::hard_link(&proposal, destination);
        drop(file);
        fs::remove_file(proposal)?;
        match linked {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
    pub fn allocate(
        &self,
        mut candidate: CheckoutIdentity,
        checkout: &Path,
    ) -> Result<CheckoutIdentity> {
        let checkout = fs::canonicalize(checkout)?;
        ensure!(
            crate::workspace::native_path(&checkout) == candidate.checkout_id,
            "main candidate path mismatch"
        );
        let derived = CheckoutIdentity::new(
            candidate.repository_id.clone(),
            candidate.checkout_id.clone(),
            "primary",
            false,
        );
        ensure!(
            derived.owner == candidate.owner,
            "main candidate owner mismatch"
        );
        let claim = loop {
            let (generation, current) = self.current_slot()?;
            if let Some(claim) = current {
                break claim;
            }
            let native = native_spelling(&checkout);
            let claim = MainClaim {
                schema: SCHEMA_VERSION,
                repository: CANONICAL_REPOSITORY.into(),
                owner: candidate.owner.clone(),
                repository_id: candidate.repository_id.clone(),
                checkout_id: candidate.checkout_id.clone(),
                checkout: native,
            };
            if self.publish(
                &self.records().join(format!("{generation:020}.json")),
                &serde_json::to_vec_pretty(&claim)?,
            )? {
                break claim;
            }
        };
        candidate.is_main = claim.owner == candidate.owner
            && claim.repository_id == candidate.repository_id
            && claim.checkout_id == candidate.checkout_id;
        if candidate.is_main {
            candidate.slug = "main".into();
        } else {
            candidate.slug = ilex_tooling_core::resource_slug(
                checkout
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("primary"),
                &candidate.owner,
            );
        }
        Ok(candidate)
    }
    pub fn release_stale(&self, expected_owner: &str) -> Result<MainClaim> {
        let (generation, claim) = self.current_slot()?;
        let claim = claim.context("no registered main claim")?;
        ensure!(
            claim.owner == expected_owner,
            "main owner changed; refusing release"
        );
        ensure!(
            !claim.checkout_exists()?,
            "registered primary checkout still exists at {}; refusing release",
            claim.checkout
        );
        let record = self.records().join(format!("{generation:020}.json"));
        self.publish(&self.records().join(format!("{generation:020}.released.json")), &serde_json::to_vec_pretty(&serde_json::json!({
            "schema": SCHEMA_VERSION, "owner": claim.owner, "record_sha256": digest(&fs::read(record)?),
        }))?)?;
        Ok(claim)
    }
}

impl MainClaim {
    pub fn checkout_exists(&self) -> Result<bool> {
        let checkout = Path::new(&self.checkout);
        // Legacy registries may contain a checkout from a different host.
        ensure!(
            checkout.is_absolute() && (cfg!(windows) || !self.checkout.starts_with("//")),
            "cannot inspect this checkout from this host; refusing release"
        );
        checkout.try_exists().map_err(Into::into)
    }
}

fn native_spelling(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    text.strip_prefix("//?/").unwrap_or(&text).into()
}
