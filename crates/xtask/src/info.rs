//! Read-only checkout, selection and capability diagnostics.
use crate::select;
use anyhow::{Context, Result};
use ilex_devkit::{Workspace, config};
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Validated TSV: tag, kind, parent, machine, shells
    #[arg(long, conflicts_with_all = ["installer", "json", "capabilities"])]
    pub publications: bool,
    /// Print the installer's published composition tag
    #[arg(long, conflicts_with_all = ["json", "capabilities"])]
    pub installer: bool,
    /// Versioned identity, source digest, paths and capabilities
    #[arg(long)]
    pub json: bool,
    /// Include this host's VM capability report
    #[arg(long)]
    pub capabilities: bool,
}
pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    if args.json {
        let (identity, source) = ws.source_identity()?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": ilex_devkit::core::SCHEMA_VERSION, "identity": identity,
                "source": source, "image": ws.image_ref(), "runtime_dir": ws.runtime_dir(),
                "state_dir": ws.state_dir(), "backend": format!("{:?}", ilex_devkit::host_backend()),
                "host_arch": ilex_devkit::core::Architecture::host()?,
                "capabilities": capabilities(ws)?,
                "build_executor": executor_info(ws)?
            }))?
        );
        return Ok(());
    }
    if args.capabilities {
        println!("{}", serde_json::to_string_pretty(&capabilities(ws)?)?);
    }
    match args {
        Args {
            publications: true, ..
        } => {
            for row in config::publications(&ws.root)? {
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    row.tag,
                    row.kind,
                    row.parent.as_deref().unwrap_or("-"),
                    row.machine,
                    if row.shells.is_empty() {
                        "-".into()
                    } else {
                        row.shells.join(",")
                    }
                );
            }
            Ok(())
        }
        Args {
            installer: true, ..
        } => {
            println!("{}", config::installer(&ws.root)?);
            Ok(())
        }
        Args { .. } => {
            let identity = ws.identity()?;
            println!("owner:       {}", identity.owner);
            println!("checkout id: {}", identity.checkout_id);
            let publications = config::publications(&ws.root)?;
            let installer = config::installer(&ws.root)?;
            let resolved = select::Args::default().resolve(ws, None, None)?;
            println!("root:        {}", ws.root.display());
            println!("slug:        {}", ws.slug);
            println!("image:       {}", resolved.image);
            println!("vm prefix:   {}", ws.vm_prefix());
            println!("runtime dir: {}", ws.runtime_dir().display());
            println!("state dir:   {}", ws.state_dir().display());
            println!("executor:    {}", executor_info(ws)?);
            #[cfg(target_os = "windows")]
            println!("vm slots:    {}", ilex_tooling_windows::qemu::max_slots()?);
            #[cfg(not(target_os = "windows"))]
            println!("vm slots:    {}", ilex_devkit::slots::max_slots());
            let selection = resolved.selection.context(
                "reading image.toml for info; run `cargo ilexctl build` for manifest diagnostics",
            )?;
            let summary = format!("selection:   {}", selection.summary());
            println!("{}", terminal_text(&summary));
            println!("installer:   {installer}");
            let table = ilex_devkit::installer::table(&ws.root, None)?;
            println!("installer table:");
            for entry in &table.compositions {
                println!(
                    "  {}: machine={} shells={}",
                    entry.tag,
                    entry.machine,
                    entry.shells.join(",")
                );
            }
            for (name, machine) in &table.machines {
                for rule in &machine.matches {
                    println!("  {name} match: {rule:?}");
                }
            }
            println!("publications (tag, kind, parent, machine, shells):");
            for row in publications {
                println!(
                    "  {}\t{}\t{}\t{}\t{}",
                    row.tag,
                    row.kind,
                    row.parent.as_deref().unwrap_or("-"),
                    row.machine,
                    if row.shells.is_empty() {
                        "-".into()
                    } else {
                        row.shells.join(",")
                    }
                );
            }
            for (directory, file) in [
                ("shells", "shell.toml"),
                ("machines", "machine.toml"),
            ] {
                println!("{directory} available:");
                for name in config::available(&ws.root, directory)? {
                    match config::manifest(&ws.root.join(directory).join(&name).join(file)) {
                        Ok(manifest) => {
                            let description = manifest
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            println!("  {}: {}", terminal_text(&name), terminal_text(description));
                        }
                        Err(error) => {
                            eprintln!("{}", terminal_text(&format!("  {name}: {error:#}")))
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

pub(crate) fn capabilities(ws: &Workspace) -> Result<ilex_devkit::core::VmCapabilities> {
    #[cfg(target_os = "windows")]
    {
        let vms =
            ilex_tooling_windows::qemu::running_vms(&ws.runtime_dir(), Some(&ws.identity()?.owner));
        match vms.as_slice() {
            [vm] => Ok(vm.capabilities()),
            [] => Ok(ilex_tooling_windows::qemu::idle_capabilities()),
            _ => anyhow::bail!(
                "several native VMs are running; use cargo ilexctl vm --vm <purpose> capabilities"
            ),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ws;
        Ok(ilex_devkit::provider::capabilities("none", (1280, 800)))
    }
}

fn executor_info(ws: &Workspace) -> Result<serde_json::Value> {
    #[cfg(target_os = "windows")]
    {
        let status = ilex_tooling_windows::builder::run(
            ilex_tooling_windows::builder::Operation::Status,
            Default::default(),
        )?;
        let owner = ws.identity()?.owner;
        let last = ws.state_dir().join("remote/last-job");
        let result = std::fs::read_to_string(last)
            .ok()
            .and_then(|id| {
                std::fs::read(
                    ws.state_dir()
                        .join("remote/jobs")
                        .join(id)
                        .join("collection.json"),
                )
                .ok()
            })
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        Ok(
            serde_json::json!({"kind":"hyperv-builder","vm_id":status["vm_id"],"state":status["state"],
            "checkout_root":format!("/var/lib/ilex-data/checkouts/{owner}"),"last_result":result}),
        )
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = ws;
        Ok(serde_json::json!({"kind":"local-linux"}))
    }
}

/// `info` can inspect an unbuilt manifest, before compose has validated it.
fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() {
                format!("\\u{{{:x}}}", ch as u32)
            } else {
                ch.to_string()
            }
        })
        .collect()
}
