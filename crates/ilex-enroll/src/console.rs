//! Local console adapter. The library is unaware of Anaconda, terminals and
//! acknowledgement. Anaconda's %post only starts this adapter on a reserved VT.
use anyhow::{Context, Result, ensure};
use ilex_enroll::*;
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
        file.lock()?;
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
    let state_path = root.join("var/lib/ilex/security/enrollment.json");
    let mut state: Value = if state_path.exists() {
        serde_json::from_slice(&fs::read(&state_path)?)?
    } else {
        json!({"schema":1, "uuid":layout.uuid, "state":"pending", "recovery":null, "tpm":null})
    };
    let legacy_unbound = state.get("uuid") == Some(&Value::Null)
        && state["state"] == "unavailable"
        && state["reason"] == "unsupported-storage-layout"
        && state.get("recovery") == Some(&Value::Null)
        && state.get("tpm") == Some(&Value::Null);
    ensure!(
        state["schema"] == 1 && (state["uuid"] == layout.uuid || legacy_unbound),
        "enrollment state UUID/schema mismatch"
    );
    let current = status(&device)?;
    let recovery_exists = current["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .any(|token| token["type"] == "systemd-recovery");
    let acknowledged = acknowledged_recovery(&state);
    if acknowledged {
        let id = state["recovery"]["token"]
            .as_u64()
            .context("missing recovery identity")?
            .to_string();
        let slot = state["recovery"]["slot"]
            .as_u64()
            .context("missing recovery slot")?
            .to_string();
        ensure!(
            current["tokens"].as_array().unwrap().iter().any(|token| {
                token["id"] == id
                    && token["type"] == "systemd-recovery"
                    && token["keyslots"] == json!([slot])
            }),
            "acknowledged recovery slot missing"
        );
        if refused && !reissue {
            mark_refusal(root, &state_path, &mut state)?;
            eprintln!(
                "Installation can finish. TPM enrollment is unavailable for this storage layout. Use the saved recovery key or disk passphrase at boot."
            );
            return Ok(());
        }
    }
    let mut console = Console::open(tty)?;
    console.clear()?;
    console.say("ilex encrypted installation — local completion\r\n\r\nThe administrator account is for maintenance and recovery.\r\nThe disk passphrase remains a recovery option.\r\n\r\n")?;
    if refused {
        console.say("TPM enrollment is unavailable for this storage layout.\r\nInstallation will finish with local recovery.\r\n\r\n")?;
        if recovery_exists && !acknowledged {
            console.say("The previous recovery key was not confirmed. A replacement will be displayed for confirmation.\r\n\r\n")?;
        }
    }
    let passphrase = console.read("Re-enter the disk passphrase (input is hidden): ")?;
    device.authenticate(passphrase.fd()?)?;
    if legacy_unbound {
        ensure!(
            status(&device)? == current,
            "LUKS keyslots or tokens changed during discovery retry"
        );
        state["uuid"] = json!(layout.uuid);
        state["state"] = json!("pending");
        write_state(&state_path, &state)?;
    }
    if !refused {
        configure_crypttab(root, &layout.uuid)?;
    }
    if !acknowledged || reissue {
        ensure!(
            !recovery_exists || reissue || refused,
            "recovery was interrupted; rerun local completion with --reissue-recovery; existing slots and passphrase preserved"
        );
        state["state"] = json!("recovery-incomplete");
        if state["recovery"].is_object() {
            state["recovery"]["acknowledged"] = json!(false);
            state["recovery"]["displayed_and_acknowledged"] = json!(false);
        }
        write_state(&state_path, &state)?;
        let mut output = out_fd()?;
        let enrolled = recovery(&device, passphrase.fd()?, output.try_clone()?.into())?;
        state["recovery"] = json!({"token":enrolled.token, "slot":enrolled.slot, "acknowledged":false, "displayed_and_acknowledged":false});
        write_state(&state_path, &state)?;
        output.seek(SeekFrom::Start(0))?;
        let key = Secret::read(output.into())?;
        console.say("\r\nWrite this recovery key on paper or store it offline.\r\nIt is shown once, only on this local console. Do not photograph it.\r\n\r\n")?;
        console.file.write_all(key.bytes())?;
        console.say("\r\n\r\nPress Enter when it is saved offline.\r\n")?;
        drop(console.read("")?);
        console.clear()?;
        let confirmation =
            console.read("Enter the saved recovery key to confirm (input is hidden): ")?;
        ensure!(
            confirmation.bytes() == key.bytes(),
            "recovery verification failed; passphrase retained"
        );
        device.authenticate(confirmation.fd()?)?;
        state["recovery"]["acknowledged"] = json!(true);
        state["recovery"]["displayed_and_acknowledged"] = json!(true);
        write_state(&state_path, &state)?;
        // key/confirmation are erased on scope exit. Explicit reissue is
        // additive; retiring old recovery slots is an authenticated later task.
    }
    if refused {
        mark_refusal(root, &state_path, &mut state)?;
        return finish_console(
            &mut console,
            "TPM enrollment unavailable for this storage layout. Use the recovery key or disk passphrase.\r\n",
        );
    }
    let capability = probe();
    let target_tools = tool_versions(root).unwrap_or_default();
    let unlock_stack: Value = fs::read(root.join("usr/lib/ilex/security/unlock.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    let reason =
        if unlock_stack["schema"] != 1 || unlock_stack["method"] != "cryptsetup-token-only-v1" {
            "unlock-stack-missing"
        } else if target_tools != capability.tools {
            "tool-mismatch"
        } else {
            capability.reason
        };
    state["probe"] = serde_json::to_value(&capability)?;
    state["target_tools"] = serde_json::to_value(&target_tools)?;
    if capability.verdict == "eligible" && reason == "pcr7-sha256" {
        let enrolled = tpm(&device, passphrase.fd()?, &target_tools)?;
        state["tpm"] = serde_json::to_value(enrolled)?;
        state["state"] = json!("pending"); // PR19 verifies the next boot's evidence.
    } else {
        state["state"] = json!("unavailable");
    }
    state["reason"] = json!(reason);
    write_state(&state_path, &state)?;
    let enrolled = state["state"] == "pending";
    update_setup(
        root,
        capability.verdict,
        reason,
        if enrolled {
            "enrolled-unverified"
        } else {
            "passphrase-only"
        },
    )?;
    finish_console(
        &mut console,
        if enrolled {
            "TPM enrolled to PCR 7 (SHA-256). Unlock verification is pending.\r\n"
        } else {
            "TPM enrollment unavailable. This disk uses its passphrase.\r\n"
        },
    )
}

fn mark_refusal(root: &Path, state_path: &Path, state: &mut Value) -> Result<()> {
    ensure!(
        acknowledged_recovery(state),
        "recovery display is still incomplete"
    );
    state["state"] = json!("unavailable");
    state["reason"] = json!("unsupported-storage-layout");
    write_state(state_path, state)?;
    update_setup(
        root,
        "unavailable",
        "unsupported-storage-layout",
        "passphrase-only",
    )
}

fn acknowledged_recovery(state: &Value) -> bool {
    let recovery = &state["recovery"];
    // Earlier schema-1 writers set acknowledged only after confirmation.
    recovery["acknowledged"] == true
        && recovery
            .get("displayed_and_acknowledged")
            .is_none_or(|value| value == true)
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

fn configure_crypttab(root: &Path, uuid: &str) -> Result<()> {
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
            let path = Path::new("/dev/disk/by-uuid").join(String::from_utf8(uuid.stdout)?.trim());
            let device = Device::open(&path)?;
            let mut output = out_fd()?;
            let enrolled = recovery(&device, pass.fd()?, output.try_clone()?.into())?;
            output.seek(SeekFrom::Start(0))?;
            let recovery_key = Secret::read(output.into())?;
            device.authenticate(recovery_key.fd()?)?;
            device.authenticate(pass.fd()?)?;
            ensure!(enrolled.slot != 0, "recovery overwrote passphrase");
            let bad = secret_fd(b"ilex-deliberately-incorrect-passphrase")?;
            ensure!(
                device.authenticate(bad).is_err(),
                "incorrect credential accepted"
            );
            token_auth_self_check(&device, &pass)?;
            println!(
                "ilex-enroll self-check: LUKS2, private FD handoff, recovery and retained passphrase passed"
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
