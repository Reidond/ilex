//! Secrets: the administrator's password (and later the disk's passphrase)
//! reach the helper as a file descriptor of a sealed memfd, never as a message
//! string, are read once into locked memory and are wiped when dropped. The
//! helper (and its answers mode) runs with `PR_SET_DUMPABLE=0` and no core files
//! (`ilex_enroll::harden`), so the memory is not another process's to read.

use anyhow::{Context, Result, bail, ensure};
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};
use zeroize::Zeroize;

/// What a caller must have sealed: the contents cannot change under the
/// helper's feet, and nobody can lift the seals.
const REQUIRED_SEALS: i32 =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

/// A secret held in memory of its own: an anonymous mapping that no other
/// allocation shares, locked (`mlock`: never swapped out), excluded from core
/// dumps, not inherited by a child after `fork`, and zeroed before it is
/// unmapped when dropped. Each secret has pages of its own, so unlocking one
/// never unlocks a page another live secret (or any other data) is on. It does
/// not implement `Debug`, `Clone` or `Display`.
pub struct Secret {
    /// The start of the mapping (page aligned).
    page: *mut u8,
    /// The secret's length in bytes.
    len: usize,
    /// The mapping's length: whole pages.
    map_len: usize,
    locked: bool,
}

// The mapping is owned by the value alone and is only read through `&self`.
unsafe impl Send for Secret {}
unsafe impl Sync for Secret {}

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
        ensure!(size <= max as u64, "the secret is larger than {max} bytes");
        let len = size as usize;
        let mut secret = Self::map(len)?;
        let bytes = secret.bytes_mut();
        let mut done = 0;
        while done < len {
            let read = unsafe {
                libc::pread(
                    file.as_raw_fd(),
                    bytes[done..].as_mut_ptr().cast::<libc::c_void>(),
                    len - done,
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

    /// A secret of these bytes, in pages of its own (the recovery key, which
    /// `systemd-cryptenroll` makes and the helper keeps until it is shown).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        ensure!(!bytes.is_empty(), "the secret is empty");
        let mut secret = Self::map(bytes.len())?;
        secret.bytes_mut().copy_from_slice(bytes);
        Ok(secret)
    }

    /// Whole pages of their own for `len` bytes, locked before anything is
    /// read into them. (A failed `mlock` is tolerated, as before: the live
    /// system has no swap but zram, and the helper runs as root.)
    fn map(len: usize) -> Result<Self> {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as usize;
        let map_len = len.div_ceil(page_size) * page_size;
        let page = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        ensure!(
            page != libc::MAP_FAILED,
            "mapping memory for a secret: {}",
            io::Error::last_os_error()
        );
        let page = page.cast::<u8>();
        let locked = unsafe { libc::mlock(page.cast::<libc::c_void>(), map_len) } == 0;
        // Not in a core dump, and zero (not a copy) in a child after fork.
        // Neither is essential: a kernel that lacks one still has the rest.
        unsafe {
            libc::madvise(page.cast::<libc::c_void>(), map_len, libc::MADV_DONTDUMP);
            libc::madvise(page.cast::<libc::c_void>(), map_len, libc::MADV_WIPEONFORK);
        }
        Ok(Self {
            page,
            len,
            map_len,
            locked,
        })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.page, self.len) }
    }

    pub fn expose(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.page, self.len) }
    }

    /// Whether `mlock` succeeded (the self-check asks).
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Where the secret's pages are (the self-check compares them with
    /// `/proc/self/smaps`).
    pub fn address(&self) -> usize {
        self.page as usize
    }

    /// How many bytes of memory the secret's pages cover.
    pub fn mapped_len(&self) -> usize {
        self.map_len
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // The length is not read after the wipe (a `Vec` that is zeroized is
        // empty, and an unlock of no bytes unlocks nothing): the whole mapping
        // is wiped, then unlocked, then returned.
        let (page, map_len, locked) = (self.page, self.map_len, self.locked);
        unsafe {
            std::slice::from_raw_parts_mut(page, map_len).zeroize();
            if locked {
                libc::munlock(page.cast::<libc::c_void>(), map_len);
            }
            libc::munmap(page.cast::<libc::c_void>(), map_len);
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
    ensure!(
        sealed == 0,
        "sealing the memfd: {}",
        io::Error::last_os_error()
    );
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

/// The rules for the disk's passphrase: the ones the model states for every
/// way a passphrase arrives (it is typed at a console prompt at every start of
/// the installed computer), and that it is text.
pub fn disk_passphrase(secret: &Secret) -> Result<&str> {
    let text = std::str::from_utf8(secret.expose()).context("the passphrase is not UTF-8")?;
    if let Some(why) = ilex_installer_model::plan::passphrase_problem(text) {
        bail!("{why}");
    }
    Ok(text)
}
