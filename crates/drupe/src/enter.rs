//! `drupe-enter`: run a program with one installed tool mounted at `/app`.
//!
//! ```text
//! drupe-enter <tool> <env-fd> <program> <argv0> [args...]
//! ```
//!
//! This is the only privileged part of drupe, installed setuid root at
//! `/usr/libexec/drupe/drupe-enter`. A tool has to see the machine exactly as
//! the user's shell does (their uid, every file, `sudo`, other processes to
//! trace), so a user namespace is out: there the tool could neither trace a
//! process outside it nor become root. What is left is root for the two
//! mount calls. As root this program:
//!
//! 1. gives itself a private mount namespace (mounts made on the host later
//!    still appear in it; nothing made here leaves it),
//! 2. binds `/run/drupe/priv/<tool>` onto `/app`, read-only, nosuid, nodev,
//!
//! and then drops to the calling user for good before it touches anything the
//! caller controls: the environment in `<env-fd>` and the program to run.
//!
//! `/run/drupe/priv` belongs to root and is closed to everyone else; only
//! `drupe activate` mounts there, and only images whose signature systemd
//! accepted. So all a caller chooses is which of the installed tools is at
//! `/app` for a process that is otherwise entirely their own. `<tool>` is a
//! single path component; nothing else from the caller reaches a privileged
//! call.
//!
//! The environment comes in a file descriptor (`KEY=VALUE` entries, each
//! ended by a NUL, as in `/proc/<pid>/environ`) because glibc removes
//! `LD_LIBRARY_PATH`, `TMPDIR` and other variables from a setuid program's
//! environment, and the tool should get the one its caller had. `-` instead
//! of a descriptor passes this process's (reduced) environment on.

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("drupe-enter: Linux only");
    std::process::exit(126);
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CString, OsString};
    use std::fs::File;
    use std::io::{self, Read};
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::ptr;

    /// Where `drupe activate` mounts the images; root only.
    const PRIV: &str = "/run/drupe/priv";
    /// Where the tool appears. An empty directory in the image.
    const APP: &str = "/app";
    /// Exit code for "could not start the tool", as shells use it.
    const FAILED: i32 = 126;
    /// More environment than `execve` accepts anyway.
    const ENV_MAX: u64 = 64 << 20;

    pub fn main() {
        let args: Vec<OsString> = std::env::args_os().collect();
        if args.len() < 5 {
            die("usage: drupe-enter <tool> <env-fd> <program> <argv0> [args...]");
        }
        let tool = args[1].to_str().filter(|t| valid_name(t)).unwrap_or_else(|| {
            die("the tool name must be one path component of [A-Za-z0-9._+-]")
        });
        let env_fd: Option<i32> = match args[2].to_str() {
            Some("-") => None,
            Some(fd) => Some(
                fd.parse()
                    .ok()
                    .filter(|fd| *fd > 2)
                    .unwrap_or_else(|| die("<env-fd> must be a descriptor above 2, or `-`")),
            ),
            None => die("<env-fd> must be a descriptor above 2, or `-`"),
        };
        if !args[3].as_bytes().starts_with(b"/") {
            die("<program> must be an absolute path");
        }
        let program = cstring(&args[3]);
        let argv: Vec<CString> = args[4..].iter().map(cstring).collect();

        enter(tool);
        drop_privileges();

        // From here on this is the caller's own process.
        let env: Vec<CString> = match env_fd {
            Some(fd) => read_env(fd).unwrap_or_else(|e| die(&format!("reading <env-fd>: {e}"))),
            None => std::env::vars_os()
                .map(|(k, v)| {
                    let mut entry = k.into_vec();
                    entry.push(b'=');
                    entry.extend(v.into_vec());
                    CString::new(entry).expect("no NUL in the environment")
                })
                .collect(),
        };
        let argv = pointers(&argv);
        let env = pointers(&env);
        // SAFETY: both arrays are NULL-terminated and point into CStrings
        // that outlive the call.
        unsafe { libc::execve(program.as_ptr(), argv.as_ptr(), env.as_ptr()) };
        die(&format!(
            "{}: {}",
            args[3].to_string_lossy(),
            io::Error::last_os_error()
        ));
    }

    /// A private mount namespace with the tool at `/app`.
    fn enter(tool: &str) {
        let source = CString::new(format!("{PRIV}/{tool}")).expect("validated name");
        let app = CString::new(APP).expect("constant");
        let root = CString::new("/").expect("constant");

        // SAFETY: plain syscalls on NUL-terminated strings that outlive them.
        unsafe {
            if libc::unshare(libc::CLONE_NEWNS) != 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EPERM) {
                    // The second case is `strace <a drupe command>` by a user:
                    // the kernel ignores the setuid bit of a traced program.
                    die("not permitted to create a mount namespace: drupe-enter must be setuid \
                         root, and is not while an unprivileged debugger traces it");
                }
                die(&format!("unshare: {e}"));
            }
            // Receive the host's later mounts, send it none of ours.
            if libc::mount(
                ptr::null(),
                root.as_ptr(),
                ptr::null(),
                libc::MS_REC | libc::MS_SLAVE,
                ptr::null(),
            ) != 0
            {
                die(&format!("making mounts private: {}", io::Error::last_os_error()));
            }
            if libc::mount(
                source.as_ptr(),
                app.as_ptr(),
                ptr::null(),
                libc::MS_BIND,
                ptr::null(),
            ) != 0
            {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ENOENT) {
                    die(&format!("tool `{tool}` is not installed (or {APP} is missing)"));
                }
                die(&format!("mounting `{tool}` at {APP}: {e}"));
            }
            // A bind mount takes its flags in a second step.
            if libc::mount(
                ptr::null(),
                app.as_ptr(),
                ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
                ptr::null(),
            ) != 0
            {
                die(&format!("restricting {APP}: {}", io::Error::last_os_error()));
            }
        }
    }

    /// Become the calling user in every id, and make sure it is final.
    fn drop_privileges() {
        // SAFETY: id syscalls without pointers.
        unsafe {
            let (uid, gid) = (libc::getuid(), libc::getgid());
            if libc::setresgid(gid, gid, gid) != 0 {
                die(&format!("setresgid: {}", io::Error::last_os_error()));
            }
            if libc::setresuid(uid, uid, uid) != 0 {
                die(&format!("setresuid: {}", io::Error::last_os_error()));
            }
            if uid != 0 && (libc::setuid(0) == 0 || libc::geteuid() == 0) {
                die("still root after dropping privileges");
            }
            if gid != 0 && uid != 0 && (libc::setgid(0) == 0 || libc::getegid() == 0) {
                die("still in group root after dropping privileges");
            }
        }
    }

    fn read_env(fd: i32) -> io::Result<Vec<CString>> {
        // SAFETY: the caller handed this descriptor over for us to read and
        // close; nothing else in this process uses it.
        let file = unsafe { File::from_raw_fd(fd) };
        let mut block = Vec::new();
        file.take(ENV_MAX).read_to_end(&mut block)?;
        Ok(block
            .split(|b| *b == 0)
            .filter(|entry| entry.iter().skip(1).any(|b| *b == b'='))
            .map(|entry| CString::new(entry).expect("split on NUL"))
            .collect())
    }

    /// `[A-Za-z0-9][A-Za-z0-9._+-]*`, at most 64 bytes: one path component
    /// that is neither `.` nor `..`.
    fn valid_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 64
            && bytes[0].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(b))
    }

    fn cstring(s: &OsString) -> CString {
        CString::new(s.as_bytes()).unwrap_or_else(|_| die("NUL in an argument"))
    }

    fn pointers(strings: &[CString]) -> Vec<*const libc::c_char> {
        strings
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(ptr::null()))
            .collect()
    }

    fn die(why: &str) -> ! {
        eprintln!("drupe-enter: {why}");
        std::process::exit(FAILED)
    }
}
