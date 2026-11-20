//! The single authenticated reconciliation point for installed-header writes.
//! A receipt is durable before any write that cannot be recognized on retry.
use super::{
    enroll::{ensure_capacity, free_id, slot, stage_tpm, trial_token},
    pending::PendingRecovery,
    *,
};
use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, Payload},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, path::PathBuf};
use zeroize::Zeroizing;

pub struct SecurityState {
    path: PathBuf,
    pub value: Value,
}

impl SecurityState {
    pub fn open(root: &Path, device: &Device) -> Result<Self> {
        let path = root.join("var/lib/ilex/security/enrollment.json");
        let value = read(&path)?.unwrap_or_else(|| {
            json!({"schema":1, "uuid":device.uuid, "state":"pending", "recovery":null, "tpm":null})
        });
        ensure!(
            value["schema"] == 1 && (value["uuid"] == device.uuid || legacy_unbound(&value)),
            "enrollment state UUID/schema mismatch"
        );
        Ok(Self { path, value })
    }

    pub fn save(&self) -> Result<()> {
        write_state(&self.path, &self.value)
    }

    pub fn acknowledged(&self) -> bool {
        acknowledged_recovery(&self.value)
    }

    pub fn recovery_pending(&self) -> Result<bool> {
        Ok(self
            .path
            .with_file_name(".pending-recovery.json")
            .try_exists()?)
    }

    pub fn tpm_pending(&self) -> Result<bool> {
        Ok(self.path.with_file_name(".pending-tpm.json").try_exists()?)
    }
}

pub fn eligibility(root: &Path) -> (Probe, BTreeMap<String, String>, &'static str) {
    let capability = probe();
    let tools = tool_versions(root).unwrap_or_default();
    let stack: Value = read(&root.join("usr/lib/ilex/security/unlock.json"))
        .ok()
        .flatten()
        .unwrap_or(Value::Null);
    let reason = if capability.verdict != "eligible" {
        capability.reason
    } else if stack["schema"] != 1 || stack["method"] != "cryptsetup-token-only-v1" {
        "unlock-stack-missing"
    } else if tools != capability.tools {
        "tool-mismatch"
    } else {
        capability.reason
    };
    (capability, tools, reason)
}

fn legacy_unbound(state: &Value) -> bool {
    state.get("uuid") == Some(&Value::Null)
        && state["state"] == "unavailable"
        && state["reason"] == "unsupported-storage-layout"
        && state.get("recovery") == Some(&Value::Null)
        && state.get("tpm") == Some(&Value::Null)
}

pub fn acknowledged_recovery(state: &Value) -> bool {
    let recovery = &state["recovery"];
    recovery["acknowledged"] == true
        && recovery
            .get("displayed_and_acknowledged")
            .is_none_or(|v| v == true)
}

pub fn recorded_recovery(state: &Value, header: &Value) -> Result<Enrollment> {
    let enrollment: Enrollment = serde_json::from_value(state["recovery"].clone())?;
    ensure!(
        header["tokens"].get(enrollment.token.to_string()) == Some(&recovery_token(enrollment))
            && header["keyslots"]
                .get(enrollment.slot.to_string())
                .is_some(),
        "recorded recovery slot or token is missing"
    );
    Ok(enrollment)
}

fn recovery_token(enrollment: Enrollment) -> Value {
    json!({"type":"systemd-recovery", "keyslots":[enrollment.slot.to_string()]})
}

fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("invalid security state or receipt")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn unlink(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    fs::File::open(path.parent().context("receipt has no parent")?)?.sync_all()?;
    Ok(())
}

fn tpm_entries(header: &Value) -> impl Iterator<Item = (&String, &Value)> {
    header["tokens"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, t)| t["type"] == "systemd-tpm2")
}

impl Device {
    /// Read-only preflight for a fresh recovery issue. Resume an existing
    /// receipt before calculating availability, since it may own the last slot.
    pub fn preflight(&self, state: &SecurityState, recovery: bool, tpm: bool) -> Result<()> {
        if !state.recovery_pending()? && !state.tpm_pending()? {
            let header = self.header()?;
            let count =
                usize::from(recovery) + usize::from(tpm && tpm_entries(&header).next().is_none());
            ensure_capacity(&header, count, count)?;
        }
        Ok(())
    }
}

pub struct Reconciled<'a> {
    device: &'a Device,
    unlock: &'a Secret,
    pub state: SecurityState,
    recovery: Option<(Enrollment, Secret)>,
    recovery_path: PathBuf,
    tpm_path: PathBuf,
    tpm_eligible: bool,
}

