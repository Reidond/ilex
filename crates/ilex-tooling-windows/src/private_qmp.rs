//! Windows AF_UNIX: QEMU's reconnectable socket backend, protected at bind by
//! inherited NTFS ACLs. No network listener or post-listen permission repair.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    net::TcpStream,
    os::windows::{
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawSocket},
    },
    path::Path,
    sync::OnceLock,
};
use windows_sys::Win32::{
    Foundation::*,
    Networking::WinSock::*,
    Security::{Authorization::*, *},
    Storage::FileSystem::*,
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub struct Access {
    _directory: crate::received::Directory,
    _socket: File,
    pub report: Value,
}

unsafe fn sid_string(sid: PSID) -> Result<String> {
    unsafe {
        let mut text = std::ptr::null_mut();
        ensure!(
            ConvertSidToStringSidW(sid, &mut text) != 0,
            "reading QMP ACL SID"
        );
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let result = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
        LocalFree(text.cast());
        Ok(result)
    }
}
fn user_sid() -> Result<String> {
    unsafe {
        let mut token = std::ptr::null_mut();
        ensure!(
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) != 0,
            "reading current user token"
        );
        let result = (|| {
            let mut required = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut required);
            ensure!(required > 0 && required <= 4096, "user token size limit");
            let mut buffer = vec![0u64; (required as usize).div_ceil(8)];
            ensure!(
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    required,
                    &mut required
                ) != 0,
                "reading user SID"
            );
            sid_string((*(buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid)
        })();
        CloseHandle(token);
        result
    }
}

pub fn verify(endpoint: &str) -> Result<Access> {
    let name = endpoint
        .strip_prefix("unix:")
        .context("QMP requires a private Windows Unix socket; TCP is disabled")?;
    ensure!(
        !name.contains(['\0', '\n', '\r', '\\']) && name.len() < 108,
        "invalid QMP Unix socket path"
    );
    let path = Path::new(name);
    let parent = path.parent().context("QMP socket parent missing")?;
    let directory = crate::received::Directory::open(parent)?;
    let root = std::fs::canonicalize(crate::host::root()?.join("qmp"))?;
    ensure!(
        std::fs::canonicalize(parent)?.parent() == Some(root.as_path())
            && matches!(
                path.file_name().and_then(|s| s.to_str()),
                Some("control.sock" | "input.sock")
            ),
        "QMP endpoint escaped private state"
    );
    let socket = OpenOptions::new()
        .access_mode(READ_CONTROL | FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    unsafe {
        let mut tag: FILE_ATTRIBUTE_TAG_INFO = std::mem::zeroed();
        ensure!(
            GetFileInformationByHandleEx(
                socket.as_raw_handle().cast(),
                FileAttributeTagInfo,
                (&mut tag as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
                std::mem::size_of_val(&tag) as u32
            ) != 0
                && tag.ReparseTag == 0x80000023,
            "QMP endpoint is not a native AF_UNIX socket"
        );
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = GetSecurityInfo(
            socket.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        );
        ensure!(status == ERROR_SUCCESS, "reading QMP ACL: {status}");
        let result = (|| -> Result<Value> {
            ensure!(!dacl.is_null(), "QMP null DACL refused");
            let mut raw_sddl = std::ptr::null_mut();
            ensure!(
                ConvertSecurityDescriptorToStringSecurityDescriptorW(
                    descriptor,
                    1,
                    DACL_SECURITY_INFORMATION,
                    &mut raw_sddl,
                    std::ptr::null_mut(),
                ) != 0,
                "reading QMP DACL SDDL"
            );
            let mut length = 0;
            while *raw_sddl.add(length) != 0 {
                length += 1;
            }
            let dacl_sddl = String::from_utf16_lossy(std::slice::from_raw_parts(raw_sddl, length));
            LocalFree(raw_sddl.cast());
            let user = user_sid()?;
            let mut allowed = Vec::new();
            for index in 0..(*dacl).AceCount as u32 {
                let mut raw = std::ptr::null_mut();
                ensure!(GetAce(dacl, index, &mut raw) != 0, "reading QMP ACE");
                let ace = &*(raw.cast::<ACCESS_ALLOWED_ACE>());
                ensure!(
                    ace.Header.AceType == 0 // ACCESS_ALLOWED_ACE_TYPE
                        && u32::from(ace.Header.AceFlags) & INHERIT_ONLY_ACE == 0,
                    "unexpected QMP ACE"
                );
                let sid = sid_string((&ace.SidStart as *const u32).cast_mut().cast())?;
                ensure!(
                    (sid == user || sid == "S-1-5-18")
                        && ace.Mask & FILE_ALL_ACCESS == FILE_ALL_ACCESS,
                    "QMP ACL grants an unexpected principal or incomplete access"
                );
                allowed.push(sid);
            }
            allowed.sort();
            allowed.dedup();
            ensure!(
                allowed.len() == 2
                    && allowed.contains(&user)
                    && allowed.iter().any(|s| s == "S-1-5-18"),
                "QMP ACL must allow only current user and SYSTEM"
            );
            Ok(
                json!({"transport":"windows-af-unix","endpoint":endpoint,"current_user":user,"allowed_sids":allowed,"dacl_sddl":dacl_sddl,"ace_count":(*dacl).AceCount,"acl_verified":true}),
            )
        })();
        LocalFree(descriptor.cast());
        Ok(Access {
            _directory: directory,
            _socket: socket,
            report: result?,
        })
    }
}

pub fn connect(endpoint: &str) -> Result<(TcpStream, Access)> {
    let access = verify(endpoint)?;
    static WINSOCK: OnceLock<i32> = OnceLock::new();
    let startup = WINSOCK.get_or_init(|| unsafe {
        let mut data = std::mem::zeroed();
        WSAStartup(0x202, &mut data)
    });
    ensure!(*startup == 0, "Winsock initialization failed: {startup}");
    unsafe {
        let raw = socket(AF_UNIX as i32, SOCK_STREAM, 0);
        ensure!(
            raw != INVALID_SOCKET,
            "creating private QMP socket: {}",
            WSAGetLastError()
        );
        // TcpStream provides Winsock read/write/deadline ownership for this
        // AF_UNIX handle. The socket never uses the TCP/IP address family.
        let stream = TcpStream::from_raw_socket(raw as _);
        stream.set_nonblocking(true)?;
        let mut address: SOCKADDR_UN = std::mem::zeroed();
        address.sun_family = AF_UNIX;
        for (target, byte) in address.sun_path.iter_mut().zip(endpoint[5..].bytes()) {
            *target = byte as i8;
        }
        if connect_raw(raw, &address) == SOCKET_ERROR {
            let error = WSAGetLastError();
            ensure!(
                error == WSAEWOULDBLOCK,
                "connecting private QMP socket: {error}"
            );
            let mut poll = WSAPOLLFD {
                fd: raw,
                events: POLLWRNORM,
                revents: 0,
            };
            ensure!(
                WSAPoll(&mut poll, 1, 3000) > 0,
                "private QMP connect deadline expired"
            );
            if let Some(error) = stream.take_error()? {
                return Err(error.into());
            }
        }
        stream.set_nonblocking(false)?;
        Ok((stream, access))
    }
}
unsafe fn connect_raw(socket: SOCKET, address: &SOCKADDR_UN) -> i32 {
    unsafe {
        windows_sys::Win32::Networking::WinSock::connect(
            socket,
            (address as *const SOCKADDR_UN).cast(),
            std::mem::size_of_val(address) as i32,
        )
    }
}
