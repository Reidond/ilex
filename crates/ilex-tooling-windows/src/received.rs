//! Windows artifact I/O rooted at verified directory handles. Every child is
//! opened by NtCreateFile with a single relative component and no-reparse flags.
//! Directory handles deny rename; in-place reparse changes cannot redirect an
//! already anchored operation. Existing links are rejected before data I/O.
use anyhow::{Context, Result, ensure};
use std::os::windows::{
    ffi::OsStrExt,
    fs::OpenOptionsExt,
    io::{AsRawHandle, FromRawHandle},
};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf, Prefix},
};
use windows_sys::Win32::Storage::FileSystem::*;

pub struct Directory {
    pub path: PathBuf,
    _guards: Vec<File>,
}
pub struct BoundFile {
    pub file: File,
    _parent: Directory,
}
#[repr(C)]
struct IoStatus {
    status_or_pointer: usize,
    information: usize,
}
#[repr(C)]
struct UnicodeName {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}
#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root: *mut std::ffi::c_void,
    name: *mut UnicodeName,
    attributes: u32,
    security: *mut std::ffi::c_void,
    quality_of_service: *mut std::ffi::c_void,
}
#[link(name = "ntdll")]
unsafe extern "system" {
    fn NtCreateFile(
        handle: *mut *mut std::ffi::c_void,
        access: u32,
        attributes: *mut ObjectAttributes,
        status: *mut IoStatus,
        allocation: *const i64,
        file_attributes: u32,
        share: u32,
        disposition: u32,
        options: u32,
        ea: *const std::ffi::c_void,
        ea_bytes: u32,
    ) -> i32;
    fn NtSetInformationFile(
        handle: *mut std::ffi::c_void,
        status: *mut IoStatus,
        information: *const std::ffi::c_void,
        length: u32,
        class: i32,
    ) -> i32;
    fn RtlNtStatusToDosError(status: i32) -> u32;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn DeviceIoControl(
        handle: *mut std::ffi::c_void,
        code: u32,
        input: *const std::ffi::c_void,
        input_bytes: u32,
        output: *mut std::ffi::c_void,
        output_bytes: u32,
        returned: *mut u32,
        overlapped: *mut std::ffi::c_void,
    ) -> i32;
}

pub fn relative(name: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && name.len() <= 512 && name.split('/').count() <= 16,
        "artifact path limit"
    );
    for part in name.split('/') {
        ensure!(
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 128
                && !part.ends_with([' ', '.'])
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b" ._-+".contains(&b)),
            "invalid Windows artifact name: {name:?}"
        );
        // Win32 also recognizes device stems with spaces before an extension.
        let stem = part
            .split('.')
            .next()
            .unwrap()
            .trim_end_matches(' ')
            .to_ascii_lowercase();
        ensure!(
            !matches!(stem.as_str(), "con" | "prn" | "aux" | "nul")
                && !(stem.len() == 4
                    && (stem.starts_with("com") || stem.starts_with("lpt"))
                    && stem.as_bytes()[3].is_ascii_digit()),
            "reserved Windows artifact name"
        );
    }
    Ok(())
}

fn check(file: &File, directory: bool) -> Result<()> {
    let mut tag: FILE_ATTRIBUTE_TAG_INFO = unsafe { std::mem::zeroed() };
    let mut standard: FILE_STANDARD_INFO = unsafe { std::mem::zeroed() };
    unsafe {
        ensure!(
            GetFileInformationByHandleEx(
                file.as_raw_handle().cast(),
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                std::mem::size_of_val(&tag) as u32
            ) != 0,
            "artifact attributes: {}",
            std::io::Error::last_os_error()
        );
        ensure!(
            GetFileInformationByHandleEx(
                file.as_raw_handle().cast(),
                FileStandardInfo,
                (&mut standard as *mut FILE_STANDARD_INFO).cast(),
                std::mem::size_of_val(&standard) as u32
            ) != 0,
            "artifact file information: {}",
            std::io::Error::last_os_error()
        );
    }
    ensure!(
        tag.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0,
        "artifact reparse point refused"
    );
    ensure!(
        standard.Directory == directory,
        "artifact file type mismatch"
    );
    ensure!(
        directory || standard.NumberOfLinks == 1,
        "artifact hard link refused"
    );
    Ok(())
}

// FILE_OPEN/CREATE/OPEN_IF and the native create-option/object-attribute bits.
const OPEN: u32 = 1;
const CREATE: u32 = 2;
const OPEN_IF: u32 = 3;

