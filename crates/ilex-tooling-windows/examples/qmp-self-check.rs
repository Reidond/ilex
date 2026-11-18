//! Real QEMU/NTFS-ACL e2e probe; not a unit test or a guest desktop profile.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|s| s == "--connect") {
        let endpoint = args
            .get(1)
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("--connect requires an endpoint"))?;
        let reply = ilex_tooling_windows::qmp::Qmp::connect(endpoint)?
            .execute("query-name", serde_json::json!({}))?;
        println!("{}", serde_json::json!({"connected":true,"reply":reply}));
        return Ok(());
    }
    anyhow::ensure!(
        args.len() == 2,
        "qmp-self-check <pinned-qemu.exe> <artifacts-dir>"
    );
    ilex_tooling_windows::qmp::transport_self_check(
        std::path::Path::new(&args[0]),
        std::path::Path::new(&args[1]),
    )
}
#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("this e2e transport probe requires Windows")
}
