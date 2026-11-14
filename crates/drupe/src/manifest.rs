//! `drupe.json`, the file at the root of every tool image:
//!
//! ```json
//! {
//!   "name": "strace",
//!   "version": "7.3-1.fc45",
//!   "commands": {
//!     "strace": {"exec": "bin/strace", "loader": "lib64/ld-linux-x86-64.so.2", "library-path": ["lib64"]},
//!     "strace-log-merge": {"exec": "bin/strace-log-merge"}
//!   }
//! }
//! ```
//!
//! Paths are relative to the image, which is `/app` when a command runs. A
//! command with a `loader` is started through that dynamic loader with
//! `library-path` as its only library directories, so it uses the libraries
//! the image brought and none of the host's; programs it starts are the
//! host's and load the host's libraries as usual, because none of this goes
//! through the environment. A command without one (a script, a static
//! binary, one built with an rpath into `/app`) is executed directly.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{Value, json};

use crate::paths::APP;

pub struct Manifest {
    pub name: String,
    pub version: String,
    pub commands: BTreeMap<String, Command>,
    /// The file as read, for the public copy in `/run/drupe/tools`.
    pub text: String,
}

pub struct Command {
    exec: String,
    loader: Option<String>,
    library_path: Vec<String>,
}

impl Manifest {
    pub fn read(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::parse(&text).map_err(|why| format!("{}: {why}", path.display()))
    }

    fn parse(text: &str) -> Result<Self, String> {
        let doc: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let name = string(&doc, "name")?;
        if !valid_name(&name) {
            return Err(format!("invalid tool name `{name}`"));
        }
        let version = string(&doc, "version")?;
        let entries = doc["commands"]
            .as_object()
            .filter(|c| !c.is_empty())
            .ok_or("`commands` must be a non-empty object")?;
        let mut commands = BTreeMap::new();
        for (cmd, entry) in entries {
            if !valid_name(cmd) || cmd == "drupe" {
                return Err(format!("invalid command name `{cmd}`"));
            }
            let exec = relative(&string(entry, "exec")?)?;
            let loader = match &entry["loader"] {
                Value::Null => None,
                Value::String(path) => Some(relative(path)?),
                _ => return Err(format!("`{cmd}`: `loader` must be a string")),
            };
            let library_path = match &entry["library-path"] {
                Value::Null => Vec::new(),
                Value::Array(dirs) => dirs
                    .iter()
                    .map(|d| d.as_str().ok_or("not a string".to_string()).and_then(relative))
                    .collect::<Result<_, _>>()
                    .map_err(|why| format!("`{cmd}`: `library-path`: {why}"))?,
                _ => return Err(format!("`{cmd}`: `library-path` must be an array")),
            };
            if loader.is_none() && !library_path.is_empty() {
                return Err(format!("`{cmd}`: `library-path` needs a `loader`"));
            }
            commands.insert(cmd.clone(), Command { exec, loader, library_path });
        }
        Ok(Self { name, version, commands, text: text.to_string() })
    }
}

impl Command {
    /// The `/run/drupe/cmd/<command>.json` entry: the tool to mount, the
    /// program to execute and the arguments that go before the caller's.
    pub fn entry(&self, tool: &str, cmd: &str) -> Value {
        let exec = format!("{APP}/{}", self.exec);
        match &self.loader {
            Some(loader) => {
                let loader = format!("{APP}/{loader}");
                let dirs: Vec<String> =
                    self.library_path.iter().map(|d| format!("{APP}/{d}")).collect();
                json!({
                    "tool": tool,
                    "program": loader,
                    "argv": [loader, "--library-path", dirs.join(":"), "--argv0", cmd, exec],
                })
            }
            None => json!({ "tool": tool, "program": exec, "argv": [cmd] }),
        }
    }
}

/// `[A-Za-z0-9][A-Za-z0-9._+-]*`, at most 64 bytes: safe as a file name, in
/// JSON and on a command line. The same rule `drupe-enter` applies.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(b))
}

fn string(doc: &Value, key: &str) -> Result<String, String> {
    doc[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("`{key}` must be a non-empty string"))
}

/// A path inside the image: relative, no `.`/`..`/empty components, no `:`
/// (it may end up in a library path).
fn relative(path: &str) -> Result<String, String> {
    let ok = !path.is_empty()
        && !path.contains([':', '\0'])
        && path.split('/').all(|c| !c.is_empty() && c != "." && c != "..");
    if ok {
        Ok(path.to_string())
    } else {
        Err(format!("`{path}` is not a plain relative path"))
    }
}
