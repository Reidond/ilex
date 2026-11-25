//! Whole-disk layouts with `systemd-repart`. The definitions ship in the
//! installer (`/usr/lib/ilex-installer/repart/whole-disk`), never in the image.
//! A plan is repart's own dry run of them: it writes nothing.

use crate::exec;
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::plan::PlannedPartition;
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

const TIMEOUT: Duration = Duration::from_secs(120);

/// The definition files of a layout, in repart's order.
pub struct Layout {
    pub dir: PathBuf,
    /// File name to the filesystem it makes (`Format=`), and whether the
    /// partition is encrypted (`Encrypt=`).
    pub formats: Vec<(String, String, bool)>,
}

fn setting(text: &str, key: &str) -> Option<String> {
    text.lines().map(str::trim).find_map(|line| {
        line.strip_prefix(key)?
            .strip_prefix('=')
            .map(|v| v.trim().to_owned())
    })
}

/// Assemble this plan's definitions in `scratch`: the shipped files, with the
/// encrypted root in place of the plain one when `encrypt`.
pub fn assemble(shipped: &Path, encrypt: bool, scratch: &Path) -> Result<Layout> {
    let source = shipped.join("whole-disk");
    let dir = scratch.join("repart.d");
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let root = if encrypt {
        "30-root-encrypted.conf"
    } else {
        "30-root.conf"
    };
    let skipped = if encrypt {
        "30-root.conf"
    } else {
        "30-root-encrypted.conf"
    };
    let mut names: Vec<String> = fs::read_dir(&source)
        .with_context(|| format!("reading {}", source.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".conf") && n != skipped)
        .collect();
    names.sort();
    ensure!(
        names.iter().any(|n| n == root),
        "{} is missing from {}",
        root,
        source.display()
    );
    let mut formats = Vec::new();
    for name in names {
        let text = fs::read_to_string(source.join(&name))?;
        // The encrypted variant is installed under the plain name: one root.
        let installed = if name == "30-root-encrypted.conf" {
            "30-root.conf".to_owned()
        } else {
            name.clone()
        };
        fs::write(dir.join(&installed), &text)?;
        formats.push((
            installed,
            setting(&text, "Format").unwrap_or_default(),
            setting(&text, "Encrypt").is_some_and(|v| v != "off"),
        ));
    }
    Ok(Layout { dir, formats })
}

/// Parse `systemd-repart --json=pretty` into planned partitions, numbered from 1.
pub fn parse(json: &str, layout: &Layout) -> Result<Vec<PlannedPartition>> {
    let rows: Value = serde_json::from_str(json).context("reading repart's plan")?;
    let rows = rows.as_array().context("repart's plan is not a list")?;
    let mut partitions = Vec::new();
    for row in rows {
        let text = |key: &str| {
            row[key]
                .as_str()
                .map(str::to_owned)
                .with_context(|| format!("repart row has no {key}"))
        };
        let number = |key: &str| {
            row[key]
                .as_u64()
                .with_context(|| format!("repart row has no {key}"))
        };
        let file = text("file")?;
        let file = file.rsplit('/').next().unwrap_or(&file).to_owned();
        let (filesystem, encrypted) = layout
            .formats
            .iter()
            .find(|(name, _, _)| *name == file)
            .map(|(_, format, encrypted)| (format.clone(), *encrypted))
            .with_context(|| format!("repart planned from an unknown definition {file}"))?;
        ensure!(
            text("activity")? == "create",
            "repart plans to {} a partition",
            text("activity")?
        );
        partitions.push(PlannedPartition {
            number: number("partno")? as u32 + 1,
            path: text("node")?,
            kind: text("type")?,
            label: text("label")?,
            uuid: text("uuid")?,
            start_bytes: number("offset")?,
            size_bytes: number("raw_size")?,
            filesystem,
            encrypted,
        });
    }
    ensure!(!partitions.is_empty(), "repart planned no partitions");
    partitions.sort_by_key(|p| p.number);
    Ok(partitions)
}

/// Dry-run the layout on `device`, erasing it on paper only.
pub fn dry_run(layout: &Layout, device: &str, seed: &str) -> Result<Vec<PlannedPartition>> {
    let args = [
        "--dry-run=yes",
        "--empty=force",
        "--pretty=no",
        "--no-pager",
        "--json=pretty",
        &format!("--definitions={}", layout.dir.display()),
        &format!("--seed={seed}"),
        device,
    ];
    let output = exec::run("systemd-repart", &args, TIMEOUT)?;
    if !output.success() {
        bail!(
            "systemd-repart: {}",
            output.stderr.lines().last().unwrap_or("failed")
        );
    }
    parse(&output.stdout_text(), layout)
}
