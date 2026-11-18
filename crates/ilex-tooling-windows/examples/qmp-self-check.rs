//! Real QEMU/NTFS-ACL e2e probe; not a unit test or a guest desktop profile.
#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|s| s == "--connect") {
        let endpoint = args
            .get(1)
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("--connect requires an endpoint"))?;
        let command = args.get(2).and_then(|s| s.to_str()).unwrap_or("query-name");
        anyhow::ensure!(
            matches!(command, "query-name" | "query-status" | "stop" | "cont"),
            "unsupported e2e command"
        );
        let reply = ilex_tooling_windows::qmp::Qmp::connect(endpoint)?
            .execute(command, serde_json::json!({}))?;
        println!("{}", serde_json::json!({"connected":true,"reply":reply}));
        return Ok(());
    }
    if args.first().is_some_and(|s| s == "--hold") {
        anyhow::ensure!(args.len() == 3, "--hold <private-endpoint> <artifacts-dir>");
        let endpoint = args[1]
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("endpoint UTF-8"))?;
        let out = std::path::Path::new(&args[2]);
        std::fs::create_dir_all(out)?;
        let mut client = ilex_tooling_windows::qmp::Qmp::connect(endpoint)?;
        let before = client.execute("query-name", serde_json::json!({}))?;
        std::fs::write(out.join("ready.json"), serde_json::to_vec(&before)?)?;
        let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while !out.join("continue").exists() {
            anyhow::ensure!(
                std::time::Instant::now() < until,
                "held connection probe timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let after = client.execute("query-name", serde_json::json!({}))?;
        anyhow::ensure!(before == after, "held QMP identity changed");
        std::fs::write(
            out.join("result.json"),
            serde_json::to_vec(
                &serde_json::json!({"before":before,"after":after,"same_connection_usable":true}),
            )?,
        )?;
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
