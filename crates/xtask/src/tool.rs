//! `cargo xtask tool <name> [packages...]`: build a drupe tool image from
//! Fedora packages (see `ilex_devkit::tool`).
//!
//! `cargo xtask tool strace` builds `<state dir>/tools/images/strace.raw`
//! from the package of the same name; `cargo xtask tool net nmap tcpdump`
//! puts the commands of two packages into one tool called `net`.

use anyhow::Result;
use ilex_devkit::{Workspace, tool};

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Name of the tool (and of its image)
    pub name: String,
    /// Fedora packages whose commands it provides (default: the tool's name)
    pub packages: Vec<String>,
}

pub fn run(ws: &Workspace, args: Args) -> Result<()> {
    let packages = if args.packages.is_empty() {
        vec![args.name.clone()]
    } else {
        args.packages
    };
    let built = tool::build(ws, &args.name, &packages)?;
    eprintln!(
        "built {}\ninstall it with `drupe install`, on a machine that has {} in /etc/verity.d/",
        built.image.display(),
        built.certificate.display()
    );
    println!("{}", built.image.display());
    Ok(())
}