impl<'a> Reconciled<'a> {
    pub fn open(
        device: &'a Device,
        unlock: &'a Secret,
        state: SecurityState,
        tpm_eligible: bool,
    ) -> Result<Self> {
        device.authenticate(unlock.fd()?)?;
        let recovery_path = state.path.with_file_name(".pending-recovery.json");
        let tpm_path = state.path.with_file_name(".pending-tpm.json");
        let mut session = Self {
            device,
            unlock,
            state,
            recovery: None,
            recovery_path,
            tpm_path,
            tpm_eligible,
        };
        let header = device.header()?;
        if legacy_unbound(&session.state.value) {
            session.state.value["uuid"] = json!(device.uuid);
            session.state.value["state"] = json!("pending");
            session.state.save()?;
        }
        if session.state.acknowledged() {
            recorded_recovery(&session.state.value, &header)?;
        }
        if let Some(mut receipt) = read::<PendingRecovery>(&session.recovery_path)? {
            if session.state.acknowledged()
                && recorded_recovery(&session.state.value, &header)? == receipt.enrollment
            {
                unlink(&session.recovery_path)?;
            } else {
                let key = receipt.key(&device.uuid, unlock).context(
                    "resume the pending recovery issue with the credential used to start it",
                )?;
                let entry = receipt.enrollment;
                if header["tokens"]
                    .get(entry.token.to_string())
                    .is_some_and(|t| t != &recovery_token(entry))
                {
                    // PR18 could allocate this receipt's token ID to TPM. The
                    // authenticated key still identifies the planned slot.
                    if header["keyslots"].get(entry.slot.to_string()).is_some() {
                        device.authenticate_slot(key.fd()?, entry.slot)?;
                    }
                    let new_token = free_id(&header, "tokens")?;
                    receipt.retarget(&device.uuid, unlock, new_token)?;
                    write_state(&session.recovery_path, &serde_json::to_value(&receipt)?)?;
                }
                let entry = receipt.enrollment;
                device.complete_slot(&entry, unlock, &key, &recovery_token(entry))?;
                session.recovery = Some((entry, key));
            }
        }
        if tpm_eligible && let Some(receipt) = read::<TpmReceipt>(&session.tpm_path)? {
            session.resume_tpm(&receipt)?;
        }
        Ok(session)
    }

    pub fn recovery(&mut self, allow_existing_unrecorded: bool) -> Result<(Enrollment, &Secret)> {
        if self.recovery.is_none() {
            let header = self.device.header()?;
            ensure!(
                allow_existing_unrecorded
                    || !header["tokens"]
                        .as_object()
                        .unwrap()
                        .values()
                        .any(|t| t["type"] == "systemd-recovery"),
                "recovery slot has no completion record; use explicit reissue"
            );
            let count = 1 + usize::from(self.tpm_eligible && tpm_entries(&header).next().is_none());
            ensure_capacity(&header, count, count)?;
            let entry = self.device.plan_slot()?;
            let receipt = PendingRecovery::new(&self.device.uuid, entry, self.unlock)?;
            write_state(&self.recovery_path, &serde_json::to_value(&receipt)?)?;
            if !self.state.acknowledged() {
                self.state.value["state"] = json!("recovery-incomplete");
                self.state.save()?;
            }
            let key = receipt.key(&self.device.uuid, self.unlock)?;
            self.device
                .complete_slot(&entry, self.unlock, &key, &recovery_token(entry))?;
            self.recovery = Some((entry, key));
        }
        let (entry, key) = self
            .recovery
            .as_ref()
            .context("missing reconciled recovery")?;
        if !self.state.acknowledged() {
            self.state.value["recovery"] = recovery_record(*entry, false);
            self.state.save()?;
        }
        Ok((*entry, key))
    }

    pub fn acknowledge(&mut self, entry: Enrollment) -> Result<()> {
        recorded_recovery(&json!({"recovery":entry}), &self.device.header()?)?;
        self.state.value["recovery"] = recovery_record(entry, true);
        self.state.save()?;
        if self.recovery.take().is_some() {
            unlink(&self.recovery_path)?;
        }
        Ok(())
    }

