//! Pure identity derivation from canonical native paths supplied by the host.
//! The owner is derived, never written into shared source or copied snapshots.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const MAX_SLUG_LEN: usize = 48;

pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutIdentity {
    pub repository_id: String,
    pub checkout_id: String,
    pub owner: String,
    pub slug: String,
    pub is_main: bool,
    pub source_revision: String,
    pub snapshot_digest: Option<String>,
}

impl CheckoutIdentity {
    /// `is_main` must come from Git's common-dir/worktree metadata, never a
    /// directory-name heuristic. Canonical path identity also separates clones.
    pub fn new(repository_id: String, checkout_id: String, name: &str, is_main: bool) -> Self {
        let key = format!("ilex-owner-v1\0{repository_id}\0{checkout_id}");
        let mut bytes: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        // RFC 9562 UUIDv8: stable application-defined SHA256 identity.
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex = bytes[..16]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let owner = format!(
            "{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        Self {
            repository_id,
            checkout_id,
            slug: if is_main {
                "main".into()
            } else {
                resource_slug(name, &owner)
            },
            owner,
            is_main,
            source_revision: String::new(),
            snapshot_digest: None,
        }
    }

    pub fn authorize(&self, owner: &str) -> Result<()> {
        ensure!(
            owner == self.owner,
            "resource belongs to owner {owner}, expected {}; refusing mutation",
            self.owner
        );
        Ok(())
    }
}

/// Resource names include canonical ownership, not Git's repository-local
/// worktree name alone. Ownership receipts still reject a hash collision.
pub fn resource_slug(name: &str, owner: &str) -> String {
    let readable = slug(name);
    let readable = readable[..readable.len().min(MAX_SLUG_LEN - 34)].trim_end_matches('-');
    let hash = digest(owner.as_bytes());
    format!("{readable}--{}", &hash[7..39])
}

/// Keep Git's unique short lowercase names stable. Changed, reserved and long
/// names use a separate hashed namespace; ambiguous names never alias `main`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    let out = match out {
        "" => "wt",
        "main" => "wt-main",
        x => x,
    };
    if out == name && out.len() <= MAX_SLUG_LEN {
        return out.into();
    }
    let readable = out[..out.len().min(MAX_SLUG_LEN - 18)].trim_end_matches('-');
    let hash = format!("{:x}", Sha256::digest(name.as_bytes()));
    format!("{readable}--{}", &hash[..16])
}