fn open_relative_raw(
    parent: &File,
    name: &std::ffi::OsStr,
    access: u32,
    share: u32,
    disposition: u32,
    directory: bool,
) -> Result<File> {
    let mut wide: Vec<u16> = name.encode_wide().collect();
    ensure!(
        !wide.is_empty()
            && wide.len() <= u16::MAX as usize / 2
            && !wide
                .iter()
                .any(|c| [0, b':' as u16, b'/' as u16, b'\\' as u16].contains(c)),
        "invalid relative native filename"
    );
    let mut unicode = UnicodeName {
        length: (wide.len() * 2) as u16,
        maximum_length: (wide.len() * 2) as u16,
        buffer: wide.as_mut_ptr(),
    };
    let mut attributes = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root: parent.as_raw_handle().cast(),
        name: &mut unicode,
        attributes: 0x40 | 0x1000, // CASE_INSENSITIVE | DONT_REPARSE
        security: std::ptr::null_mut(),
        quality_of_service: std::ptr::null_mut(),
    };
    let mut io = IoStatus {
        status_or_pointer: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            access | 0x0010_0000,
            &mut attributes,
            &mut io,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            share,
            disposition,
            0x0020_0000 | 0x20 | if directory { 1 } else { 0x40 }, // OPEN_REPARSE_POINT | SYNCHRONOUS_NONALERT | DIRECTORY/NON_DIRECTORY
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(std::io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(status) } as i32
        )
        .into());
    }
    ensure!(!handle.is_null(), "native open returned no handle");
    Ok(unsafe { File::from_raw_handle(handle.cast()) })
}
fn open_relative(
    parent: &File,
    name: &std::ffi::OsStr,
    access: u32,
    share: u32,
    disposition: u32,
    directory: bool,
) -> Result<File> {
    check(parent, true)?;
    let file = open_relative_raw(parent, name, access, share, disposition, directory)?;
    // A racing tag mutation can change path lookup, never this handle's root.
    // Still reject the mutated parent before returning a writable child.
    check(parent, true)?;
    check(&file, directory)?;
    Ok(file)
}

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let absolute = std::path::absolute(path)?;
        let mut parts = absolute.components();
        let prefix = parts.next().context("artifact root has no volume")?;
        ensure!(
            matches!(prefix, Component::Prefix(p) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
            "unsafe Windows output volume"
        );
        let root = parts.next().context("artifact root is not absolute")?;
        ensure!(
            matches!(root, Component::RootDir),
            "artifact root is not absolute"
        );
        let mut volume = PathBuf::from(prefix.as_os_str());
        volume.push(root.as_os_str());
        let handle = OpenOptions::new()
            .read(true)
            .access_mode(FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&volume)?;
        check(&handle, true)?;
        Self {
            path: volume,
            _guards: vec![handle],
        }
        .descend(parts.as_path())
    }
    fn descend(&self, path: &Path) -> Result<Self> {
        let mut result = Self {
            path: self.path.clone(),
            _guards: self
                ._guards
                .iter()
                .map(File::try_clone)
                .collect::<std::io::Result<Vec<_>>>()?,
        };
        check(result._guards.last().context("missing root handle")?, true)?;
        for part in path.components() {
            ensure!(
                matches!(part, Component::Normal(_)),
                "unsafe relative directory"
            );
            let name = part.as_os_str();
            let text = name.to_string_lossy();
            ensure!(
                !text.ends_with([' ', '.'])
                    && !text
                        .chars()
                        .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c)),
                "invalid Windows output directory"
            );
            let handle = open_relative(
                result._guards.last().unwrap(),
                name,
                FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                OPEN_IF,
                true,
            )
            .with_context(|| {
                format!(
                    "opening artifact directory {}",
                    result.path.join(name).display()
                )
            })?;
            result.path.push(name);
            result._guards.push(handle);
        }
        Ok(result)
    }
    pub fn child(&self, name: &str) -> Result<Self> {
        relative(name)?;
        self.descend(Path::new(name))
    }
    fn file(&self, name: &str, create: bool, append: bool) -> Result<BoundFile> {
        relative(name)?;
        let path = Path::new(name);
        let parent = self.descend(path.parent().unwrap())?;
        let access = if create {
            FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE
        } else if append {
            FILE_GENERIC_READ | (FILE_GENERIC_WRITE & !FILE_WRITE_DATA)
        } else {
            FILE_GENERIC_READ
        };
        let disposition = if create {
            CREATE
        } else if append {
            OPEN_IF
        } else {
            OPEN
        };
        let file = open_relative(
            parent._guards.last().unwrap(),
            path.file_name().unwrap(),
            access,
            if create || append { 0 } else { FILE_SHARE_READ },
            disposition,
            false,
        )?;
        Ok(BoundFile {
            file,
            _parent: parent,
        })
    }
    pub fn create(&self, name: &str) -> Result<BoundFile> {
        self.file(name, true, false)
    }
    pub fn read(&self, name: &str) -> Result<BoundFile> {
        self.file(name, false, false)
    }
    pub fn append(&self, name: &str) -> Result<BoundFile> {
        self.file(name, false, true)
    }
    pub fn write_new(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let mut output = self.create(name)?;
        output.file.write_all(bytes)?;
        output.file.sync_all()?;
        Ok(())
    }
}

