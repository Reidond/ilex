//! Local console adapter. The library is unaware of Anaconda, terminals and
//! acknowledgement. Anaconda's %post only starts this adapter on a reserved VT.
use super::*;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::OpenOptionsExt,
    },
    path::Path,
    process::Command,
};

struct Console {
    file: File,
    settings: libc::termios,
}
impl Console {
    fn open(path: &Path) -> Result<Self> {
        // Only a virtual console is allowed: a remote Cockpit/web session and
        // serial log must never receive recovery material implicitly.
        ensure!(
            path == Path::new("/dev/tty8"),
            "completion requires the reserved local tty8"
        );
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_NOFOLLOW)
            .open(path)?;
        file.try_lock()
            .context("local setup console is already in use")?;
        let mut mode: libc::c_int = -1;
        // SAFETY: KDGETMODE writes one int. Refuse a console owned by a GUI;
        // Fedora Anaconda itself runs GNOME Kiosk on VT6.
        ensure!(
            unsafe { libc::ioctl(file.as_raw_fd(), 0x4b3b, &mut mode) } == 0 && mode == 0,
            "completion requires a text-mode virtual console"
        );
        // SAFETY: tcgetattr initializes a termios for this live terminal.
        let mut settings = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::tcgetattr(file.as_raw_fd(), &mut settings) } == 0,
            "local console unavailable"
        );
        let mut private = settings;
        private.c_lflag &= !(libc::ECHO | libc::ECHONL);
        ensure!(
            unsafe { libc::tcsetattr(file.as_raw_fd(), libc::TCSAFLUSH, &private) } == 0,
            "cannot disable echo"
        );
        Ok(Self { file, settings })
    }
    fn clear(&mut self) -> Result<()> {
        // Erase screen and terminal scrollback before/after key presentation.
        self.file.write_all(b"\x1b[2J\x1b[3J\x1b[H")?;
        self.file.flush()?;
        Ok(())
    }
    fn say(&mut self, text: &str) -> Result<()> {
        self.file.write_all(text.as_bytes())?;
        self.file.flush()?;
        Ok(())
    }
    fn read(&mut self, prompt: &str) -> Result<Secret> {
        self.say(prompt)?;
        let mut secret = Secret::empty()?;
        loop {
            let mut byte = [0u8];
            ensure!(self.file.read(&mut byte)? == 1, "local console closed");
            if byte[0] == b'\n' || byte[0] == b'\r' {
                break;
            }
            secret.push(byte[0])?;
            use zeroize::Zeroize;
            byte.zeroize();
        }
        self.say("\r\n")?;
        Ok(secret)
    }
}
impl Drop for Console {
    fn drop(&mut self) {
        let _ = self.clear();
        // SAFETY: restore the settings for the same live terminal.
        unsafe {
            libc::tcsetattr(self.file.as_raw_fd(), libc::TCSAFLUSH, &self.settings);
        }
    }
}
fn out_fd() -> Result<File> {
    // SAFETY: constant name; returned descriptor has one owner.
    let fd = unsafe { libc::memfd_create(c"ilex-recovery-output".as_ptr(), libc::MFD_CLOEXEC) };
    ensure!(fd >= 0, "cannot create recovery output descriptor");
    Ok(unsafe { File::from_raw_fd(fd) })
}
pub fn complete(root: &Path, tty: &Path, reissue: bool) -> Result<()> {
    let (layout, refused) = match discover(root) {
        Ok(Some(layout)) => (layout, false),
        Ok(None) => return Ok(()),
        Err(error) => {
            eprintln!("TPM enrollment skipped: {error:#}");
            match recovery_layout(root).context("storage discovery incomplete; retry local completion once the devices are available")? {
                Some(layout) => (layout, true),
                None => {
                    // A verified plaintext stack has no LUKS device to bind.
                    update_setup(
                        root,
                        "unavailable",
                        "unsupported-storage-layout",
                        "unavailable",
                    )?;
                    eprintln!(
                        "Installation can finish. Use any existing storage credentials for recovery. No LUKS recovery key was issued because a unique encrypted root could not be verified. Choose a supported encrypted layout to enable TPM enrollment."
                    );
                    return Ok(());
                }
            }
        }
    };
    let device = Device::open(&layout.device)?;
    let mut state = SecurityState::open(root, &device)?;
    if refused
        && !reissue
        && state.acknowledged()
        && !state.recovery_pending()?
        && !state.tpm_pending()?
    {
        recorded_recovery(&state.value, &device.header()?)?;
        state.value["state"] = json!("unavailable");
        state.value["reason"] = json!("unsupported-storage-layout");
        state.save()?;
        update_setup(
            root,
            "unavailable",
            "unsupported-storage-layout",
            "passphrase-only",
        )?;
        eprintln!(
            "Installation can finish. TPM enrollment is unavailable for this storage layout. Use the saved recovery key or disk passphrase at boot."
        );
        return Ok(());
    }
    let (capability, tools, reason) = if refused {
        (None, Default::default(), "unsupported-storage-layout")
    } else {
        let (capability, tools, reason) = eligibility(root);
        (Some(capability), tools, reason)
    };
    let eligible = reason == "pcr7-sha256";
    let wants_recovery = !state.acknowledged() || reissue;
    let legacy_confirmation = !state.acknowledged()
        && !reissue
        && !state.recovery_pending()?
        && state.value["recovery"].is_object();
    device.preflight(&state, wants_recovery && !legacy_confirmation, eligible)?;
    let mut console = Console::open(tty)?;
    console.clear()?;
    console.say("ilex encrypted installation — local completion\r\n\r\nThe administrator account is for maintenance and recovery.\r\nThe disk passphrase remains a recovery option.\r\n\r\n")?;
    if refused {
        console.say("TPM enrollment is unavailable for this storage layout.\r\nInstallation will finish with local recovery.\r\n\r\n")?;
    }
    let passphrase = console.read("Re-enter the disk passphrase (input is hidden): ")?;
    let mut session = Reconciled::open(&device, &passphrase, state, eligible)?;
    if !refused {
        configure_crypttab(root, &layout.uuid)?;
    }
    if wants_recovery {
        let entry = if legacy_confirmation {
            let entry = recorded_recovery(&session.state.value, &device.header()?)?;
            let confirmation =
                console.read("Enter the previously displayed recovery key (input is hidden): ")?;
            device.authenticate_slot(confirmation.fd()?, entry.slot)
                .context("saved key did not unlock its slot; explicit reissue can create a new key when space is available")?;
            entry
        } else {
            let (entry, key) = session.recovery(reissue)?;
            display_recovery(&mut console, &device, key, entry.slot)?;
            entry
        };
        session.acknowledge(entry)?;
    }
    if eligible {
        session.enroll_tpm(false)?;
    } else {
        session.state.value["state"] = json!("unavailable");
    }
    session.state.value["probe"] = serde_json::to_value(&capability)?;
    session.state.value["target_tools"] = serde_json::to_value(tools)?;
    session.state.value["reason"] = json!(reason);
    session.state.save()?;
    update_setup(
        root,
        capability.as_ref().map_or("unavailable", |p| p.verdict),
        reason,
        if eligible {
            "enrolled-unverified"
        } else {
            "passphrase-only"
        },
    )?;
    finish_console(
        &mut console,
        if eligible {
            "TPM enrolled to PCR 7 (SHA-256). Unlock verification is pending.\r\n"
        } else {
            "TPM enrollment unavailable. Use the recovery key or disk passphrase.\r\n"
        },
    )
}

