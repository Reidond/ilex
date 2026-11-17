//! Read exact checkout bytes into a local content-addressed transfer cache.
use anyhow::{Context, Result, ensure};
use ilex_devkit::{
    Workspace,
    core::{CHUNK_BYTES, Snapshot, SourceEntryKind, digest},
    git,
};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub struct Staged {
    pub snapshot: Snapshot,
    pub cache: PathBuf,
    pub chunks: BTreeMap<String, PathBuf>,
    pub bytes: u64,
}
impl Staged {
    pub fn file(&mut self, path: &Path) -> Result<Vec<String>> {
        let mut file = fs::File::open(path)?;
        let mut chunks = Vec::new();
        loop {
            let mut bytes = vec![0; CHUNK_BYTES];
            let mut used = 0;
            while used < bytes.len() {
                let n = file.read(&mut bytes[used..])?;
                if n == 0 {
                    break;
                }
                used += n;
            }
            if used == 0 {
                break;
            }
            bytes.truncate(used);
            let key = digest(&bytes);
            let output = self.cache.join(&key[7..]);
            if !output.exists() {
                fs::write(&output, &bytes)?;
            }
            ensure!(
                digest(&fs::read(&output)?) == key,
                "local source cache corruption"
            );
            self.chunks.insert(key.clone(), output);
            chunks.push(key);
        }
        Ok(chunks)
    }
}

pub fn stage(ws: &Workspace) -> Result<Staged> {
    let (identity, source) = ws.source_identity()?;
    source.validate()?;
    let excluded = git::git(&ws.root)
        .args([
            "ls-tree",
            "-r",
            "--name-only",
            "-z",
            "HEAD",
            "--",
            ".claude",
            ".t3",
            "target",
            ".ilex-state",
        ])
        .output()?;
    ensure!(
        excluded.status.success() && excluded.stdout.is_empty(),
        "HEAD tracks excluded state/cache paths; refusing to include their contents in the source object pack"
    );
    let cache = ws.ensure_state()?.join("remote/chunks");
    fs::create_dir_all(&cache)?;
    let mut staged = Staged {
        snapshot: Snapshot {
            schema: 1,
            identity,
            source,
            files: BTreeMap::new(),
            git_pack: vec![],
        },
        cache,
        chunks: BTreeMap::new(),
        bytes: 0,
    };
    for entry in staged.snapshot.source.entries.clone() {
        if let SourceEntryKind::File { .. } = entry.kind {
            let path = ws.root.join(&entry.path);
            // Inventory already rejects escaping links/reparse points. Repeat the
            // inventory after copying to detect changes, then validate cached bytes.
            let content = fs::read(&path)?;
            ensure!(
                Some(digest(&content)) == entry.sha256 && content.len() as u64 == entry.bytes,
                "source changed during staging: {}",
                entry.path
            );
            check_lf(ws, &entry.path, &content)?;
            let chunks = staged.file(&path)?;
            let mut hash = sha2::Sha256::new();
            use sha2::Digest;
            for key in &chunks {
                hash.update(fs::read(&staged.chunks[key])?);
            }
            ensure!(
                entry.sha256.as_deref() == Some(format!("sha256:{:x}", hash.finalize()).as_str()),
                "source changed while copying {}",
                entry.path
            );
            staged.bytes += entry.bytes;
            staged.snapshot.files.insert(entry.path, chunks);
        }
    }
    let bundle = staged
        .cache
        .join(format!("head-{}.pack", staged.snapshot.source.revision));
    if !bundle.exists() {
        let temporary = bundle.with_extension(format!("{}.partial", std::process::id()));
        let objects = git::git(&ws.root)
            .args([
                "rev-list",
                "--objects",
                "--no-object-names",
                "--no-walk",
                "HEAD",
            ])
            .output()?;
        ensure!(objects.status.success(), "enumerating HEAD objects");
        let mut pack = git::git(&ws.root)
            .args(["pack-objects", "--stdout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::from(fs::File::create(&temporary)?))
            .stderr(Stdio::piped())
            .spawn()?;
        pack.stdin
            .take()
            .context("Git pack input")?
            .write_all(&objects.stdout)?;
        let result = pack.wait_with_output()?;
        ensure!(
            result.status.success(),
            "creating minimal source object pack: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        fs::rename(temporary, &bundle)?;
    }
    staged.snapshot.git_pack = staged.file(&bundle)?;
    let after = ilex_devkit::source::manifest(ws)?;
    ensure!(
        serde_json::to_vec(&staged.snapshot.source)? == serde_json::to_vec(&after)?,
        "source changed during snapshot; retry command"
    );
    Ok(staged)
}

fn check_lf(ws: &Workspace, path: &str, content: &[u8]) -> Result<()> {
    if !content.windows(2).any(|w| w == b"\r\n") {
        return Ok(());
    }
    let mut command: Command = git::git(&ws.root);
    let mut child = command
        .args(["check-attr", "-z", "--stdin", "text", "eol"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let mut input = child.stdin.take().context("git attributes stdin")?;
        input.write_all(path.as_bytes())?;
        input.write_all(b"\0")?;
    }
    let result = child.wait_with_output()?;
    ensure!(result.status.success(), "reading source attributes");
    let values: Vec<_> = result.stdout.split(|b| *b == 0).collect();
    let binary = values
        .chunks(3)
        .any(|v| v.len() == 3 && v[1] == b"text" && v[2] == b"unset");
    ensure!(
        binary
            || content.contains(&0)
            || !values
                .chunks(3)
                .any(|v| v.len() == 3 && v[1] == b"eol" && v[2] == b"lf"),
        "CRLF in LF source {path}; fix the checkout bytes before retrying (no bytes were rewritten)"
    );
    Ok(())
}
