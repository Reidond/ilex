//! A temporary, authenticated recovery receipt. Persist it before allocating
//! anything so retry can redisplay the same key even after an abrupt stop.
use super::{Enrollment, Secret};
use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use anyhow::{Result, ensure};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};
use std::{fs::File, io::Read};
use zeroize::Zeroizing;

#[derive(Serialize, Deserialize)]
pub struct PendingRecovery {
    #[serde(default)]
    schema: Option<u32>,
    pub enrollment: Enrollment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    salt: Option<[u8; 16]>,
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

impl PendingRecovery {
    pub fn generate() -> Result<Secret> {
        let mut entropy = Zeroizing::new([0u8; 32]);
        File::open("/dev/urandom")?.read_exact(&mut *entropy)?;
        format_key(&entropy[..])
    }

    pub fn seal(
        uuid: &str,
        enrollment: Enrollment,
        protector: &Secret,
        key: &Secret,
    ) -> Result<Self> {
        let mut nonce = [0; 12];
        File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let mut receipt = Self {
            schema: Some(2),
            enrollment,
            salt: None,
            nonce,
            ciphertext: Vec::new(),
        };
        receipt.ciphertext = cipher(protector.bytes())?
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: key.bytes(),
                    aad: receipt.binding(uuid, 2).as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot protect pending recovery key"))?;
        Ok(receipt)
    }

    pub fn key(&self, uuid: &str, unlock: &Secret, protector: &Secret) -> Result<Secret> {
        let version = self.schema.unwrap_or(1);
        let cipher = match version {
            1 => self.legacy_cipher(unlock)?,
            2 => cipher(protector.bytes())?,
            _ => anyhow::bail!("unknown recovery receipt schema"),
        };
        let size = if version == 1 { 48 } else { 87 };
        ensure!(
            self.ciphertext.len() == size,
            "invalid pending recovery receipt"
        );
        let key = Zeroizing::new(
            cipher
                .decrypt(
                    (&self.nonce).into(),
                    Payload {
                        msg: &self.ciphertext,
                        aad: self.binding(uuid, version).as_bytes(),
                    },
                )
                .map_err(|_| anyhow::anyhow!("cannot authenticate pending recovery receipt"))?,
        );
        if version == 1 {
            format_key(&key)
        } else {
            Secret::copy(&key)
        }
    }

    fn binding(&self, uuid: &str, version: u32) -> String {
        format!(
            "ilex-pending-recovery-v{version}:{uuid}:{}:{}",
            self.enrollment.slot, self.enrollment.token
        )
    }

    fn legacy_cipher(&self, unlock: &Secret) -> Result<Aes256Gcm> {
        let mut key = Zeroizing::new([0u8; 32]);
        let salt = self
            .salt
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("legacy recovery receipt lacks salt"))?;
        let params = Params::new(128 * 1024, 3, 1, Some(32))
            .map_err(|_| anyhow::anyhow!("invalid recovery receipt KDF parameters"))?;
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(unlock.bytes(), salt, &mut *key)
            .map_err(|_| anyhow::anyhow!("cannot derive recovery receipt key"))?;
        cipher(&*key)
    }
}

fn cipher(key: &[u8]) -> Result<Aes256Gcm> {
    Aes256Gcm::new_from_slice(key).map_err(|_| anyhow::anyhow!("invalid recovery receipt key"))
}

fn format_key(entropy: &[u8]) -> Result<Secret> {
    let alphabet = b"cbdefghijklnrtuv";
    let mut key = Secret::empty()?;
    for (index, byte) in entropy.iter().enumerate() {
        if index > 0 && index % 4 == 0 {
            key.push(b'-')?;
        }
        key.push(alphabet[usize::from(byte >> 4)])?;
        key.push(alphabet[usize::from(byte & 15)])?;
    }
    Ok(key)
}
