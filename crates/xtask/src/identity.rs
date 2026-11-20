//! Inspect automatic main ownership and explicitly release a missing checkout.
use anyhow::Result;
use ilex_devkit::main_claim::MainClaims;

#[derive(clap::Args, Debug)]
pub struct Args {
    #[command(subcommand)]
    pub command: Action,
}
#[derive(clap::Subcommand, Debug)]
pub enum Action {
    /// Show this checkout and the automatically registered primary
    Status,
    /// Release only a claim whose checkout no longer exists; resources stay untouched
    ReleaseMain {
        #[arg(long)]
        owner: String,
    },
    /// Rebuild an inspected unlabelled image; preserve the previous ID under a backup alias
    RebuildLegacyImage {
        #[arg(long)]
        expected_id: String,
        #[command(flatten)]
        build: crate::build::Args,
    },
}
pub fn run(args: &Args) -> Result<()> {
    match &args.command {
        Action::Status => {
            let ws = crate::checkout::workspace()?;
            let registry = MainClaims::system()?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"checkout": ws.identity()?, "registry": registry.path(), "main": registry.current()?})
                )?
            );
        }
        Action::ReleaseMain { owner } => {
            let claim = MainClaims::system()?.release_stale(owner)?;
            println!(
                "released main claim {} at {}; all images, VMs and state remain untouched",
                claim.owner, claim.checkout
            );
        }
        Action::RebuildLegacyImage { expected_id, build } => {
            let ws = crate::checkout::workspace()?;
            println!("{}", crate::build::rebuild_legacy(&ws, build, expected_id)?);
        }
    }
    Ok(())
}
