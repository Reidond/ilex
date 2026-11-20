use anyhow::Result;
use ilex_devkit::Workspace;
use std::path::PathBuf;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    operation: Operation,
}

#[derive(clap::Subcommand)]
enum Operation {
    /// Import a quiescent x86_64 Linux kache store from a standalone tar
    Kache {
        /// Exported tar with index.db and its companion files at the archive root
        archive: PathBuf,
        /// Source kache version (must match Containerfile)
        #[arg(long)]
        version: String,
    },
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let Args {
        operation: Operation::Kache { archive, version },
    } = args;
    #[cfg(target_os = "windows")]
    return crate::remote::import_kache(ws, archive, version);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (ws, archive, version);
        anyhow::bail!("cache import targets the native Windows builder")
    }
}