fn display_recovery(console: &mut Console, device: &Device, key: &Secret, slot: u32) -> Result<()> {
    console.say("\r\nWrite this recovery key on paper or store it offline.\r\nIt is shown only on this local console. Do not photograph it.\r\n\r\n")?;
    console.file.write_all(key.bytes())?;
    console.say("\r\n\r\nPress Enter when it is saved offline.\r\n")?;
    drop(console.read("")?);
    console.clear()?;
    let confirmation =
        console.read("Enter the saved recovery key to confirm (input is hidden): ")?;
    ensure!(
        confirmation.bytes() == key.bytes(),
        "recovery verification failed; retry can redisplay the pending key"
    );
    device.authenticate_slot(confirmation.fd()?, slot)
}

pub fn repair() -> Result<Value> {
    harden()?;
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "security repair requires root"
    );
    let root = Path::new("/");
    ensure!(
        Path::new("/run/ostree-booted").exists() && !Path::new("/etc/initrd-release").exists(),
        "security repair requires the installed system"
    );
    let Some(layout) = discover(root).ok().flatten() else {
        return Ok(
            json!({"schema":1, "state":"unavailable", "reason":"unsupported-storage-layout"}),
        );
    };
    let device = Device::open(&layout.device)?;
    let state = SecurityState::open(root, &device)?;
    let (capability, tools, reason) = eligibility(root);
    if reason != "pcr7-sha256" {
        return Ok(json!({"schema":1, "state":"unavailable", "reason":reason}));
    }
    ensure!(
        state.acknowledged(),
        "confirm the saved recovery key with local completion first"
    );
    let boot = super::transaction::current_boot()?;
    let evidence = super::verify::current_observation(&device, &boot)?;
    ensure!(
        evidence["method"] == "fallback"
            || (evidence["method"] == "tpm2" && state.tpm_pending()?),
        "repair requires this boot's root fallback evidence or an interrupted TPM transaction"
    );
    let tty = Path::new("/dev/tty8");
    eprintln!("TPM repair requires a disk credential on the local console (Ctrl+Alt+F8).");
    let mut console = Console::open(tty)?;
    console.clear()?;
    let unlock = console.read(
        "Enter the disk passphrase or saved recovery key for TPM repair (input is hidden): ",
    )?;
    let mut session = Reconciled::open(&device, &unlock, state, true)?;
    let enrolled = session.enroll_tpm(true)?;
    session.state.value["probe"] = serde_json::to_value(capability)?;
    session.state.value["target_tools"] = serde_json::to_value(tools)?;
    session.state.save()?;
    Ok(
        json!({"schema":1, "uuid":device.uuid, "state":session.state.value["state"], "tpm":enrolled,
        "reason":"check-next-cold-boot"}),
    )
}