    pub fn enroll_tpm(&mut self, repair: bool) -> Result<Enrollment> {
        ensure!(self.tpm_eligible, "TPM enrollment unavailable");
        ensure!(
            self.state.acknowledged(),
            "confirm the recovery key with local completion first"
        );
        ensure!(!self.state.tpm_pending()?, "TPM transaction still pending");
        let header = self.device.header()?;
        super::layout::verify_header_cipher(&header)?;
        let mut entries = tpm_entries(&header);
        let old = if let Some((id, token)) = entries.next() {
            ensure!(
                entries.next().is_none(),
                "ambiguous preexisting TPM enrollment"
            );
            validate_token(token)?;
            let entry = Enrollment {
                token: id.parse()?,
                slot: slot(token)?,
            };
            let owner = self.state.value["tpm"].clone();
            let fingerprint = OwnedTpm::new(entry, &header)?;
            ensure!(
                self.state.value["tpm_header"].is_null()
                    || self.state.value["tpm_header"] == serde_json::to_value(&fingerprint)?,
                "recorded TPM token or keyslot was changed"
            );
            if trial_token(self.device, entry.token).is_ok() {
                ensure!(
                    owner.is_null() || serde_json::from_value::<Enrollment>(owner)? == entry,
                    "TPM token differs from the recorded enrollment"
                );
                self.record_tpm(entry, repair)?;
                return Ok(entry);
            }
            ensure!(
                repair,
                "TPM token failed to unseal; use local authenticated repair"
            );
            ensure!(
                serde_json::from_value::<Enrollment>(owner).ok() == Some(entry),
                "TPM token does not belong to ilex's recorded enrollment"
            );
            let mut available = header.clone();
            available["tokens"]
                .as_object_mut()
                .unwrap()
                .remove(&entry.token.to_string());
            available["keyslots"]
                .as_object_mut()
                .unwrap()
                .remove(&entry.slot.to_string());
            ensure_capacity(&available, 1, 1)?;
            Some(fingerprint)
        } else {
            ensure_capacity(&header, 1, 1)?;
            None
        };
        let (mut token, key) = stage_tpm(self.device)?;
        if let Some(old) = old {
            self.commit_tpm(TpmChange::Remove(old))?;
        }
        let entry = self.device.plan_slot()?;
        token["keyslots"] = json!([entry.slot.to_string()]);
        let (_, protector) = super::crypt::receipt_key(self.device, self.unlock)?;
        let mut nonce = [0; 12];
        fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
        let binding = serde_json::to_vec(&(&self.device.uuid, entry, &token))?;
        let ciphertext = cipher(&protector)?
            .encrypt(
                (&nonce).into(),
                Payload {
                    msg: key.bytes(),
                    aad: &binding,
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot protect TPM transaction credential"))?;
        self.commit_tpm(TpmChange::Install {
            entry,
            token,
            nonce,
            ciphertext,
        })?;
        trial_token(self.device, entry.token)?;
        Ok(entry)
    }

    fn record_tpm(&mut self, entry: Enrollment, recheck: bool) -> Result<()> {
        let fingerprint = serde_json::to_value(OwnedTpm::new(entry, &self.device.header()?)?)?;
        if !recheck && self.state.value["tpm_header"] == fingerprint {
            return Ok(());
        }
        self.state.value["tpm"] = serde_json::to_value(entry)?;
        self.state.value["tpm_header"] = fingerprint;
        self.state.value["tpm_enrolled_boot_id"] = json!(current_boot()?);
        self.state.value["verification"] =
            json!({"state":"pending", "reason":"awaiting-cold-boot"});
        self.state.value["state"] = json!("pending");
        self.state.value["reason"] = json!("pcr7-sha256");
        self.state.save()
    }

    fn commit_tpm(&mut self, change: TpmChange) -> Result<()> {
        let receipt = TpmReceipt {
            schema: 1,
            uuid: self.device.uuid.clone(),
            change,
        };
        write_state(&self.tpm_path, &serde_json::to_value(&receipt)?)?;
        self.resume_tpm(&receipt)
    }

    fn resume_tpm(&mut self, receipt: &TpmReceipt) -> Result<()> {
        ensure!(
            receipt.schema == 1 && receipt.uuid == self.device.uuid,
            "TPM transaction UUID/schema mismatch"
        );
        let (fallback_slot, protector) = super::crypt::receipt_key(self.device, self.unlock)?;
        let header = self.device.header()?;
        super::layout::verify_header_cipher(&header)?;
        match &receipt.change {
            TpmChange::Remove(old) => {
                let owner = self.state.value["tpm"].clone();
                ensure!(
                    owner.is_null() || serde_json::from_value::<Enrollment>(owner)? == old.entry,
                    "TPM retirement differs from recorded enrollment"
                );
                let slot = old.entry.slot.to_string();
                let token = old.entry.token.to_string();
                ensure!(
                    old.entry.slot != fallback_slot,
                    "repair requires a separate fallback credential"
                );
                old.check(&header)?;
                if header["tokens"].get(&token).is_some() {
                    self.device.remove_token(&token)?;
                }
                let header = self.device.header()?;
                old.check(&header)?;
                if header["keyslots"].get(&slot).is_some() {
                    self.device.remove_slot(&slot)?;
                }
                self.state.value["tpm"] = Value::Null;
                self.state.value["tpm_header"] = Value::Null;
                self.state.value["tpm_enrolled_boot_id"] = Value::Null;
                self.state.value["verification"] =
                    json!({"state":"error", "reason":"repair-incomplete"});
                self.state.value["state"] = json!("error");
                self.state.value["reason"] = json!("repair-incomplete");
                self.state.save()?;
            }
            TpmChange::Install {
                entry,
                token,
                nonce,
                ciphertext,
            } => {
                validate_token(token)?;
                ensure!(
                    entry.slot != fallback_slot && (17..=1040).contains(&ciphertext.len()),
                    "invalid TPM slot transaction"
                );
                let binding = serde_json::to_vec(&(&receipt.uuid, entry, token))?;
                let key = Zeroizing::new(
                    cipher(&protector)?
                        .decrypt(
                            nonce.into(),
                            Payload {
                                msg: ciphertext,
                                aad: &binding,
                            },
                        )
                        .map_err(|_| {
                            anyhow::anyhow!("cannot authenticate TPM transaction receipt")
                        })?,
                );
                self.device
                    .complete_slot(entry, self.unlock, &Secret::copy(&key)?, token)?;
                self.record_tpm(*entry, false)?;
            }
        }
        unlink(&self.tpm_path)
    }
}

pub fn recovery_record(entry: Enrollment, acknowledged: bool) -> Value {
    json!({"token":entry.token, "slot":entry.slot, "acknowledged":acknowledged, "displayed_and_acknowledged":acknowledged})
}

fn cipher(key: &Secret) -> Result<Aes256Gcm> {
    Aes256Gcm::new_from_slice(key.bytes()).map_err(|_| anyhow::anyhow!("invalid TPM receipt key"))
}

pub(super) fn current_boot() -> Result<String> {
    Ok(fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

#[derive(Serialize, Deserialize)]
struct TpmReceipt {
    schema: u32,
    uuid: String,
    change: TpmChange,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
enum TpmChange {
    Remove(OwnedTpm),
    Install {
        entry: Enrollment,
        token: Value,
        nonce: [u8; 12],
        ciphertext: Vec<u8>,
    },
}

#[derive(Serialize, Deserialize)]
pub(super) struct OwnedTpm {
    entry: Enrollment,
    keyslot_sha256: String,
    token_sha256: String,
}

impl OwnedTpm {
    pub(super) fn new(entry: Enrollment, header: &Value) -> Result<Self> {
        let token = header["tokens"]
            .get(entry.token.to_string())
            .context("recorded TPM token is missing")?;
        validate_token(token)?;
        ensure!(
            slot(token)? == entry.slot,
            "recorded TPM slot differs from token"
        );
        let keyslot = header["keyslots"]
            .get(entry.slot.to_string())
            .context("recorded TPM slot is missing")?;
        let result = Self {
            entry,
            keyslot_sha256: fingerprint(keyslot)?,
            token_sha256: fingerprint(token)?,
        };
        result.check(header)?;
        Ok(result)
    }

    fn check(&self, header: &Value) -> Result<()> {
        let entry = self.entry;
        let slot = entry.slot.to_string();
        let token = entry.token.to_string();
        if let Some(found) = header["tokens"].get(&token) {
            ensure!(
                fingerprint(found)? == self.token_sha256,
                "TPM token was changed"
            );
        }
        if let Some(found) = header["keyslots"].get(&slot) {
            ensure!(
                fingerprint(found)? == self.keyslot_sha256,
                "TPM keyslot was changed"
            );
        }
        ensure!(
            header["tokens"]
                .as_object()
                .unwrap()
                .iter()
                .all(|(id, t)| id == &token
                    || !t["keyslots"]
                        .as_array()
                        .is_some_and(|ids| ids.iter().any(|id| id == &slot))),
            "another token references the TPM keyslot"
        );
        Ok(())
    }
}

fn fingerprint(value: &Value) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}
