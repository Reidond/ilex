use super::secret::key_command;
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::{fs::FileTypeExt, process::CommandExt},
    },
    path::PathBuf,
};

/// A validated LUKS2 block device, addressed by its immutable LUKS UUID.
pub struct Device {
    path: PathBuf,
    pub uuid: String,
    _lock: File,
}

pub(super) struct PlannedWrite<'a> {
    pub entry: Enrollment,
    pub key: &'a Secret,
    pub token: &'a Value,
}

impl Device {
    pub fn open(path: &Path) -> Result<Self> {
        let uuid = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("missing LUKS UUID")?;
        ensure!(
            path == Path::new("/dev/disk/by-uuid").join(uuid),
            "use /dev/disk/by-uuid/<LUKS UUID>"
        );
        ensure!(
            fs::metadata(path)?.file_type().is_block_device(),
            "LUKS target is not a block device"
        );
        let lock = Self::lock_uuid(uuid)?;
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
        device.header()?;
        Ok(device)
    }

    /// Serialize ilex operations for a UUID, including invalidation when the
    /// verified device cannot be opened. Cryptsetup retains its header lock.
    pub(super) fn lock_uuid(uuid: &str) -> Result<File> {
        ensure!(
            uuid.len() == 36
                && uuid.bytes().enumerate().all(|(i, b)| {
                    if [8, 13, 18, 23].contains(&i) {
                        b == b'-'
                    } else {
                        b.is_ascii_hexdigit()
                    }
                }),
            "invalid LUKS UUID"
        );
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create("/run/ilex-enroll")?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(format!(
                "/run/ilex-enroll/{}.lock",
                uuid.to_ascii_lowercase()
            ))?;
        lock.try_lock()
            .context("another enrollment operation is running")?;
        Ok(lock)
    }
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn scratch(&self) -> Result<Scratch> {
        use std::os::unix::fs::OpenOptionsExt;
        let scratch = Scratch(Path::new("/run/ilex-enroll").join(format!("{}.scratch", self.uuid)));
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&scratch.0)?
            .set_len(64 << 20)?;
        Ok(scratch)
    }

    fn keep_lock(&self, command: &mut Command) {
        // Reserve a descriptor above credential FD 3 before spawning. The
        // child retains the lock if the controlling process dies mid-write.
        let lock = self._lock.as_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(lock, libc::F_DUPFD, 10) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    pub(super) fn header(&self) -> Result<Value> {
        header(&self.path)
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

    pub(super) fn plan_slot(&self) -> Result<Enrollment> {
        self.plan_slot_avoiding(None)
    }

    pub(super) fn plan_slot_avoiding(&self, hint: Option<Enrollment>) -> Result<Enrollment> {
        let header = self.header()?;
        Ok(Enrollment {
            token: free_id_avoiding(&header, "tokens", hint.map(|entry| entry.token))?,
            slot: free_id_avoiding(&header, "keyslots", hint.map(|entry| entry.slot))?,
        })
    }

    /// Exercise the qualified allocator on a protected copy. JSON capacity
    /// is measured by libcryptsetup using both actual token payloads and the
    /// same keyslot parameters and IDs as the installed-header writes.
    pub(super) fn prove_capacity(
        &self,
        before: &Value,
        unlock: &Secret,
        writes: &[PlannedWrite<'_>],
    ) -> Result<()> {
        let backup = Scratch(Path::new("/run/ilex-enroll").join(format!("{}.budget", self.uuid)));
        if backup.0.try_exists()? {
            fs::remove_file(&backup.0)?;
        }
        let mut command = Command::new("/usr/sbin/cryptsetup");
        command
            .arg("luksHeaderBackup")
            .arg(self.path())
            .arg("--header-backup-file")
            .arg(&backup.0);
        self.keep_lock(&mut command);
        run(&mut command)?;
        let payload = before["segments"]["0"]["offset"]
            .as_str()
            .context("missing LUKS2 data offset")?
            .parse::<u64>()?;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        fs::set_permissions(&backup.0, fs::Permissions::from_mode(0o600))?;
        let file = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&backup.0)?;
        file.set_len(
            file.metadata()?.len().max(
                payload
                    .checked_add(4 << 20)
                    .context("invalid LUKS2 data offset")?,
            ),
        )?;
        let copy = Self {
            path: backup.0.clone(),
            uuid: self.uuid.clone(),
            _lock: self._lock.try_clone()?,
        };
        ensure!(
            copy.header()? == *before,
            "LUKS2 header changed before capacity check"
        );
        for write in writes {
            copy.complete_slot(&write.entry, unlock, write.key, write.token)?;
        }
        Ok(())
    }

    /// Install or resume the exact preplanned recovery slot. The caller
    /// persisted the protected key before allocation.
    pub(super) fn complete_slot(
        &self,
        enrollment: &Enrollment,
        unlock: &Secret,
        key: &Secret,
        token: &Value,
    ) -> Result<()> {
        ensure!(
            enrollment.slot < 32 && enrollment.token < 32,
            "invalid LUKS2 slot plan"
        );
        let slot_id = enrollment.slot.to_string();
        let token_id = enrollment.token.to_string();
        ensure!(
            slot(token)? == enrollment.slot,
            "token does not match slot plan"
        );
        let before = self.header()?;
        let existing_token = before["tokens"].get(&token_id);
        ensure!(
            existing_token.is_none_or(|old| old == token),
            "planned LUKS2 token was reused"
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
            "planned LUKS2 slot belongs to a different token"
        );
        if before["keyslots"].get(&slot_id).is_none() {
            ensure!(existing_token.is_none(), "pending LUKS2 slot missing");
            ensure_capacity(&before, 1, 1)?;
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
            // Both enrolled keys have full entropy. Use fixed parameters as
            // systemd does, including when proving JSON capacity on a copy.
            command.args(["--pbkdf", "pbkdf2", "--pbkdf-force-iterations", "1000"]);
            self.keep_lock(&mut command);
            key_command(&mut command, &unlock_fd);
            run_with_input(&mut command, Stdio::from(key.fd()?))?;
        }
        self.authenticate_slot(key.fd()?, enrollment.slot)?;
        if existing_token.is_none() {
            let mut import = Command::new("/usr/sbin/cryptsetup");
            import
                .args(["token", "import", "--json-file", "-", "--token-id"])
                .arg(&token_id)
                .arg(self.path());
            self.keep_lock(&mut import);
            run_with_input(
                &mut import,
                Stdio::from(secret_fd(&serde_json::to_vec(token)?)?),
            )?;
        }
        let after = self.header()?;
        preserve_entries(&before, &after)?;
        ensure!(
            after["tokens"].get(&token_id) == Some(token),
            "pending LUKS2 token missing"
        );
        Ok(())
    }

    pub(super) fn remove_token(&self, token: &str) -> Result<()> {
        let mut command = Command::new("/usr/sbin/cryptsetup");
        command
            .args(["token", "remove", "--token-id", token])
            .arg(self.path());
        self.keep_lock(&mut command);
        run(&mut command)?;
        Ok(())
    }

    pub(super) fn remove_slot(&self, slot: &str) -> Result<()> {
        let mut command = Command::new("/usr/sbin/cryptsetup");
        command
            .args(["luksKillSlot", "--batch-mode"])
            .arg(self.path())
            .arg(slot);
        self.keep_lock(&mut command);
        run(&mut command)?;
        Ok(())
    }
}

pub(super) fn free_id(header: &Value, field: &str) -> Result<u32> {
    free_id_avoiding(header, field, None)
}

fn free_id_avoiding(header: &Value, field: &str, hint: Option<u32>) -> Result<u32> {
    (0..32)
        .filter(|id| header[field].get(id.to_string()).is_none())
        .min_by_key(|id| Some(*id) == hint)
        .with_context(|| format!("no free LUKS2 identifier in {field}"))
}

pub(super) fn ensure_capacity(header: &Value, required: usize, tokens: usize) -> Result<()> {
    let slots = header["keyslots"]
        .as_object()
        .context("missing LUKS2 keyslots")?;
    let mut available = 32usize.saturating_sub(slots.len());
    if required > 0 {
        match area_capacity(header) {
            Ok(areas) => available = available.min(areas),
            // If an older layout cannot be budgeted, still let the normal
            // single enrollment ask libcryptsetup to allocate it. Never spend
            // a slot on a reissue when two allocations cannot be confirmed.
            Err(error) if required > 1 => return Err(error),
            Err(_) => {}
        }
    }
    ensure!(
        required != 1 || available > 0,
        "LUKS2 has no free keyslot or keyslot area for the remaining enrollment. Existing recovery acknowledgement is unchanged."
    );
    ensure!(
        available >= required,
        "Completion needs {required} free LUKS2 keyslots, including the slot for pending TPM enrollment; {available} available. Existing recovery acknowledgement is unchanged."
    );
    let free_tokens = 32usize.saturating_sub(
        header["tokens"]
            .as_object()
            .context("missing LUKS2 tokens")?
            .len(),
    );
    ensure!(
        free_tokens >= tokens,
        "Completion needs {tokens} free LUKS2 token IDs; {free_tokens} available. Existing recovery acknowledgement is unchanged."
    );
    Ok(())
}

fn area_capacity(header: &Value) -> Result<usize> {
    const ALIGNMENT: u64 = 4096;
    const STRIPES: u64 = 4000;
    let number = |field: &Value| -> Result<u64> {
        field
            .as_str()
            .context("missing LUKS2 area geometry")?
            .parse()
            .context("invalid LUKS2 area geometry")
    };
    let start = number(&header["config"]["json_size"])?
        .checked_add(ALIGNMENT)
        .and_then(|n| n.checked_mul(2))
        .context("invalid LUKS2 area start")?;
    let end = start
        .checked_add(number(&header["config"]["keyslots_size"])?)
        .context("invalid LUKS2 area end")?;
    let mut areas = Vec::new();
    let mut key_bytes = None;
    for slot in header["keyslots"]
        .as_object()
        .context("missing LUKS2 keyslots")?
        .values()
    {
        let bytes = slot["key_size"]
            .as_u64()
            .context("unknown LUKS2 key size")?;
        ensure!(
            slot["type"] == "luks2"
                && (1..=64).contains(&bytes)
                && key_bytes.is_none_or(|old| old == bytes),
            "cannot establish LUKS2 allocation size"
        );
        key_bytes = Some(bytes);
        let offset = number(&slot["area"]["offset"])?;
        let size = number(&slot["area"]["size"])?;
        let end = offset
            .checked_add(size)
            .context("invalid LUKS2 keyslot area")?;
        ensure!(
            size > 0 && offset.is_multiple_of(ALIGNMENT) && end.is_multiple_of(ALIGNMENT),
            "unaligned LUKS2 keyslot area"
        );
        areas.push((offset, end));
    }
    let allocation =
        (key_bytes.context("missing LUKS2 key size")? * STRIPES).next_multiple_of(ALIGNMENT);
    areas.sort_unstable();
    let mut cursor = start;
    let mut count = 0;
    for (offset, area_end) in areas {
        ensure!(
            offset >= cursor && area_end <= end,
            "overlapping or out-of-range LUKS2 keyslot area"
        );
        count += (offset - cursor) / allocation;
        cursor = area_end;
    }
    Ok((count + (end - cursor) / allocation).min(32) as usize)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    device.refuse_unjournaled_reissue(&before)?;
    ensure_capacity(&before, 1, 1)?;
    // A retry must inspect/acknowledge an existing key, or explicitly reissue it
    // in an authenticated UI. This primitive never deletes existing recovery.
    let mut command = Command::new("/usr/bin/systemd-cryptenroll");
    command
        .args(["--recovery-key", "--unlock-key-file=/proc/self/fd/3"])
        .arg(device.path());
    device.keep_lock(&mut command);
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

pub(super) fn trial_token(device: &Device, token: u32) -> Result<()> {
    run(Command::new("/usr/bin/timeout")
        .args([
            "--kill-after=5",
            "45",
            "/usr/sbin/cryptsetup",
            "open",
            "--test-passphrase",
            "--token-only",
            "--token-id",
            &token.to_string(),
        ])
        .arg(device.path()))?;
    Ok(())
}

/// Run systemd's separate keyslot/token writes on an unrelated disposable
/// header. A crash here cannot leave an anonymous slot on the real device.
pub(super) fn stage_tpm(device: &Device) -> Result<(Value, Secret)> {
    use std::io::Read;
    let scratch = device.scratch()?;
    let mut passphrase = Secret::empty()?;
    File::open("/dev/urandom")?.read_exact(passphrase.buffer(32)?)?;
    let fd = passphrase.fd()?;
    let mut format = Command::new("/usr/sbin/cryptsetup");
    format
        .args([
            "luksFormat",
            "--batch-mode",
            "--type",
            "luks2",
            "--pbkdf",
            "pbkdf2",
            "--pbkdf-force-iterations",
            "1000",
            "--key-file",
            "/proc/self/fd/3",
        ])
        .arg(&scratch.0);
    device.keep_lock(&mut format);
    key_command(&mut format, &fd);
    run(&mut format)?;
    let before = header(&scratch.0)?;
    let mut command = Command::new("/usr/bin/timeout");
    command
        .args([
            "--kill-after=5",
            "90",
            "/usr/bin/systemd-cryptenroll",
            "--unlock-key-file=/proc/self/fd/3",
            "--tpm2-device=auto",
            "--tpm2-pcrs=7:sha256",
            "--tpm2-with-pin=no",
            "--tpm2-public-key=",
            "--tpm2-public-key-pcrs=",
            "--tpm2-signature=",
            "--tpm2-pcrlock=",
        ])
        .arg(&scratch.0);
    device.keep_lock(&mut command);
    key_command(&mut command, &fd);
    run(&mut command)?;
    let after = header(&scratch.0)?;
    let added = new_enrollment(&before, &after, "systemd-tpm2")?;
    let token = after["tokens"][added.token.to_string()].clone();
    validate_token(&token)?;
    let key = super::crypt::unseal(&scratch.0, added.token)?;
    Ok((token, key))
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