fn finish_console(console: &mut Console, message: &str) -> Result<()> {
    console.clear()?;
    console.say("Recovery key saved and privately verified.\r\n\r\n")?;
    console.say(message)?;
    console.say("Installation completion is ready. Press Enter to return to Anaconda.\r\n")?;
    drop(console.read("")?);
    Ok(())
}

fn update_setup(root: &Path, capability: &str, reason: &str, status: &str) -> Result<()> {
    let path = root.join("etc/ilex/setup.json");
    let permissions = fs::metadata(&path)?.permissions();
    let mut setup: Value = serde_json::from_slice(&fs::read(&path)?)?;
    ensure!(setup["schema"] == 1, "unsupported setup schema");
    setup["security"]["tpm"] = json!({
        "requested":"auto", "capability":capability, "reason":reason, "status":status
    });
    write_state(&path, &setup)?;
    fs::set_permissions(&path, permissions)?;
    Ok(())
}

pub fn report_boot() -> Result<()> {
    let path = Path::new("/etc/ilex/setup.json");
    let setup: Value = serde_json::from_slice(&fs::read(path)?)?;
    let tpm = &setup["security"]["tpm"];
    if tpm["reason"] == "unsupported-storage-layout" {
        if tpm["status"] == "passphrase-only" {
            let state: Option<Value> = fs::read("/var/lib/ilex/security/enrollment.json")
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            if state
                .as_ref()
                .is_some_and(|state| state["uuid"].is_string() && acknowledged_recovery(state))
            {
                println!(
                    "ilex: TPM enrollment was skipped for this storage layout. Use the disk passphrase or saved recovery key at boot."
                );
            } else {
                println!(
                    "ilex: TPM enrollment was skipped for this storage layout. Use the disk passphrase and confirm a new recovery key with local completion."
                );
            }
        } else {
            println!(
                "ilex: TPM enrollment was skipped for this storage layout. Use the existing storage recovery procedure."
            );
        }
    }
    Ok(())
}

