use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};

pub fn root() -> Result<PathBuf> {
    ensure!(
        std::env::consts::ARCH == "x86_64",
        "Windows tooling currently supports x86_64 only"
    );
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is missing")?)
            .join("ilex"),
    )
}
pub fn available_memory() -> Result<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // All fields are integers; dwLength is required before the OS fills them.
    let mut memory: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    memory.dwLength = std::mem::size_of_val(&memory) as u32;
    ensure!(
        unsafe { GlobalMemoryStatusEx(&mut memory) } != 0,
        "reading available memory: {}",
        std::io::Error::last_os_error()
    );
    Ok(memory.ullAvailPhys)
}
pub fn hash_file(path: &Path) -> Result<String> {
    hash_reader(File::open(path)?)
}
pub fn hash_reader(mut f: impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        };
        digest.update(&buf[..n]);
    }
    Ok(format!("{:x}", digest.finalize()))
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn verify(path: &Path, expected: &str) -> Result<()> {
    ensure!(
        hash_file(path)? == expected,
        "SHA256 mismatch: {}; refusing extraction/use (remove the damaged archive to retry)",
        path.display()
    );
    Ok(())
}
pub fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = path.with_extension("partial");
    let mut f = File::create(&temp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(temp, path)?;
    Ok(())
}
pub fn lock(path: &Path) -> Result<File> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    fs2::FileExt::try_lock_exclusive(&f)
        .context("another ilex operation holds the installation lock")?;
    Ok(f)
}
pub fn try_lock(path: &Path) -> Result<Option<File>> {
    match lock(path) {
        Ok(file) => Ok(Some(file)),
        Err(error)
            if error.chain().any(|cause| {
                cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    io.kind() == std::io::ErrorKind::WouldBlock || io.raw_os_error() == Some(33)
                })
            }) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
pub fn command_ok(cmd: &mut Command) -> Result<()> {
    let (_job, mut child) = spawn_owned(cmd)?;
    let status = child.wait()?;
    ensure!(status.success(), "helper failed ({status})");
    Ok(())
}
/// Closing this handle (including abrupt parent termination) kills only this
/// job's helper descendants. Hyper-V VMs are external persistent resources.
pub struct ProcessJob(windows_sys::Win32::Foundation::HANDLE);
impl Drop for ProcessJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}
pub fn spawn_owned(command: &mut Command) -> Result<(ProcessJob, Child)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::*;
    // New unnamed, non-inheritable job. Zero is valid for every optional limit.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    ensure!(
        !raw.is_null(),
        "creating helper job: {}",
        std::io::Error::last_os_error()
    );
    let job = ProcessJob(raw);
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            raw,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&info) as u32,
        )
    };
    ensure!(
        configured != 0,
        "configuring helper job: {}",
        std::io::Error::last_os_error()
    );
    let mut child = command.spawn().context("starting owned helper")?;
    let assigned = unsafe { AssignProcessToJobObject(raw, child.as_raw_handle().cast()) };
    if assigned == 0 {
        let error = std::io::Error::last_os_error();
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("assigning helper process to owned job: {error}");
    }
    Ok((job, child))
}
pub fn output_owned(command: &mut Command, stderr: Stdio) -> Result<Output> {
    let (_job, child) = spawn_owned(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr),
    )?;
    Ok(child.wait_with_output()?)
}
/// All PowerShell inputs are argument values or JSON files, never interpolated code.
pub fn powershell(script: &Path, request: &Path) -> Command {
    let mut c = powershell_script(script);
    c.arg("-Request").arg(request);
    c
}

pub fn powershell_script(script: &Path) -> Command {
    let mut c = Command::new("powershell.exe");
    // A PowerShell 7 caller may export its module path. Windows PowerShell must
    // resolve its own inbox ACL/Hyper-V modules, rather than loading PS7 binaries.
    c.env_remove("PSModulePath");
    c.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(script);
    c
}
pub fn secure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    // Reject reparse points in every existing ancestor, including the final root.
    use std::os::windows::fs::MetadataExt;
    for ancestor in path.ancestors() {
        ensure!(
            fs::symlink_metadata(ancestor)?.file_attributes() & 0x400 == 0,
            "reparse point in tooling state path: {}",
            ancestor.display()
        );
    }
    let script = root()?.join(format!("protect-state-{}.ps1", std::process::id()));
    let request = root()?.join(format!("protect-state-{}.json", std::process::id()));
    // root itself is protected before any secret is generated. This script has no secrets.
    fs::create_dir_all(root()?)?;
    fs::write(&script, include_str!("../assets/protect-state.ps1"))?;
    fs::write(
        &request,
        serde_json::to_vec(&serde_json::json!({"path":path}))?,
    )?;
    let result = command_ok(&mut powershell(&script, &request));
    fs::remove_file(script)?;
    fs::remove_file(request)?;
    result
}
pub fn download(url: &str, path: &Path, sha: &str, bytes: u64) -> Result<()> {
    if path.exists() {
        return verify(path, sha);
    }
    let tmp = path.with_extension("download");
    // curl is included with supported Windows. HTTPS redirects only; never alternate versions.
    command_ok(
        Command::new("curl.exe")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--connect-timeout",
                "30",
                "--max-time",
                "1800",
                "--output",
            ])
            .arg(&tmp)
            .arg(url),
    )?;
    ensure!(
        fs::metadata(&tmp)?.len() == bytes,
        "download length mismatch for {url}"
    );
    verify(&tmp, sha)?;
    fs::rename(tmp, path)?;
    Ok(())
}
