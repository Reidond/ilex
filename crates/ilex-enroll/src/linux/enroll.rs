use super::secret::key_command;
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::Write,
    os::{fd::OwnedFd, unix::fs::FileTypeExt},
    path::PathBuf,
};

/// A validated LUKS2 block device, addressed by its immutable LUKS UUID.
pub struct Device {
    path: PathBuf,
    pub uuid: String,
    _lock: File,
}
impl Device {
    pub fn open(path: &Path) -> Result<Self> {
        let uuid = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("missing LUKS UUID")?;
        ensure!(
            uuid.len() == 36
                && uuid
                    .bytes()
                    .enumerate()
                    .all(|(i, b)| if [8, 13, 18, 23].contains(&i) {
                        b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    }),
            "invalid LUKS UUID"
        );
        ensure!(
            path == Path::new("/dev/disk/by-uuid").join(uuid),
            "use /dev/disk/by-uuid/<LUKS UUID>"
        );
        ensure!(
            fs::metadata(path)?.file_type().is_block_device(),
            "LUKS target is not a block device"
        );
        // Serialize ilex operations across processes. cryptsetup retains its own
        // header lock. UUID comes from the block device, not user-supplied state.
        fs::create_dir_all("/run/ilex-enroll")?;
        use std::os::unix::fs::OpenOptionsExt;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(format!("/run/ilex-enroll/{uuid}.lock"))?;
        lock.lock()?;
        let output = run(Command::new("/usr/sbin/cryptsetup")
            .arg("luksUUID")
            .arg(path))?;
        ensure!(
            String::from_utf8(output.stdout)?
                .trim()
                .eq_ignore_ascii_case(uuid),
            "LUKS UUID changed"
        );
        let device = Self {
            path: path.into(),
            uuid: uuid.to_ascii_lowercase(),
            _lock: lock,
        };
        device.header()?; // --dump-json-metadata requires LUKS2.
        Ok(device)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub(super) fn header(&self) -> Result<Value> {
        let value = json(
            Command::new("/usr/sbin/cryptsetup")
                .args(["luksDump", "--dump-json-metadata"])
                .arg(&self.path),
        )?;
        ensure!(
            value["keyslots"].is_object() && value["tokens"].is_object(),
            "not LUKS2"
        );
        Ok(value)
    }
    pub fn authenticate(&self, credential: OwnedFd) -> Result<()> {
        self.authenticate_with_slot(credential, None)
    }

    pub fn authenticate_slot(&self, credential: OwnedFd, slot: u32) -> Result<()> {
        self.authenticate_with_slot(credential, Some(slot))
    }

    fn authenticate_with_slot(&self, credential: OwnedFd, slot: Option<u32>) -> Result<()> {
        let key = Secret::read(credential)?.fd()?;
        let mut command = Command::new("/usr/sbin/cryptsetup");
        command.args([
            "open",
            "--test-passphrase",
            "--type",
            "luks2",
            // open otherwise tries available tokens before the key file.
            // Exclude external plugins and every built-in token type so
            // success proves the supplied credential itself was accepted.
            "--disable-external-tokens",
            "--token-type=none",
            "--key-file",
            "/proc/self/fd/3",
        ]);
        if let Some(slot) = slot {
            command.arg("--key-slot").arg(slot.to_string());
        }
        command.arg(&self.path);
        key_command(&mut command, &key);
        run(&mut command)?;
        Ok(())
    }

    pub fn ensure_free_slots(&self, required: usize) -> Result<()> {
        ensure_free_slots(&self.header()?, required)
    }

    pub fn plan_recovery(&self) -> Result<Enrollment> {
        let header = self.header()?;
        let free_id = |field: &str| {
            (0..32)
                .find(|id| header[field].get(id.to_string()).is_none())
                .with_context(|| format!("no free LUKS2 identifier in {field}"))
        };
        Ok(Enrollment {
            token: free_id("tokens")?,
            slot: free_id("keyslots")?,
        })
    }

    /// Install or resume the exact preplanned recovery slot. The caller
    /// persisted the protected key before allocation.
    pub fn complete_recovery_slot(
        &self,
        enrollment: &Enrollment,
        unlock: &Secret,
        key: &Secret,
    ) -> Result<()> {
        ensure!(
            enrollment.slot < 32 && enrollment.token < 32,
            "invalid recovery slot plan"
        );
        let slot_id = enrollment.slot.to_string();
        let token_id = enrollment.token.to_string();
        let token = serde_json::json!({"type":"systemd-recovery", "keyslots":[&slot_id]});
        let before = self.header()?;
        let existing_token = before["tokens"].get(&token_id);
        ensure!(
            existing_token.is_none_or(|old| old == &token),
            "planned recovery token was reused"
        );
        ensure!(
            before["tokens"]
                .as_object()
                .unwrap()
                .iter()
                .all(|(id, value)| {
                    id == &token_id
                        || !value["keyslots"]
                            .as_array()
                            .is_some_and(|slots| slots.iter().any(|slot| slot == &slot_id))
                }),
            "planned recovery slot belongs to a different token"
        );
        if before["keyslots"].get(&slot_id).is_none() {
            ensure!(existing_token.is_none(), "pending recovery slot missing");
            ensure_free_slots(&before, 1)?;
            let unlock_fd = unlock.fd()?;
            let mut command = Command::new("/usr/sbin/cryptsetup");
            command
                .args([
                    "luksAddKey",
                    "--batch-mode",
                    "--key-file",
                    "/proc/self/fd/3",
                    "--new-key-slot",
                ])
                .arg(&slot_id)
                .arg(self.path())
                .arg("-");
            key_command(&mut command, &unlock_fd);
            run_with_input(&mut command, Stdio::from(key.fd()?))?;
        }
        self.authenticate_slot(key.fd()?, enrollment.slot)?;
        if existing_token.is_none() {
            run_with_input(
                Command::new("/usr/sbin/cryptsetup")
                    .args(["token", "import", "--json-file", "-", "--token-id"])
                    .arg(&token_id)
                    .arg(self.path()),
                Stdio::from(secret_fd(&serde_json::to_vec(&token)?)?),
            )?;
        }
        let after = self.header()?;
        preserve_entries(&before, &after)?;
        ensure!(
            after["tokens"].get(&token_id) == Some(&token),
            "pending recovery token missing"
        );
        Ok(())
    }
}

fn ensure_free_slots(header: &Value, required: usize) -> Result<()> {
    // LUKS2 exposes slot IDs 0 through 31. Allocation may still fail if a
    // header with a smaller keyslot area runs out of storage first.
    let slots = header["keyslots"]
        .as_object()
        .context("missing LUKS2 keyslots")?;
    let available = 32usize.saturating_sub(slots.len());
    ensure!(
        required != 1 || available > 0,
        "LUKS2 has no free keyslot for the remaining enrollment. Existing recovery acknowledgement is unchanged."
    );
    ensure!(
        available >= required,
        "Completion needs {required} free LUKS2 keyslots, including the slot for pending TPM enrollment; {available} available. Existing recovery acknowledgement is unchanged."
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub token: u32,
    pub slot: u32,
}
pub(super) fn slot(token: &Value) -> Result<u32> {
    let slots = token["keyslots"]
        .as_array()
        .context("missing token keyslots")?;
    ensure!(slots.len() == 1, "ambiguous token keyslots");
    slots[0]
        .as_str()
        .context("invalid keyslot")?
        .parse()
        .context("invalid keyslot id")
}

/// A strict allowlist rejects policy extensions we have not qualified. Blob and
/// SRK values remain private to the header; callers receive ids and policy only.
pub fn validate_token(token: &Value) -> Result<()> {
    ensure!(
        token["type"] == "systemd-tpm2"
            && token["tpm2-pcrs"] == serde_json::json!([7])
            && token["tpm2-pcr-bank"] == "sha256"
            && (token["tpm2-pin"].is_null() || token["tpm2-pin"] == false),
        "unexpected TPM policy"
    );
    slot(token)?;
    for (key, value) in token.as_object().context("invalid token")? {
        match key.as_str() {
            "type" | "keyslots" | "tpm2-blob" | "tpm2-pcrs" | "tpm2-pcr-bank"
            | "tpm2-primary-alg" | "tpm2-policy-hash" | "tpm2-pin" | "tpm2_srk" | "tpm2-srk" => {}
            "tpm2-pcrlock" if value == false => {}
            _ => anyhow::bail!("unqualified TPM token field"),
        }
    }
    ensure!(
        token["tpm2-blob"].is_string() && token["tpm2-policy-hash"].is_string(),
        "missing sealed policy"
    );
    Ok(())
}

pub fn status(device: &Device) -> Result<Value> {
    let header = device.header()?;
    let tokens: Vec<Value> = header["tokens"]
        .as_object()
        .context("missing tokens")?
        .iter()
        .map(|(id, t)| {
            serde_json::json!({"id": id, "type": t["type"], "keyslots": t["keyslots"],
            "pcr7_sha256": validate_token(t).is_ok()})
        })
        .collect();
    Ok(serde_json::json!({"schema":1, "uuid":device.uuid,
        "slots":header["keyslots"].as_object().context("missing slots")?.keys().collect::<Vec<_>>(), "tokens":tokens}))
}
fn preserve_entries(before: &Value, after: &Value) -> Result<()> {
    // All old slots and tokens must survive unchanged; never wipe or replace.
    for field in ["keyslots", "tokens"] {
        for (id, old) in before[field].as_object().context("invalid LUKS header")? {
            ensure!(
                after[field].get(id) == Some(old),
                "preexisting LUKS metadata changed"
            );
        }
    }
    Ok(())
}

fn new_enrollment(before: &Value, after: &Value, kind: &str) -> Result<Enrollment> {
    preserve_entries(before, after)?;
    let added: Vec<_> = after["tokens"]
        .as_object()
        .context("missing tokens")?
        .iter()
        .filter(|(id, _)| before["tokens"].get(*id).is_none())
        .collect();
    ensure!(
        added.len() == 1 && added[0].1["type"] == kind,
        "unexpected enrollment result"
    );
    let enrollment = Enrollment {
        token: added[0].0.parse()?,
        slot: slot(added[0].1)?,
    };
    ensure!(
        before["keyslots"]
            .get(enrollment.slot.to_string())
            .is_none(),
        "enrollment reused an old slot"
    );
    Ok(enrollment)
}

pub fn recovery(device: &Device, unlock: OwnedFd, key_out: OwnedFd) -> Result<Enrollment> {
    harden()?;
    let key = Secret::read(unlock)?.fd()?;
    device.authenticate(key.try_clone()?)?;
    let before = device.header()?;
    ensure_free_slots(&before, 1)?;
    // A retry must inspect/acknowledge an existing key, or explicitly reissue it
    // in an authenticated UI. This primitive never deletes existing recovery.
    let mut command = Command::new("/usr/bin/systemd-cryptenroll");
    command
        .args(["--recovery-key", "--unlock-key-file=/proc/self/fd/3"])
        .arg(device.path());
    key_command(&mut command, &key);
    let mut output = run(&mut command)?;
    let result = (|| {
        ensure!(output.stdout.len() <= 1024, "invalid recovery output");
        // v262 emits the recovery key alone when stdout is not a terminal.
        let bytes = output.stdout.trim_ascii();
        ensure!(
            bytes.len() == 71
                && bytes.iter().enumerate().all(|(i, b)| if i % 9 == 8 {
                    *b == b'-'
                } else {
                    b"cbdefghijklnrtuv".contains(b)
                }),
            "unqualified recovery format"
        );
        let enrollment = new_enrollment(&before, &device.header()?, "systemd-recovery")?;
        File::from(key_out).write_all(bytes)?;
        Ok(enrollment)
    })();
    output.stdout.zeroize();
    result
}

pub fn tpm(
    device: &Device,
    unlock: OwnedFd,
    target_tools: &std::collections::BTreeMap<String, String>,
) -> Result<Enrollment> {
    harden()?;
    let eligible = probe();
    ensure!(
        eligible.verdict == "eligible",
        "TPM enrollment unavailable: {}",
        eligible.reason
    );
    ensure!(
        &eligible.tools == target_tools,
        "TPM enrollment unavailable: tool-mismatch"
    );
    let key = Secret::read(unlock)?.fd()?;
    device.authenticate(key.try_clone()?)?;
    let before = device.header()?;
    // Reuse an exact existing policy only after authenticating the passphrase
    // and proving this token can currently unseal. Never silently reseal.
    super::layout::verify_header_cipher(&before)?;
    let tokens = before["tokens"].as_object().context("missing tokens")?;
    let existing: Vec<_> = tokens
        .iter()
        .filter(|(_, t)| t["type"] == "systemd-tpm2")
        .collect();
    if !existing.is_empty() {
        ensure!(existing.len() == 1, "ambiguous preexisting TPM enrollment");
        let (id, token) = existing[0];
        validate_token(token)?;
        run(Command::new("/usr/sbin/cryptsetup")
            .args([
                "open",
                "--test-passphrase",
                "--token-only",
                "--token-id",
                id,
            ])
            .arg(device.path()))?;
        return Ok(Enrollment {
            token: id.parse()?,
            slot: slot(token)?,
        });
    }
    ensure_free_slots(&before, 1).context("TPM enrollment needs a free keyslot")?;
    let mut command = Command::new("/usr/bin/systemd-cryptenroll");
    command
        .args([
            "--unlock-key-file=/proc/self/fd/3",
            "--tpm2-device=auto",
            "--tpm2-pcrs=7:sha256",
            "--tpm2-with-pin=no",
            "--tpm2-public-key=",
            "--tpm2-public-key-pcrs=",
            "--tpm2-signature=",
            "--tpm2-pcrlock=",
        ])
        .arg(device.path());
    key_command(&mut command, &key);
    run(&mut command)?;
    let after = device.header()?;
    let added = new_enrollment(&before, &after, "systemd-tpm2")?;
    validate_token(&after["tokens"][added.token.to_string()])?;
    let before_slots: BTreeSet<_> = before["keyslots"].as_object().unwrap().keys().collect();
    ensure!(!before_slots.is_empty(), "missing fallback passphrase slot");
    run(Command::new("/usr/sbin/cryptsetup")
        .args([
            "open",
            "--test-passphrase",
            "--token-only",
            "--token-id",
            &added.token.to_string(),
        ])
        .arg(device.path()))?;
    Ok(added)
}