/// Rename the opened file itself, not a re-resolved temporary filename.
pub fn rename(file: &BoundFile, name: &str) -> Result<()> {
    relative(name)?;
    ensure!(
        !name.contains('/'),
        "published filename must be a single component"
    );
    match file._parent.read(name) {
        Ok(_) => {}
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    let name: Vec<u16> = Path::new(name).as_os_str().encode_wide().collect();
    let size = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
    let mut storage = vec![0u64; size.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        let mut io = IoStatus {
            status_or_pointer: 0,
            information: 0,
        };
        // Native FileRenameInformation with a single leaf and NULL root
        // renames within this open file's existing directory, without a path
        // translation or reopening the parent for write access.
        let status = NtSetInformationFile(
            file.file.as_raw_handle().cast(),
            &mut io,
            info.cast(),
            size as u32,
            10,
        );
        ensure!(
            status >= 0,
            "publishing output by handle: {}",
            std::io::Error::from_raw_os_error(RtlNtStatusToDosError(status) as i32)
        );
    }
    Ok(())
}

pub fn sha256(file: &mut File) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

/// Real fault injection confined to the receiver's disposable self-check tree.
/// WRITE_ATTRIBUTES can set a junction even while directory sharing denies
/// generic writes; the child syscall itself must therefore be handle-relative.
pub fn self_check_reparse_race(held: &Directory, target: &Directory) -> Result<serde_json::Value> {
    let fixture = held
        .path
        .parent()
        .and_then(Path::parent)
        .context("self-check root missing")?;
    ensure!(
        held.path.file_name().is_some_and(|n| n == "held")
            && target.path.parent() == Some(fixture)
            && target.path.file_name().is_some_and(|n| n == "control")
            && fixture
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ilex-receiver-self-check-"))
            && std::fs::canonicalize(fixture.parent().context("self-check temp missing")?)?
                == std::fs::canonicalize(std::env::temp_dir())?,
        "reparse fault injection requires the disposable receiver self-check tree"
    );
    check(held._guards.last().unwrap(), true)?;
    let file = OpenOptions::new()
        .access_mode(FILE_WRITE_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(&held.path)?;
    let printable = target.path.to_string_lossy();
    let substitute = format!(
        r"\??\{}",
        printable.strip_prefix(r"\\?\").unwrap_or(&printable)
    );
    let substitute: Vec<u8> = substitute
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let printable: Vec<u8> = printable
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&0xa0000003u32.to_le_bytes());
    bytes.extend_from_slice(
        &u16::try_from(8 + substitute.len() + printable.len() + 4)?.to_le_bytes(),
    );
    bytes.extend_from_slice(&0u16.to_le_bytes());
    for value in [0, substitute.len(), substitute.len() + 2, printable.len()] {
        bytes.extend_from_slice(&u16::try_from(value)?.to_le_bytes());
    }
    bytes.extend_from_slice(&substitute);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&printable);
    bytes.extend_from_slice(&0u16.to_le_bytes());
    ioctl(&file, 0x0009_00a4, &bytes)?; // FSCTL_SET_REPARSE_POINT, mount-point tag
    let mut mutation = ReparseMutation { file, active: true };
    // Inject *after* the parent's normal attribute check and before the exact
    // syscall used in production. No second path lookup may follow this tag.
    let opened = open_relative_raw(
        held._guards.last().unwrap(),
        std::ffi::OsStr::new("race-probe.bin"),
        FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE,
        0,
        CREATE,
        false,
    );
    let opened_ok = opened.is_ok();
    drop(opened);
    let outside_absent = target.read("race-probe.bin").is_err_and(|error| {
        error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    });
    let parent_rejected = check(held._guards.last().unwrap(), true).is_err();
    mutation.clear()?;
    check(held._guards.last().unwrap(), true)?;
    let original = !opened_ok || held.read("race-probe.bin").is_ok();
    ensure!(
        outside_absent && parent_rejected && original,
        "in-place junction redirected native child creation"
    );
    Ok(
        serde_json::json!({"tag_mutation_succeeded":true,"raw_relative_open_succeeded":opened_ok,
        "outside_entry_absent":outside_absent,"parent_recheck_rejected":parent_rejected,"relative_open_confined":original,"tag_cleanup_verified":true}),
    )
}
fn ioctl(file: &File, code: u32, bytes: &[u8]) -> Result<()> {
    let mut returned = 0;
    ensure!(
        unsafe {
            DeviceIoControl(
                file.as_raw_handle().cast(),
                code,
                bytes.as_ptr().cast(),
                bytes.len() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        } != 0,
        "self-check reparse control: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}
struct ReparseMutation {
    file: File,
    active: bool,
}
impl ReparseMutation {
    fn clear(&mut self) -> Result<()> {
        let mut bytes = 0xa0000003u32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0u32.to_le_bytes());
        ioctl(&self.file, 0x0009_00ac, &bytes)?;
        self.active = false;
        Ok(())
    }
}
impl Drop for ReparseMutation {
    fn drop(&mut self) {
        if self.active {
            let _ = self.clear();
        }
    }
}
