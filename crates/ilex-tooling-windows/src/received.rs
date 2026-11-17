//! Handle-bound Windows artifact I/O. Every directory remains open without
//! FILE_SHARE_DELETE, so neither it nor an ancestor can be exchanged for a
//! junction while a child is opened. Final files are opened without following
//! reparse points; existing hard links are refused before reading or writing.
use anyhow::{Context, Result, ensure};
use std::os::windows::{ffi::OsStrExt, fs::OpenOptionsExt, io::AsRawHandle};
use std::{
    fs::{self, File, OpenOptions},
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
        let stem = part.split('.').next().unwrap().to_ascii_lowercase();
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

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let path = std::path::absolute(path)?;
        let mut current = PathBuf::new();
        let mut guards = Vec::new();
        for component in path.components() {
            ensure!(
                matches!(component, Component::Prefix(p) if matches!(p.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)))
                    || matches!(component, Component::RootDir | Component::Normal(_)),
                "unsafe Windows output root"
            );
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            if !matches!(component, Component::RootDir) {
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => {
                        return Err(e).with_context(|| {
                            format!("creating artifact directory {}", current.display())
                        });
                    }
                }
            }
            let handle = OpenOptions::new()
                .read(true)
                .access_mode(FILE_READ_ATTRIBUTES | FILE_LIST_DIRECTORY)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&current)
                .with_context(|| format!("opening artifact directory {}", current.display()))?;
            check(&handle, true)?;
            guards.push(handle);
        }
        ensure!(!guards.is_empty(), "artifact root has no volume");
        Ok(Self {
            path,
            _guards: guards,
        })
    }
    pub fn child(&self, name: &str) -> Result<Self> {
        relative(name)?;
        Self::open(&self.path.join(name))
    }
    fn file(&self, name: &str, create: bool, append: bool) -> Result<BoundFile> {
        relative(name)?;
        let path = Path::new(name);
        let parent = Self::open(&self.path.join(path.parent().unwrap()))?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .share_mode(if create || append { 0 } else { FILE_SHARE_READ })
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        if create {
            options
                .write(true)
                .create_new(true)
                .access_mode(FILE_GENERIC_READ | FILE_GENERIC_WRITE | DELETE);
        } else if append {
            options.append(true).create(true);
        }
        let file = options.open(parent.path.join(path.file_name().unwrap()))?;
        check(&file, false)?;
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
    let name: Vec<u16> = file
        ._parent
        .path
        .join(name)
        .as_os_str()
        .encode_wide()
        .collect();
    let size = std::mem::offset_of!(FILE_RENAME_INFO, FileName) + name.len() * 2;
    let mut storage = vec![0u64; size.div_ceil(8)];
    let info = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        (*info).Anonymous.ReplaceIfExists = true;
        (*info).FileNameLength = (name.len() * 2) as u32;
        std::ptr::copy_nonoverlapping(name.as_ptr(), (*info).FileName.as_mut_ptr(), name.len());
        ensure!(
            SetFileInformationByHandle(
                file.file.as_raw_handle().cast(),
                FileRenameInfo,
                info.cast(),
                size as u32
            ) != 0,
            "publishing output by handle: {}",
            std::io::Error::last_os_error()
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
