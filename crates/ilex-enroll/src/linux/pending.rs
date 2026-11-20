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
    pub enrollment: Enrollment,
    salt: [u8; 16],
    nonce: [u8; 12],
    ciphertext: Vec<u8>,
}

impl PendingRecovery {
    pub fn new(uuid: &str, enrollment: Enrollment, passphrase: &Secret) -> Result<Self> {
        let mut random = File::open("/dev/urandom")?;
        let mut entropy = Zeroizing::new([0u8; 32]);
        let mut receipt = Self {
            enrollment,
            salt: [0; 16],
            nonce: [0; 12],
            ciphertext: Vec::new(),
        };
        random.read_exact(&mut *entropy)?;
        random.read_exact(&mut receipt.salt)?;
        random.read_exact(&mut receipt.nonce)?;
        receipt.ciphertext = receipt
            .cipher(passphrase)?
            .encrypt(
                (&receipt.nonce).into(),
                Payload {
                    msg: &*entropy,
                    aad: receipt.binding(uuid).as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot protect pending recovery key"))?;
        Ok(receipt)
    }

    pub fn key(&self, uuid: &str, passphrase: &Secret) -> Result<Secret> {
        let bytes = self.decrypt(uuid, passphrase)?;
        let alphabet = b"cbdefghijklnrtuv";
        let mut key = Secret::empty()?;
        for (index, byte) in bytes.iter().enumerate() {
            if index > 0 && index % 4 == 0 {
                key.push(b'-')?;
            }
            key.push(alphabet[usize::from(byte >> 4)])?;
            key.push(alphabet[usize::from(byte & 15)])?;
        }
        Ok(key)
    }

    pub fn retarget(&mut self, uuid: &str, passphrase: &Secret, token: u32) -> Result<()> {
        let entropy = self.decrypt(uuid, passphrase)?;
        self.enrollment.token = token;
        File::open("/dev/urandom")?.read_exact(&mut self.nonce)?;
        self.ciphertext = self
            .cipher(passphrase)?
            .encrypt(
                (&self.nonce).into(),
                Payload {
                    msg: &entropy,
                    aad: self.binding(uuid).as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot protect pending recovery key"))?;
        Ok(())
    }

    fn decrypt(&self, uuid: &str, passphrase: &Secret) -> Result<Zeroizing<Vec<u8>>> {
        ensure!(
            self.ciphertext.len() == 48,
            "invalid pending recovery receipt"
        );
        Ok(Zeroizing::new(
            self.cipher(passphrase)?
                .decrypt(
                    (&self.nonce).into(),
                    Payload {
                        msg: &self.ciphertext,
                        aad: self.binding(uuid).as_bytes(),
                    },
                )
                .map_err(|_| anyhow::anyhow!("cannot authenticate pending recovery receipt"))?,
        ))
    }

    fn binding(&self, uuid: &str) -> String {
        format!(
            "ilex-pending-recovery-v1:{uuid}:{}:{}",
            self.enrollment.slot, self.enrollment.token
        )
    }

    fn cipher(&self, passphrase: &Secret) -> Result<Aes256Gcm> {
        let mut key = Zeroizing::new([0u8; 32]);
        let params = Params::new(128 * 1024, 3, 1, Some(32))
            .map_err(|_| anyhow::anyhow!("invalid recovery receipt KDF parameters"))?;
        Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
            .hash_password_into(passphrase.bytes(), &self.salt, &mut *key)
            .map_err(|_| anyhow::anyhow!("cannot derive recovery receipt key"))?;
        Aes256Gcm::new_from_slice(&*key)
            .map_err(|_| anyhow::anyhow!("invalid recovery receipt cipher key"))
    }
}
