//! The single authenticated reconciliation point for installed-header writes.
//! A receipt is durable before any write that cannot be recognized on retry.
use super::{
    enroll::{PlannedWrite, ensure_capacity, free_id, slot, stage_tpm, trial_token},
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

    pub(super) fn read_bound(root: &Path) -> Result<Option<Self>> {
        use std::os::unix::fs::MetadataExt;
        let path = root.join("var/lib/ilex/security/enrollment.json");
        match fs::symlink_metadata(&path) {
            Ok(meta) => ensure!(
                meta.is_file() && meta.uid() == 0 && meta.mode() & 0o022 == 0,
                "untrusted enrollment state file"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let value: Value = read(&path)?.context("enrollment state disappeared during lookup")?;
        ensure!(value["schema"] == 1, "unsupported enrollment state schema");
        if legacy_unbound(&value) {
            return Ok(None);
        }
        ensure!(
            value["uuid"].as_str().is_some_and(|uuid| uuid.len() == 36
                && uuid.bytes().enumerate().all(|(index, byte)| {
                    if [8, 13, 18, 23].contains(&index) {
                        byte == b'-'
                    } else {
                        byte.is_ascii_hexdigit()
                    }
                })),
            "invalid enrollment state UUID"
        );
        Ok(Some(Self { path, value }))
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

    pub(super) fn guard_recovery_allocation(&self) -> Result<()> {
        for root in [Path::new("/"), installer_root()?] {
            let state = SecurityState::read_bound(root).with_context(|| {
                format!("cannot read root enrollment state under {}", root.display())
            })?;
            let has_receipt = || -> Result<bool> {
                let dir = root.join("var/lib/ilex/security");
                for name in [".pending-recovery.json", ".pending-tpm.json"] {
                    let path = dir.join(name);
                    if path.try_exists().with_context(|| {
                        format!("cannot check enrollment receipt {}", path.display())
                    })? {
                        return Ok(true);
                    }
                }
                Ok(false)
            };
            let blocked = match state {
                Some(state)
                    if state.value["uuid"]
                        .as_str()
                        .is_some_and(|uuid| uuid.eq_ignore_ascii_case(&self.uuid)) =>
                {
                    let needs_tpm = state.value["tpm"].is_null()
                        && matches!(
                            state.value["state"].as_str(),
                            Some("pending" | "error" | "recovery-incomplete")
                        );
                    needs_tpm || has_receipt()?
                }
                None if has_receipt()? => {
                    // The initial recovery receipt can be durable before the
                    // first state write. In that window resolve the root UUID.
                    let context = || {
                        format!(
                            "cannot identify LUKS2 root for enrollment receipt under {}",
                            root.display()
                        )
                    };
                    let layout = recovery_layout(root)
                        .with_context(context)?
                        .with_context(context)?;
                    layout.uuid.eq_ignore_ascii_case(&self.uuid)
                }
                _ => false,
            };
            ensure!(
                !blocked,
                "use local completion to reconcile pending root enrollment before allocation"
            );
        }
        Ok(())
    }
}

pub struct Reconciled<'a> {
    device: &'a Device,
    unlock: &'a Secret,
    pub state: SecurityState,
    recovery: Option<(Enrollment, Secret)>,
    pending: Option<(Enrollment, Secret)>,
    staged_tpm: Option<(Enrollment, Value, Secret)>,
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
            pending: None,
            staged_tpm: None,
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
        if tpm_eligible && let Some(receipt) = read::<TpmReceipt>(&session.tpm_path)? {
            session.resume_tpm(&receipt)?;
        }
        session.reconcile_recovery()?;
        Ok(session)
    }

    fn reconcile_recovery(&mut self) -> Result<()> {
        let header = self.device.header()?;
        let acknowledged = if self.state.acknowledged() {
            Some(recorded_recovery(&self.state.value, &header)?)
        } else {
            None
        };
        let planned = serde_json::from_value::<Option<Enrollment>>(
            self.state.value["recovery_attempt"].clone(),
        )?;
        let bytes = match fs::read(&self.recovery_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(entry) = planned {
                    self.manual_recovery(&header, Some(entry), None)?;
                }
                return Ok(());
            }
            Err(_) => {
                self.manual_recovery(&header, planned, None)?;
                return Ok(());
            }
        };
        let receipt = serde_json::from_slice::<PendingRecovery>(&bytes).ok();
        if let Some(receipt) = receipt.as_ref() {
            let (_, protector) = super::crypt::receipt_key(self.device, self.unlock)?;
            if let Ok(key) = receipt.key(&self.device.uuid, self.unlock, &protector) {
                if acknowledged == Some(receipt.enrollment)
                    && self
                        .device
                        .authenticate_slot(key.fd()?, receipt.enrollment.slot)
                        .is_ok()
                {
                    self.state.value["recovery_attempt"] = Value::Null;
                    self.state.save()?;
                    unlink(&self.recovery_path)?;
                } else {
                    self.pending = Some((receipt.enrollment, key));
                }
                return Ok(());
            }
        }
        // The presented credential already authenticated the volume. Neither
        // a legacy receipt nor an unreadable receipt grants deletion rights
        // over a slot. Retain its ciphertext and leave every candidate intact.
        self.manual_recovery(
            &header,
            receipt.map(|r| r.enrollment).or(planned),
            Some(&bytes),
        )
    }

    fn manual_recovery(
        &mut self,
        header: &Value,
        hint: Option<Enrollment>,
        bytes: Option<&[u8]>,
    ) -> Result<()> {
        let archive =
            bytes.map(|bytes| format!(".recovery-manual-{:x}.json", Sha256::digest(bytes)));
        let slot = hint
            .and_then(|entry| header["keyslots"].get(entry.slot.to_string()))
            .map(fingerprint)
            .transpose()?;
        let token = hint
            .and_then(|entry| header["tokens"].get(entry.token.to_string()))
            .map(fingerprint)
            .transpose()?;
        self.state.value["recovery_cleanup"] = json!({"state":"manual", "hint":hint,
            "slot_sha256":slot, "token_sha256":token, "receipt":archive});
        self.state.value["recovery_attempt"] = Value::Null;
        self.state.save()?;
        if let Some(archive) = archive {
            fs::rename(
                &self.recovery_path,
                self.recovery_path.with_file_name(archive),
            )?;
            fs::File::open(
                self.recovery_path
                    .parent()
                    .context("receipt has no parent")?,
            )?
            .sync_all()?;
        }
        Ok(())
    }

    pub fn recovery(&mut self, explicit: bool) -> Result<(Enrollment, &Secret)> {
        if self.recovery.is_none() {
            let header = self.device.header()?;
            let (entry, key) = if let Some((hint, key)) = self.pending.take() {
                (self.recovery_plan(&header, hint, &key)?, key)
            } else {
                ensure!(
                    !self.recovery_path.try_exists()?,
                    "unreadable recovery receipt requires manual cleanup"
                );
                ensure!(
                    explicit
                        || (self.state.value["recovery_cleanup"].is_null()
                            && !header["tokens"]
                                .as_object()
                                .unwrap()
                                .values()
                                .any(|t| t["type"] == "systemd-recovery")),
                    "recovery enrollment needs explicit reissue or confirmation of a saved key"
                );
                (self.device.plan_slot()?, PendingRecovery::generate()?)
            };
            let token = recovery_token(entry);
            self.reserve_recovery(&header, entry, &key, &token)?;
            let (_, protector) = super::crypt::receipt_key(self.device, self.unlock)?;
            let receipt = PendingRecovery::seal(&self.device.uuid, entry, &protector, &key)?;
            write_state(&self.recovery_path, &serde_json::to_value(&receipt)?)?;
            self.state.value["recovery_attempt"] = serde_json::to_value(entry)?;
            if !self.state.acknowledged() {
                self.state.value["state"] = json!("recovery-incomplete");
            }
            self.state.save()?;
            self.device
                .complete_slot(&entry, self.unlock, &key, &token)?;
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

    /// Reservations are hints. A present slot belongs to the receipt only if
    /// the authenticated recovery key actually unlocks that slot.
    fn recovery_plan(&self, header: &Value, hint: Enrollment, key: &Secret) -> Result<Enrollment> {
        let slot_id = hint.slot.to_string();
        let tokens = header["tokens"].as_object().unwrap();
        let mut references = tokens.iter().filter(|(_, token)| {
            token["keyslots"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == &slot_id))
        });
        let first = references.next();
        let ours = header["keyslots"].get(&slot_id).is_some()
            && self.device.authenticate_slot(key.fd()?, hint.slot).is_ok();
        if ours
            && let Some((id, token)) = first
            && references.next().is_none()
            && token == &recovery_token(hint)
        {
            return Ok(Enrollment {
                slot: hint.slot,
                token: id.parse()?,
            });
        }
        let slot = if first.is_none() && (ours || header["keyslots"].get(&slot_id).is_none()) {
            hint.slot
        } else {
            free_id(header, "keyslots")?
        };
        let token = if header["tokens"].get(hint.token.to_string()).is_none() {
            hint.token
        } else {
            free_id(header, "tokens")?
        };
        Ok(Enrollment { slot, token })
    }

    fn reserve_recovery(
        &mut self,
        header: &Value,
        entry: Enrollment,
        key: &Secret,
        token: &Value,
    ) -> Result<()> {
        let needs_slot = header["keyslots"].get(entry.slot.to_string()).is_none();
        let needs_token = header["tokens"].get(entry.token.to_string()).is_none();
        if !needs_slot && !needs_token {
            return Ok(());
        }
        let needs_tpm = self.tpm_eligible && tpm_entries(header).next().is_none();
        ensure_capacity(
            header,
            usize::from(needs_slot) + usize::from(needs_tpm),
            usize::from(needs_token) + usize::from(needs_tpm),
        )?;
        if !self.tpm_eligible {
            return Ok(());
        }
        let recovery = PlannedWrite { entry, key, token };
        if needs_tpm {
            let tpm_entry = self.device.plan_slot_avoiding(Some(entry))?;
            let (mut tpm_token, tpm_key) = stage_tpm(self.device)?;
            tpm_token["keyslots"] = json!([tpm_entry.slot.to_string()]);
            let tpm = PlannedWrite {
                entry: tpm_entry,
                key: &tpm_key,
                token: &tpm_token,
            };
            // A reissue already has an acknowledged key, so commit the
            // proved TPM allocation first. Even power loss before recovery
            // acknowledgement cannot consume the space reserved for TPM.
            if self.state.acknowledged() {
                self.device
                    .prove_capacity(header, self.unlock, &[tpm, recovery])
                    .context(
                        "LUKS2 capacity cannot fit both TPM enrollment and recovery reissue",
                    )?;
                self.install_tpm(tpm_entry, tpm_token, &tpm_key)?;
            } else {
                self.device
                    .prove_capacity(header, self.unlock, &[recovery, tpm])
                    .context("LUKS2 capacity cannot fit recovery and TPM enrollment")?;
                self.staged_tpm = Some((tpm_entry, tpm_token, tpm_key));
            }
        } else {
            if self.state.acknowledged() {
                // Confirm that the allocation already present is usable
                // before issuing an optional key. Stale TPM needs repair.
                self.enroll_tpm(false)?;
            }
            self.device
                .prove_capacity(header, self.unlock, &[recovery])
                .context("LUKS2 capacity cannot fit the recovery enrollment")?;
        }
        Ok(())
    }

    pub fn acknowledge(&mut self, entry: Enrollment) -> Result<()> {
        recorded_recovery(&json!({"recovery":entry}), &self.device.header()?)?;
        self.state.value["recovery"] = recovery_record(entry, true);
        self.state.value["recovery_attempt"] = Value::Null;
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
        if let Some((entry, token, key)) = self.staged_tpm.take() {
            ensure!(old.is_none(), "TPM header changed after capacity proof");
            return self.install_tpm(entry, token, &key);
        }
        let (mut token, key) = stage_tpm(self.device)?;
        if let Some(old) = old {
            self.commit_tpm(TpmChange::Remove(old))?;
        }
        let entry = self
            .device
            .plan_slot_avoiding(self.pending.as_ref().map(|(entry, _)| *entry))?;
        token["keyslots"] = json!([entry.slot.to_string()]);
        self.install_tpm(entry, token, &key)
    }

    fn install_tpm(&mut self, entry: Enrollment, token: Value, key: &Secret) -> Result<Enrollment> {
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
