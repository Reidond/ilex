//! The installation's logs: what each tool printed, one file per stage under
//! `/run/ilex-installer/log/<plan id>/`, owner root only. Nothing is passed to
//! a tool where it could be echoed (a secret travels as a descriptor or on
//! standard input), so what is logged is stage names, command names, exit
//! codes and the tools' own output.

use crate::sys;
use anyhow::{Context, Result, ensure};
use ilex_installer_model::state::Stage;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

/// The directory of one installation's logs.
pub struct Logs {
    dir: PathBuf,
}

impl Logs {
    /// `<run_dir>/log/<plan id>`, created 0700. The plan id is 32 hex digits.
    pub fn new(run_dir: &Path, plan_id: &str) -> Result<Self> {
        ensure!(
            plan_id.len() == 32 && plan_id.bytes().all(|b| b.is_ascii_hexdigit()),
            "a plan id is 32 hexadecimal digits"
        );
        let parent = run_dir.join("log");
        for dir in [&parent, &parent.join(plan_id)] {
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
            }
        }
        Ok(Self {
            dir: parent.join(plan_id),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The log of one stage: `NN-<stage>.log`, appended to, mode 0600.
    pub fn stage(&self, stage: Stage) -> Result<StageLog> {
        let name = format!("{:02}-{}.log", stage as u8, stage.name());
        let path = self.dir.join(name);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(StageLog { file })
    }
}

pub struct StageLog {
    file: File,
}

impl StageLog {
    /// One timestamped line. A log that cannot be written is not worth
    /// failing an installation for: the journal has the stage names too.
    pub fn line(&mut self, text: &str) {
        let _ = writeln!(self.file, "[{}] {text}", sys::unix_time());
    }

    /// What a command is, before it runs: its name and arguments, which carry
    /// no secret by construction (one goes on standard input or in a descriptor).
    pub fn command(&mut self, program: &str, args: &[&str]) {
        self.line(&format!("$ {program} {}", args.join(" ")));
    }
}
