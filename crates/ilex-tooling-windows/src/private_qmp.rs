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

pub(crate) fn root() -> Result<std::path::PathBuf> {
    // Artifact isolation may override LOCALAPPDATA with a deep fixture path.
    // Socket names must fit sockaddr_un: anchor their ephemeral namespace in
    // the OS-known current user's directory, independent of that override.
    use windows::Win32::{System::Com::CoTaskMemFree, UI::Shell::*};
    unsafe {
        let folder = SHGetKnownFolderPath(&FOLDERID_LocalAppData, KF_FLAG_DEFAULT, None)?;
        let result = folder.to_string();
        CoTaskMemFree(Some(folder.0.cast()));
        Ok(std::path::PathBuf::from(result?).join("ilex/qmp"))
    }
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
    let root = std::fs::canonicalize(root()?)?;
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
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = GetSecurityInfo(
            socket.as_raw_handle().cast(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut descriptor,
        );
        ensure!(status == ERROR_SUCCESS, "reading QMP ACL: {status}");
        let result = (|| -> Result<Value> {
            ensure!(!dacl.is_null(), "QMP null DACL refused");
            let user = user_sid()?;
            ensure!(!owner.is_null(), "QMP owner missing");
            let owner = sid_string(owner)?;
            ensure!(owner == user, "QMP owner must be the current user");
            ensure!(
                (*dacl).AceCount == 2,
                "QMP DACL must contain exactly two ACEs"
            );
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
            let mut allowed = Vec::new();
            let mut entries = Vec::new();
            for index in 0..(*dacl).AceCount as u32 {
                let mut raw = std::ptr::null_mut();
                ensure!(GetAce(dacl, index, &mut raw) != 0, "reading QMP ACE");
                let header = &*(raw.cast::<ACE_HEADER>());
                ensure!(
                    header.AceType == 0 // ACCESS_ALLOWED_ACE_TYPE
                        && usize::from(header.AceSize) >= std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                        // ID records inherited provenance; it cannot propagate.
                        // Reject OI, CI, NP, IO and every other ACE flag.
                        && u32::from(header.AceFlags) & !INHERITED_ACE == 0,
                    "unexpected QMP ACE type, size or inheritable flags"
                );
                let ace = &*(raw.cast::<ACCESS_ALLOWED_ACE>());
                let sid = sid_string((&ace.SidStart as *const u32).cast_mut().cast())?;
                ensure!(
                    (sid == user || sid == "S-1-5-18") && ace.Mask == FILE_ALL_ACCESS,
                    "QMP ACL grants an unexpected principal or incomplete access"
                );
                entries.push(json!({"sid":sid,"type":"allow","mask":ace.Mask,"flags":header.AceFlags,"inherited":u32::from(header.AceFlags) & INHERITED_ACE != 0,"inheritable":false}));
                allowed.push(sid);
            }
            allowed.sort();
            ensure!(
                allowed.len() == 2
                    && allowed.contains(&user)
                    && allowed.iter().any(|s| s == "S-1-5-18"),
                "QMP ACL must allow only current user and SYSTEM"
            );
            Ok(
                json!({"transport":"windows-af-unix","endpoint":endpoint,"current_user":user,"owner_sid":owner,"owner_verified":true,"allowed_sids":allowed,"dacl_sddl":dacl_sddl,"ace_count":(*dacl).AceCount,"aces":entries,"no_inheritable_aces":true,"acl_verified":true}),
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

/// Real NTFS policy faults on sockets created by the transport e2e fixture.
/// The caller must stop its disposable QEMU first: no permissive test ACL is
/// ever applied to a listening endpoint. Restore the original DACL on errors.
pub(crate) fn policy_self_check(endpoint: &str) -> Result<Value> {
    let baseline = verify(endpoint)?;
    let user = baseline.report["current_user"]
        .as_str()
        .context("fixture user SID missing")?;
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL | WRITE_DAC)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(endpoint.strip_prefix("unix:").context("fixture endpoint")?)?;
    let mut cases = Vec::new();
    for (name, sid) in [
        ("Everyone", "WD"),
        ("Users", "BU"),
        ("AuthenticatedUsers", "AU"),
        ("Administrators", "BA"),
        ("Anonymous", "AN"),
    ] {
        cases.push((
            name.to_string(),
            format!("D:P(A;;FA;;;{user})(A;;FA;;;{sid})"),
        ));
    }
    cases.push((
        "duplicate-user".into(),
        format!("D:P(A;;FA;;;{user})(A;;FA;;;{user})"),
    ));
    cases.push((
        "extra-ACE".into(),
        format!("D:P(A;;FA;;;{user})(A;;FA;;;SY)(A;;FR;;;SY)"),
    ));
    cases.push((
        "incomplete-rights".into(),
        format!("D:P(A;;FA;;;{user})(A;;FR;;;SY)"),
    ));
    for flags in ["OI", "CI", "OINP", "OIIO"] {
        cases.push((
            format!("propagation-{flags}"),
            format!("D:P(A;;FA;;;{user})(A;{flags};FA;;;SY)"),
        ));
    }
    unsafe {
        let handle = file.as_raw_handle().cast();
        let mut original = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        ensure!(
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut original,
                std::ptr::null_mut(),
                &mut descriptor
            ) == ERROR_SUCCESS,
            "reading fixture DACL"
        );
        let result = (|| -> Result<Vec<Value>> {
            let mut observations = Vec::new();
            for (name, sddl) in cases {
                let wide: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
                let mut changed = std::ptr::null_mut();
                ensure!(
                    ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        wide.as_ptr(),
                        1,
                        &mut changed,
                        std::ptr::null_mut()
                    ) != 0,
                    "creating fixture descriptor"
                );
                let applied = (|| -> Result<()> {
                    let mut present = 0;
                    let mut defaulted = 0;
                    let mut dacl = std::ptr::null_mut();
                    ensure!(
                        GetSecurityDescriptorDacl(changed, &mut present, &mut dacl, &mut defaulted)
                            != 0
                            && present != 0
                            && !dacl.is_null(),
                        "reading changed fixture DACL"
                    );
                    ensure!(
                        SetSecurityInfo(
                            handle,
                            SE_FILE_OBJECT,
                            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            dacl,
                            std::ptr::null_mut()
                        ) == ERROR_SUCCESS,
                        "applying fixture DACL"
                    );
                    Ok(())
                })();
                LocalFree(changed.cast());
                applied?;
                match verify(endpoint) {
                    Err(error) => observations.push(json!({"case":name,"applied_sddl":sddl,"rejected":true,"error":format!("{error:#}")})),
                    Ok(access) => {
                        // Windows may strip propagation bits from a file's
                        // DACL. Record the actual safe readback, rather than
                        // claiming that a flag absent on disk was rejected.
                        ensure!(name.starts_with("propagation-")
                            && access.report["aces"].as_array().is_some_and(|aces| aces.iter().all(|ace| ace["flags"].as_u64().is_some_and(|flags| flags & 15 == 0))),
                            "accepted invalid fixture DACL {name}: {}", access.report);
                        observations.push(json!({"case":name,"applied_sddl":sddl,"rejected":false,"outcome":"OS removed propagation flags","readback":access.report}));
                    }
                }
            }
            Ok(observations)
        })();
        // The fixture started with an inherited DACL. Reapply inheritance from
        // its private parent, including when a negative case failed.
        let restored = SetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            original,
            std::ptr::null_mut(),
        );
        LocalFree(descriptor.cast());
        ensure!(
            restored == ERROR_SUCCESS,
            "restoring fixture DACL: {restored}"
        );
        let cases = result?;
        let rejected_count = cases.iter().filter(|case| case["rejected"] == true).count();
        let normalized_count = cases.len() - rejected_count;
        let restored = verify(endpoint)?.report;
        Ok(
            json!({"baseline":baseline.report,"cases":cases,"rejected_count":rejected_count,"normalized_count":normalized_count,"restored":restored}),
        )
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
