//! Secrets: the administrator's password (and later the disk's passphrase)
//! reach the helper as a file descriptor of a sealed memfd, never as a message
//! string, are read once into locked memory and are wiped when dropped. The
//! helper runs with `PR_SET_DUMPABLE=0` and no core files (`ilex_enroll::harden`),
//! so the memory is not another process's to read.

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
use zeroize::Zeroize;

/// What a caller must have sealed: the contents cannot change under the
/// helper's feet, and nobody can lift the seals.
const REQUIRED_SEALS: i32 = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// A secret held in memory that is locked (`mlock`: never swapped out) and
/// zeroed when dropped. It does not implement `Debug`, `Clone` or `Display`.
pub struct Secret {
    bytes: Vec<u8>,
    locked: bool,
}

impl Secret {
    /// Take the descriptor of a sealed memfd and read it once, from the start.
    /// Refused, without saying what it held: anything that is not a memfd
    /// (a pipe, a regular file, a socket), a memfd that is not fully sealed
    /// against writing and resizing, one that is empty or over `max` bytes.
    pub fn from_memfd(fd: OwnedFd, max: usize) -> Result<Self> {
        let raw = fd.as_raw_fd();
        // F_GET_SEALS fails with EINVAL on anything that is not a sealable
        // file (a pipe, an ordinary file on most file systems).
        let seals = unsafe { libc::fcntl(raw, libc::F_GET_SEALS) };
        ensure!(
            seals >= 0,
            "the descriptor is not a memfd ({})",
            io::Error::last_os_error()
        );
        ensure!(
            seals & REQUIRED_SEALS == REQUIRED_SEALS,
            "the memfd is not sealed against writing and resizing (F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_GROW and F_SEAL_WRITE are all required)"
        );
        let file = File::from(fd);
        let size = file.metadata().context("looking at the memfd")?.len();
        ensure!(size > 0, "the secret is empty");
        ensure!(
            size <= max as u64,
            "the secret is larger than {max} bytes"
        );
        let size = size as usize;
        let bytes = vec![0u8; size];
        // Locked before anything is read into it; the vector never grows, so
        // the locked pages are the ones that hold the secret.
        let locked =
            unsafe { libc::mlock(bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) } == 0;
        let mut secret = Self { bytes, locked };
        let mut done = 0;
        while done < size {
            let read = unsafe {
                libc::pread(
                    file.as_raw_fd(),
                    secret.bytes[done..].as_mut_ptr().cast::<libc::c_void>(),
                    size - done,
                    done as libc::off_t,
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                bail!("reading the memfd: {error}");
            }
            ensure!(read > 0, "the memfd is shorter than it said");
            done += read as usize;
        }
        // Sealed, so it cannot have changed; this is what the descriptor held.
        Ok(secret)
    }

    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.bytes.zeroize();
        if self.locked {
            unsafe {
                libc::munlock(self.bytes.as_ptr().cast::<libc::c_void>(), self.bytes.len());
            }
        }
    }
}

/// A sealed memfd holding `content`, as an interface makes one for `SetSecret`:
/// created close-on-exec, written, then sealed against everything.
pub fn sealed_memfd(content: &[u8]) -> Result<OwnedFd> {
    let fd = unsafe {
        libc::memfd_create(
            c"ilex-secret".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut file = File::from(owned.try_clone()?);
    io::Write::write_all(&mut file, content).context("writing the memfd")?;
    let sealed = unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, REQUIRED_SEALS) };
    ensure!(sealed == 0, "sealing the memfd: {}", io::Error::last_os_error());
    Ok(owned)
}

/// The rules for the administrator's password: it goes to `chpasswd` as one
/// line, `name:password`, so it may not hold a line break or NUL.
pub fn admin_password(secret: &Secret) -> Result<&str> {
    let text = std::str::from_utf8(secret.expose()).context("the password is not UTF-8")?;
    ensure!(!text.is_empty(), "the password is empty");
    ensure!(
        !text.contains(['\n', '\r', '\0']),
        "the password may not contain a line break or a NUL character"
    );
    Ok(text)
}
