//! LUKS2 for an encrypted installation: the passphrase's key file (a memfd the
//! tools read through `/proc/self/fd/N`, never an argument and never a file on
//! a disk), the volume's name, its header as the installed system's unlock
//! stack expects it, and the two things the installed system needs to find the
//! volume again: a line of `/etc/crypttab` and the kernel arguments.
//!
//! `systemd-repart` makes the volume (`Encrypt=key-file` in
//! `whole-disk-encrypted/30-root.conf`) with the passphrase as the key of its one
//! keyslot. Everything here is what is done with it afterwards, and none of it
//! needs a secret except [`KeyFile`].

use crate::secret::Secret;
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
};

/// The payload cipher `systemd-repart` and `cryptsetup` choose, and the one
/// the installed system's unlock stack (`ilex-enroll`) accepts.
pub const CIPHER: &str = "aes-xts-plain64";
/// The key derivation of the passphrase's keyslot.
pub const KDF: &str = "argon2id";
/// The payload key's size in bits: `aes-xts-plain64` takes two 256-bit keys.
pub const KEY_BITS: u64 = 512;

/// The device-mapper name of the opened volume, which is also what the
/// installed system's initramfs names it (`luks-<uuid>`): the unlock wrapper
/// of the image keeps its evidence under that name.
pub fn mapping_name(uuid: &str) -> String {
    format!("luks-{uuid}")
}

pub fn mapping_node(uuid: &str) -> String {
    format!("/dev/mapper/{}", mapping_name(uuid))
}

/// A LUKS UUID as `cryptsetup luksUUID` prints it: 36 characters of lower-case
/// hexadecimal and hyphens in the usual groups.
pub fn is_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// The line of the installed system's `/etc/crypttab` for the root volume:
/// the shape Anaconda writes (and `ilex-enroll`'s layout check requires): the
/// mapping name, the volume by UUID, no key file, and `discard` so that the
/// root can be trimmed.
pub fn crypttab_line(uuid: &str) -> String {
    format!("{} UUID={uuid} none discard\n", mapping_name(uuid))
}

/// The kernel arguments `bootc` is given so that the installed system can find
/// and unlock the root. `rd.luks.uuid` names the volume (the image's initramfs
/// is generic: it carries no crypttab and no list of volumes, so without this
/// it would never ask for the passphrase), and `vconsole.keymap` puts the
/// console in the layout the passphrase was typed in before the prompt appears,
/// for systemd's own `systemd-vconsole-setup` in the initramfs. (The image's
/// `rd.luks.options=tpm2-device=auto` comes with the image.)
pub fn kernel_args(uuid: &str, keymap: &str) -> Vec<String> {
    vec![
        format!("rd.luks.uuid={uuid}"),
        format!("vconsole.keymap={keymap}"),
    ]
}

/// What a LUKS2 header says, as far as the installation cares.
#[derive(Debug, PartialEq, Eq)]
pub struct Header {
    pub cipher: String,
    pub key_bits: u64,
    pub kdf: String,
    pub sector_size: u64,
}

