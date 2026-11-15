//! `cargo xtask <cmd>`: build, boot, test and release the ilex image.
//!
//! Each subcommand lives in its own module with its own `Args`, so commands
//! can grow independently.

use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ilex_devkit::{Workspace, config, slots};

mod build;
mod changed;
mod disk;
mod iso;
mod publish;
mod rechunk;
mod registry;
mod select;
mod test;
mod tool;
mod vm;
mod wsl;

#[derive(Parser)]
#[command(name = "cargo xtask", about = "Build, boot, test and release the ilex image")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// podman build, tagged with this checkout's slug, labels included
    Build(build::Args),
    /// Split the image into reproducible chunks and add the `ilex.fingerprint` label
    Rechunk(rechunk::Args),
    /// Boot the current image as an interactive ephemeral VM
    Vm(vm::Args),
    /// Write a qcow2 or raw disk with `bcvk to-disk`
    Disk(disk::Args),
    /// Build if needed, then run the e2e harness (`cargo xtask test ephemeral::`)
    Test(test::Args),
    /// Compare the candidate's fingerprint with a published image
    Changed(changed::Args),
    /// Push by digest, tag and sign (CI only)
    Publish(publish::Args),
    /// Build the installer ISO for a published digest and push it (CI only)
    Iso(iso::Args),
    /// Build a drupe tool image from Fedora packages (`cargo xtask tool strace`)
    Tool(tool::Args),
    /// Show this checkout's slug, image, VM names and state directories
    Info,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cmd: Cmd) -> Result<()> {
    let ws = Workspace::discover()?;
    match cmd {
        Cmd::Build(args) => build::run(&ws, args),
        Cmd::Rechunk(args) => rechunk::run(&ws, args),
        // The VM stack lives in WSL2; --gui's window is ilex-viewer.exe.
        Cmd::Vm(args) => {
            // Reject invalid selection before building the native viewer or
            // handing over to WSL. --print-image only needs these local files.
            let resolved = args.resolve(&ws)?;
            if cfg!(windows) && !args.print_image {
                wsl::forward(&ws.root, args.gui)
            } else {
                vm::run(&ws, args, resolved)
            }
        }
        Cmd::Disk(args) => disk::run(&ws, args),
        Cmd::Test(args) => test::run(&ws, args),
        Cmd::Changed(args) => changed::run(&ws, args),
        Cmd::Publish(args) => publish::run(&ws, args),
        Cmd::Iso(args) => iso::run(&ws, args),
        Cmd::Tool(args) => tool::run(&ws, args),
        Cmd::Info => {
            let resolved = select::Args::default().resolve(&ws, None)?;
            println!("root:        {}", ws.root.display());
            println!("slug:        {}", ws.slug);
            println!("image:       {}", resolved.image);
            println!("vm prefix:   {}", ws.vm_prefix());
            println!("runtime dir: {}", ws.runtime_dir().display());
            println!("state dir:   {}", ws.state_dir().display());
            println!("vm slots:    {}", slots::max_slots());
            let selection = resolved.selection.context("reading image.toml for info; run `cargo xtask build` for manifest diagnostics")?;
            let summary = format!("selection:   {}", selection.summary());
            println!("{}", terminal_text(&summary));
            for (directory, file) in [
                ("shells", "shell.toml"),
                ("design-systems", "design-system.toml"),
                ("machines", "machine.toml"),
            ] {
                println!("{directory} available:");
                for name in config::available(&ws.root, directory)? {
                    match config::manifest(&ws.root.join(directory).join(&name).join(file)) {
                        Ok(manifest) => {
                            let description = manifest.get("description").and_then(|v| v.as_str()).unwrap_or("");
                            println!("  {}: {}", terminal_text(&name), terminal_text(description));
                        }
                        Err(error) => eprintln!("{}", terminal_text(&format!("  {name}: {error:#}"))),
                    }
                }
            }
            Ok(())
        }
    }
}

/// `info` can inspect an unbuilt manifest, before compose has validated it.
fn terminal_text(text: &str) -> String {
    text.chars().map(|ch| {
        if ch.is_control() { format!("\\u{{{:x}}}", ch as u32) } else { ch.to_string() }
    }).collect()
}
