use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
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
pub fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
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
pub fn command_ok(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().context("starting helper")?;
    ensure!(status.success(), "helper failed ({status})");
    Ok(())
}
/// All PowerShell inputs are argument values or JSON files, never interpolated code.
pub fn powershell(script: &Path, request: &Path) -> Command {
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
    .arg(script)
    .arg("-Request")
    .arg(request);
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