/// Check `cryptsetup luksDump --dump-json-metadata` of the new volume: one
/// payload segment of `aes-xts-plain64` with a 512-bit key, one keyslot (the
/// passphrase's) derived with Argon2id, and no token. Anything else is not
/// what the installed system's unlock stack was qualified against.
pub fn check_header(json: &str) -> Result<Header> {
    let header: Value = serde_json::from_str(json).context("reading the LUKS2 header")?;
    let segments = header["segments"]
        .as_object()
        .context("the header has no segments")?;
    ensure!(
        segments.len() == 1,
        "the volume has {} data segments, not one",
        segments.len()
    );
    let segment = segments.values().next().context("no segment")?;
    ensure!(
        segment["type"] == "crypt" && segment["size"] == "dynamic",
        "the data segment is not a dynamic crypt segment"
    );
    let cipher = segment["encryption"]
        .as_str()
        .context("the segment has no cipher")?
        .to_owned();
    ensure!(
        cipher == CIPHER,
        "the volume's cipher is {cipher}, not {CIPHER}"
    );
    let slots = header["keyslots"]
        .as_object()
        .context("the header has no keyslots")?;
    ensure!(
        slots.len() == 1,
        "the volume has {} keyslots, not one",
        slots.len()
    );
    let slot = slots.values().next().context("no keyslot")?;
    ensure!(slot["type"] == "luks2", "the keyslot is not a LUKS2 one");
    let key_bits = slot["key_size"]
        .as_u64()
        .context("the keyslot has no size")?
        * 8;
    ensure!(
        key_bits == KEY_BITS,
        "the payload key is {key_bits} bits, not {KEY_BITS}"
    );
    let kdf = slot["kdf"]["type"]
        .as_str()
        .context("the keyslot has no key derivation")?
        .to_owned();
    ensure!(
        kdf == KDF,
        "the keyslot's key derivation is {kdf}, not {KDF}"
    );
    ensure!(
        header["tokens"].as_object().is_none_or(|t| t.is_empty()),
        "the new volume already has tokens"
    );
    Ok(Header {
        cipher,
        key_bits,
        kdf,
        sector_size: segment["sector_size"].as_u64().unwrap_or(512),
    })
}

/// The passphrase as a file the tools can read: a memfd holding its bytes and
/// nothing else, mode 0600, that a child inherits as the descriptor it names
/// (`--key-file=/proc/self/fd/N`). The bytes are the typed ones exactly: no
/// newline, because `systemd-repart` and `cryptsetup` both take the whole file
/// as the passphrase and the prompt at the next start does not type one. It is
/// overwritten with zeros and emptied when dropped.
pub struct KeyFile {
    fd: OwnedFd,
    len: usize,
}

impl KeyFile {
    pub fn new(passphrase: &Secret) -> Result<Self> {
        // SAFETY: a plain syscall with a constant name and flags.
        let raw = unsafe { libc::memfd_create(c"ilex-key".as_ptr(), libc::MFD_CLOEXEC) };
        ensure!(raw >= 0, "memfd_create: {}", io::Error::last_os_error());
        // SAFETY: `raw` is a descriptor this call just made.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // A memfd is made 0777; repart warns about a key file others can read.
        // SAFETY: a plain syscall on a descriptor this function owns.
        ensure!(
            unsafe { libc::fchmod(raw, 0o600) } == 0,
            "fchmod: {}",
            io::Error::last_os_error()
        );
        let bytes = passphrase.expose();
        let mut file = File::from(fd.try_clone()?);
        io::Write::write_all(&mut file, bytes).context("writing the key file")?;
        Ok(Self {
            fd,
            len: bytes.len(),
        })
    }

    pub fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The path a child that inherits the descriptor reads it by.
    pub fn path(&self) -> String {
        format!("/proc/self/fd/{}", self.raw())
    }
}

impl Drop for KeyFile {
    fn drop(&mut self) {
        let zeros = vec![0u8; self.len];
        let raw = self.fd.as_raw_fd();
        // SAFETY: writing and truncating a descriptor this value owns, with a
        // buffer of the stated length.
        unsafe {
            libc::pwrite(raw, zeros.as_ptr().cast::<libc::c_void>(), zeros.len(), 0);
            libc::ftruncate(raw, 0);
        }
    }
}

/// Make `fd` survive `exec` in a child: what `Command` needs between fork and
/// exec to give a tool the key file under its own number.
pub fn inheritable(fd: RawFd) -> io::Result<()> {
    // SAFETY: async-signal-safe `fcntl` calls on a descriptor the parent
    // holds open.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The UUID `cryptsetup luksUUID` printed.
pub fn parse_uuid(output: &[String]) -> Result<String> {
    let line = output
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_ascii_lowercase());
    match line {
        Some(uuid) if is_uuid(&uuid) => Ok(uuid),
        Some(other) => bail!("cryptsetup luksUUID printed {other:?}, not a UUID"),
        None => bail!("cryptsetup luksUUID printed nothing"),
    }
}
