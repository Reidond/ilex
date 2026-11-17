//! Small bounded, locked buffers and sealed anonymous files. No persistent
//! secret files, command arguments, environment values, or debug formatting.
use anyhow::{Result, ensure};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::Command,
};
use zeroize::Zeroize;

pub struct Secret(Vec<u8>);
impl Secret {
    pub fn empty() -> Result<Self> {
        let mut bytes = vec![0u8; 65537];
        // SAFETY: the allocation remains live and fixed in size until Drop.
        ensure!(
            unsafe { libc::mlock(bytes.as_ptr().cast(), bytes.len()) } == 0,
            "cannot lock secret memory"
        );
        bytes.clear();
        Ok(Self(bytes))
    }
    pub fn read(fd: OwnedFd) -> Result<Self> {
        let mut secret = Self::empty()?;
        secret.0.resize(65537, 0);
        let mut file = File::from(fd);
        let mut length = 0;
        while length < 65537 {
            let n = file.read(&mut secret.0[length..])?;
            if n == 0 {
                break;
            }
            length += n;
        }
        ensure!(length > 0 && length <= 65536, "invalid secret size");
        secret.0.truncate(length);
        Ok(secret)
    }
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn push(&mut self, byte: u8) -> Result<()> {
        ensure!(self.0.len() < 65536, "secret too long");
        self.0.push(byte);
        Ok(())
    }
    pub fn trim_newline(&mut self) {
        while self.0.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            self.0.pop();
        }
    }
    pub fn fd(&self) -> Result<OwnedFd> {
        secret_fd(self.bytes())
    }
}
impl Drop for Secret {
    fn drop(&mut self) {
        let size = self.0.capacity();
        // All bytes, including a removed newline, are erased before unlocking.
        self.0.resize(size, 0);
        self.0.zeroize();
        // SAFETY: allocation has not moved or been freed.
        unsafe {
            libc::munlock(self.0.as_ptr().cast(), size);
        }
    }
}

pub fn harden() -> Result<()> {
    // SAFETY: process-local settings; do not alter the host or TPM.
    unsafe {
        ensure!(
            libc::prctl(libc::PR_SET_DUMPABLE, 0) == 0,
            "cannot disable dumps"
        );
        let limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        ensure!(
            libc::setrlimit(libc::RLIMIT_CORE, &limit) == 0,
            "cannot disable core files"
        );
    }
    Ok(())
}

pub fn secret_fd(bytes: &[u8]) -> Result<OwnedFd> {
    // SAFETY: constant NUL-terminated name; descriptor ownership is transferred once.
    let fd = unsafe {
        libc::memfd_create(
            c"ilex-credential".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    ensure!(fd >= 0, "cannot create credential memfd");
    let mut file = unsafe { File::from_raw_fd(fd) };
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    // SAFETY: fd is a live memfd; sealing prevents changing the credential.
    ensure!(
        unsafe {
            libc::fcntl(
                fd,
                libc::F_ADD_SEALS,
                libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
            )
        } == 0,
        "cannot seal credential"
    );
    Ok(file.into())
}

pub(super) fn key_command(command: &mut Command, key: &OwnedFd) {
    use std::os::unix::process::CommandExt;
    let fd = key.as_raw_fd();
    // SAFETY: only async-signal-safe descriptor operations run after fork. The
    // original OwnedFd remains live until Command::output completes.
    unsafe {
        command.pre_exec(move || {
            if libc::lseek(fd, 0, libc::SEEK_SET) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if fd != 3 && libc::dup2(fd, 3) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::fcntl(3, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub fn duplicate(fd: i32) -> Result<OwnedFd> {
    ensure!(fd >= 3, "credential descriptors must be >= 3");
    // SAFETY: fcntl validates fd and creates an independent owned descriptor.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    ensure!(copy >= 0, "invalid credential descriptor");
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}