pub fn configure_crypttab(root: &Path, uuid: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("etc/crypttab");
    let metadata = fs::symlink_metadata(&path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "crypttab must be a regular file"
    );
    let original = fs::read_to_string(&path)?;
    let source = format!("UUID={uuid}");
    let mut updated = String::new();
    let mut changed = false;
    for line in original.lines() {
        let mut fields: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        if fields.get(1) == Some(&source) && !line.trim_start().starts_with('#') {
            // crypttab permits omitting the key-file column (implicit none).
            if fields.len() == 2 {
                fields.push("none".into());
            }
            let mut options: Vec<&str> = fields
                .get(3)
                .map_or("", String::as_str)
                .split(',')
                .filter(|s| !s.is_empty())
                .collect();
            ensure!(
                options
                    .iter()
                    .all(|o| !o.starts_with("tpm2-device=") || *o == "tpm2-device=auto"),
                "conflicting root TPM device option"
            );
            if !options.contains(&"tpm2-device=auto") {
                options.push("tpm2-device=auto");
                let options = options.join(",");
                if fields.len() == 3 {
                    fields.push(options);
                } else {
                    fields[3] = options;
                }
                updated.push_str(&fields.join("\t"));
                changed = true;
            } else {
                updated.push_str(line);
            }
        } else {
            updated.push_str(line);
        }
        updated.push('\n');
    }
    if !changed {
        return Ok(());
    }
    let temporary = root.join(format!("etc/.crypttab-ilex-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(metadata.permissions().mode() & 0o777)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)?;
    let result = (|| {
        file.write_all(updated.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)?;
        File::open(root.join("etc"))?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

pub fn self_check() -> Result<()> {
    // This mode runs only in a booted test guest: use a disposable loopback
    // LUKS2 volume, then test real recovery and passphrase authentication. No TPM
    // operation occurs here; exact-ISO tests own TPM enrollment qualification.
    ensure!(
        Path::new("/run/ostree-booted").exists() || Path::new("/run/ilex-test-guest").exists(),
        "self-check requires a booted test guest"
    );
    // Exercise the actual mounted root, including Btrfs's anonymous mount
    // number and subvolume source. Loopback recovery alone cannot cover it.
    let layout = serde_json::to_value(discover(Path::new("/"))?)?;
    if Path::new("/run/ostree-booted").exists() {
        // Reproduce Anaconda's overmounted sysroot in a private child mount
        // namespace. The guest's own mounts and unlock evidence stay untouched.
        let overmounted = Command::new("unshare")
            .args([
                "--mount",
                "--fork",
                "--propagation",
                "private",
                "/bin/sh",
                "-ec",
                "mount --bind /sysroot /sysroot; mount --bind /sysroot /sysroot; /usr/bin/ilex-enroll layout --root /",
            ])
            .output()?;
        ensure!(
            overmounted.status.success()
                && serde_json::from_slice::<Value>(&overmounted.stdout)
                    .is_ok_and(|observed| observed == layout),
            "overmounted root discovery differs"
        );
    }
    let dir = Path::new("/run").join(format!("ilex-enroll-check-{}", std::process::id()));
    fs::create_dir(&dir)?;
    let image = dir.join("luks.img");
    File::create(&image)?.set_len(64 << 20)?;
    let result = (|| {
        // Exercise the ISO's RPM-less lookup against actual booted-image
        // binaries, and reject a changed executable rather than trusting JSON.
        let metadata = fs::read("/usr/lib/ilex/security/tools.json")?;
        let manifest: Value = serde_json::from_slice(&metadata)?;
        let tools_root = dir.join("tools-root");
        for path in manifest["sha256"]
            .as_object()
            .context("missing tool hashes")?
            .keys()
        {
            let relative = Path::new(path);
            ensure!(
                relative.is_relative()
                    && !relative
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_))),
                "invalid tool path"
            );
            let target = tools_root.join(relative);
            fs::create_dir_all(target.parent().unwrap())?;
            fs::copy(Path::new("/").join(relative), target)?;
        }
        fs::create_dir_all(tools_root.join("usr/lib/ilex/security"))?;
        fs::write(
            tools_root.join("usr/lib/ilex/security/tools.json"),
            metadata,
        )?;
        ensure!(
            tool_versions(&tools_root)? == tool_versions(Path::new("/"))?,
            "RPM-less tool versions differ"
        );
        fs::write(tools_root.join("usr/bin/ilex-enroll"), b"changed tool")?;
        ensure!(
            tool_versions(&tools_root).is_err(),
            "changed tool was accepted"
        );
        println!(
            "ilex-enroll self-check: RPM-less version metadata and changed-tool rejection passed"
        );
        let output = Command::new("losetup")
            .args(["--find", "--show"])
            .arg(&image)
            .output()?;
        ensure!(output.status.success(), "self-check loop setup failed");
        let loopdev = String::from_utf8(output.stdout)?.trim().to_string();
        let run = (|| {
            let mut pass = Secret::empty()?;
            let mut random = File::open("/dev/urandom")?;
            for _ in 0..48 {
                let mut byte = [0];
                random.read_exact(&mut byte)?;
                pass.push(b'a' + byte[0] % 26)?;
            }
            let key = pass.fd()?;
            // A separate child verifies inherited-FD access against cryptsetup.
            let mut command = Command::new("cryptsetup");
            command
                .args([
                    "luksFormat",
                    "--batch-mode",
                    "--type",
                    "luks2",
                    "--luks2-keyslots-size",
                    "768k",
                    "--key-size",
                    "512",
                    "--pbkdf",
                    "pbkdf2",
                    "--pbkdf-force-iterations",
                    "1000",
                    "--key-file",
                    "/proc/self/fd/3",
                ])
                .arg(&loopdev);
            inherit(&mut command, &key);
            ensure!(
                command
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()?
                    .success(),
                "self-check LUKS2 format failed"
            );
            ensure!(
                Command::new("udevadm")
                    .args(["trigger", "--action=change"])
                    .arg(&loopdev)
                    .status()?
                    .success(),
                "udev trigger failed"
            );
            ensure!(
                Command::new("udevadm").args(["settle"]).status()?.success(),
                "udev settle failed"
            );
            let uuid = Command::new("cryptsetup")
                .arg("luksUUID")
                .arg(&loopdev)
                .output()?;
            let uuid = String::from_utf8(uuid.stdout)?;
            let path = Path::new("/dev/disk/by-uuid").join(uuid.trim());
            recovery_guard_self_check(&dir, &path, &pass, uuid.trim())?;
            let device = Device::open(&path)?;
            capacity_self_check(&device)?;
            let mut keys = Vec::new();
            for _ in 0..2 {
                if !keys.is_empty() {
                    let header = device.header()?;
                    super::enroll::ensure_capacity(&header, 1, 1)?;
                    ensure!(
                        super::enroll::ensure_capacity(&header, 2, 2).is_err(),
                        "small keyslot area appeared to fit two more keys"
                    );
                }
                let mut output = out_fd()?;
                let enrolled = recovery(&device, pass.fd()?, output.try_clone()?.into())?;
                output.seek(SeekFrom::Start(0))?;
                keys.push((enrolled, Secret::read(output.into())?));
            }
            ensure!(
                keys[0].0.slot != keys[1].0.slot && keys[0].0.token != keys[1].0.token,
                "second data-volume recovery reused the first enrollment"
            );
            for (enrolled, key) in &keys {
                ensure!(enrolled.slot != 0, "recovery overwrote passphrase");
                device.authenticate_slot(key.fd()?, enrolled.slot)?;
            }
            device.authenticate(pass.fd()?)?;
            let bad = secret_fd(b"ilex-deliberately-incorrect-passphrase")?;
            ensure!(
                device.authenticate(bad).is_err(),
                "incorrect credential accepted"
            );
            token_auth_self_check(&device, &pass)?;
            println!(
                "ilex-enroll self-check: two data-volume recovery keys, private FD handoff and retained passphrase passed"
            );
            Ok(())
        })();
        let detached = Command::new("losetup")
            .arg("--detach")
            .arg(&loopdev)
            .status()?;
        run?;
        ensure!(detached.success(), "self-check loop teardown failed");
        Ok(())
    })();
    fs::remove_dir_all(&dir)?;
    result
}

fn recovery_guard_self_check(dir: &Path, device: &Path, pass: &Secret, uuid: &str) -> Result<()> {
    let mounts = dir.join("mnt");
    let state_dir = mounts.join("sysimage/var/lib/ilex/security");
    fs::create_dir_all(&state_dir)?;
    let state = state_dir.join("enrollment.json");
    let key = pass.fd()?;
    let before = header(device)?;
    let refuse = |reason: &str| -> Result<()> {
        let mut command = Command::new("unshare");
        command
            .args([
                "--mount",
                "--propagation",
                "private",
                "/bin/sh",
                "-ec",
                r#"mount --bind "$1" /mnt
exec /usr/bin/ilex-enroll recovery --device "$2" --unlock-fd 3 --key-out-fd 4 4>/dev/null"#,
                "ilex-check",
            ])
            .arg(&mounts)
            .arg(device);
        inherit(&mut command, &key);
        let output = command.output()?;
        ensure!(
            !output.status.success() && String::from_utf8_lossy(&output.stderr).contains(reason),
            "recovery guard did not refuse the installer fixture for {reason}"
        );
        ensure!(
            header(device)? == before,
            "refused recovery allocation changed the header"
        );
        Ok(())
    };
    write_state(
        &state,
        &json!({"schema":1, "uuid":uuid, "state":"pending", "tpm":null}),
    )?;
    refuse("reconcile pending root enrollment")?;
    fs::write(&state, b"{")?;
    refuse("cannot read root enrollment state under /mnt/sysimage")?;
    fs::remove_file(state)?;
    fs::write(state_dir.join(".pending-recovery.json"), b"{}")?;
    refuse("cannot identify LUKS2 root for enrollment receipt under /mnt/sysimage")
}

fn capacity_self_check(device: &Device) -> Result<()> {
    use super::enroll::ensure_capacity;
    let header = device.header()?;
    ensure_capacity(&header, 2, 2)?;
    let token = serde_json::to_vec(&json!({"type":"ilex-self-check", "keyslots":[]}))?;
    for id in 1..32 {
        run_with_input(
            Command::new("/usr/sbin/cryptsetup")
                .args([
                    "token",
                    "import",
                    "--json-file",
                    "-",
                    "--token-id",
                    &id.to_string(),
                ])
                .arg(device.path()),
            Stdio::from(secret_fd(&token)?),
        )?;
    }
    let header = device.header()?;
    ensure_capacity(&header, 2, 1)?;
    ensure!(
        ensure_capacity(&header, 2, 2).is_err(),
        "one free token ID appeared to fit two tokens"
    );
    for id in 1..32 {
        device.remove_token(&id.to_string())?;
    }
    Ok(())
}

fn token_auth_self_check(device: &Device, pass: &Secret) -> Result<()> {
    // A real in-kernel keyring token is an available automatic unlock route.
    // The credential API must still reject a wrong FD. No TPM is accessed.
    let name = std::ffi::CString::new(format!("ilex-enroll-self-check-{}", std::process::id()))?;
    // SAFETY: KEYCTL_JOIN_SESSION_KEYRING (1) creates a private session for
    // this test process and its children; the caller's session is unchanged.
    let session = unsafe { libc::syscall(libc::SYS_keyctl, 1, name.as_ptr()) };
    ensure!(session >= 0, "self-check session keyring unavailable");
    // SAFETY: constant type, NUL-terminated description and a live bounded
    // secret buffer. add_key copies exactly len bytes to this session keyring.
    let key = unsafe {
        libc::syscall(
            libc::SYS_add_key,
            c"user".as_ptr(),
            name.as_ptr(),
            pass.bytes().as_ptr(),
            pass.bytes().len(),
            session,
        )
    };
    ensure!(key >= 0, "self-check keyring credential unavailable");
    let result = (|| -> Result<()> {
        ensure!(
            Command::new("cryptsetup")
                .args(["token", "add", "--key-description"])
                .arg(name.to_str()?)
                .args(["--key-slot", "0"])
                .arg(device.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?
                .success(),
            "self-check keyring token enrollment failed"
        );
        let current = status(device)?;
        let token = current["tokens"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["type"] == "luks2-keyring")
            .context("self-check keyring token missing")?["id"]
            .as_str()
            .context("self-check keyring token identity missing")?;
        ensure!(
            Command::new("cryptsetup")
                .args([
                    "open",
                    "--test-passphrase",
                    "--token-only",
                    "--token-id",
                    token
                ])
                .arg(device.path())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()?
                .success(),
            "self-check automatic token did not unlock"
        );
        ensure!(
            device
                .authenticate(secret_fd(b"ilex-deliberately-incorrect-passphrase")?)
                .is_err(),
            "automatic token bypassed credential authentication"
        );
        device.authenticate(pass.fd()?)?;
        Ok(())
    })();
    // SAFETY: revoke (3) and unlink (9) only the key created above, from the
    // newly created session. Never alter any preexisting key or keyring.
    let (revoked, unlinked) = unsafe {
        (
            libc::syscall(libc::SYS_keyctl, 3, key),
            libc::syscall(libc::SYS_keyctl, 9, key, session),
        )
    };
    result?;
    ensure!(
        revoked >= 0 && unlinked >= 0,
        "self-check keyring cleanup failed"
    );
    println!("ilex-enroll self-check: supplied FD required despite an available automatic token");
    Ok(())
}

fn inherit(command: &mut Command, key: &OwnedFd) {
    use std::os::unix::process::CommandExt;
    let fd = key.as_raw_fd();
    // SAFETY: async-signal-safe dup/fcntl; key remains alive through status().
    unsafe {
        command.pre_exec(move || {
            if libc::lseek(fd, 0, libc::SEEK_SET) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(fd, 3) < 0 || libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
