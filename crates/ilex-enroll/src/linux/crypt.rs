//! The qualified libcryptsetup ABI. Load it only in the installed/installer
//! transaction path so the initramfs wrapper needs no new ELF dependencies.
use super::*;
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString, c_void},
    os::unix::ffi::OsStrExt,
    ptr,
};

struct Library(*mut c_void);

impl Library {
    fn open(path: &CStr) -> Result<Self> {
        // SAFETY: path is NUL terminated. The handle is owned until Drop.
        let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        ensure!(!handle.is_null(), "cannot load enrollment library");
        Ok(Self(handle))
    }

    unsafe fn symbol<T: Copy>(&self, name: &CStr) -> Result<T> {
        // SAFETY: callers specify the function's ABI and keep the library live.
        let symbol = unsafe { libc::dlsym(self.0, name.as_ptr()) };
        ensure!(!symbol.is_null(), "missing enrollment library symbol");
        Ok(unsafe { std::mem::transmute_copy(&symbol) })
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.0) };
    }
}

struct Context {
    raw: *mut c_void,
    free: unsafe extern "C" fn(*mut c_void),
    library: Library,
}

impl Context {
    fn open(path: &Path) -> Result<Self> {
        let library = Library::open(c"/usr/lib64/libcryptsetup.so.12")?;
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: the signatures are from libcryptsetup.h. The context stays
        // alive through any callback and is freed before unloading its library.
        unsafe {
            let init: unsafe extern "C" fn(*mut *mut c_void, *const libc::c_char) -> libc::c_int =
                library.symbol(c"crypt_init")?;
            let load: unsafe extern "C" fn(
                *mut c_void,
                *const libc::c_char,
                *mut c_void,
            ) -> libc::c_int = library.symbol(c"crypt_load")?;
            let log: unsafe extern "C" fn(
                *mut c_void,
                unsafe extern "C" fn(libc::c_int, *const libc::c_char, *mut c_void),
                *mut c_void,
            ) = library.symbol(c"crypt_set_log_callback")?;
            let free = library.symbol(c"crypt_free")?;
            let mut raw = ptr::null_mut();
            ensure!(
                init(&mut raw, path.as_ptr()) >= 0 && !raw.is_null(),
                "cannot open LUKS2 context"
            );
            let context = Self { raw, free, library };
            log(raw, ignore_log, ptr::null_mut());
            ensure!(
                load(raw, c"LUKS2".as_ptr(), ptr::null_mut()) >= 0,
                "cannot load LUKS2 context"
            );
            Ok(context)
        }
    }
}

unsafe extern "C" fn ignore_log(_: libc::c_int, _: *const libc::c_char, _: *mut c_void) {}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { (self.free)(self.raw) };
    }
}

/// Derive a transaction-local AES key from the authenticated volume key so
/// any retained fallback credential can resume it, even after a TPM reset.
pub(super) fn receipt_key(device: &Device, unlock: &Secret) -> Result<(u32, Secret)> {
    let context = Context::open(device.path())?;
    let mut volume = Secret::empty()?;
    let (slot, size) = {
        let buffer = volume.buffer(64)?;
        let mut size = buffer.len();
        // SAFETY: all pointers describe live memory and the ABI accepts an
        // arbitrary binary passphrase with its explicit length.
        let slot = unsafe {
            let get: unsafe extern "C" fn(
                *mut c_void,
                libc::c_int,
                *mut u8,
                *mut usize,
                *const u8,
                usize,
            ) -> libc::c_int = context.library.symbol(c"crypt_volume_key_get")?;
            get(
                context.raw,
                -1,
                buffer.as_mut_ptr(),
                &mut size,
                unlock.bytes().as_ptr(),
                unlock.bytes().len(),
            )
        };
        ensure!(
            slot >= 0 && (16..=64).contains(&size),
            "fallback credential did not unlock the LUKS2 payload"
        );
        (slot as u32, size)
    };
    let mut hash = Sha256::new();
    hash.update(b"ilex TPM transaction key v1\0");
    hash.update(device.uuid.as_bytes());
    hash.update(&volume.bytes()[..size]);
    let mut derived = hash.finalize();
    let key = Secret::copy(&derived);
    derived.zeroize();
    Ok((slot, key?))
}

pub(super) fn unseal(path: &Path, token: u32) -> Result<Secret> {
    let context = Context::open(path)?;
    let plugin = Library::open(c"/usr/lib64/cryptsetup/libcryptsetup-token-systemd-tpm2.so")?;
    // SAFETY: this is the installed cryptsetup token plugin ABI. The caller
    // has validated the token on our private scratch volume. The plugin owns
    // its returned buffer until its erase-and-free callback is called.
    unsafe {
        let open: unsafe extern "C" fn(
            *mut c_void,
            libc::c_int,
            *mut *mut u8,
            *mut usize,
            *mut c_void,
        ) -> libc::c_int = plugin.symbol(c"cryptsetup_token_open")?;
        let free: unsafe extern "C" fn(*mut u8, usize) =
            plugin.symbol(c"cryptsetup_token_buffer_free")?;
        let mut key = ptr::null_mut();
        let mut size = 0;
        ensure!(
            open(
                context.raw,
                token as libc::c_int,
                &mut key,
                &mut size,
                ptr::null_mut()
            ) == 0
                && !key.is_null(),
            "cannot unseal staged TPM credential"
        );
        let result = if (1..=1024).contains(&size) {
            Secret::copy(std::slice::from_raw_parts(key, size))
        } else {
            Err(anyhow::anyhow!("invalid staged TPM credential length"))
        };
        free(key, size);
        result
    }
}
