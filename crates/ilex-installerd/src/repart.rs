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

/// The directory of one complete layout below the shipped definitions. Each
/// layout is a directory of its own, and repart is pointed at that directory:
/// there is no assembling from parts, and no directory repart reads ever
/// holds two root partitions.
pub fn layout_dir(shipped: &Path, encrypt: bool) -> PathBuf {
    shipped.join(if encrypt {
        "whole-disk-encrypted"
    } else {
        "whole-disk"
    })
}

/// Read a layout directory: its files, what each makes, and that it is a
/// whole layout (one root partition, encrypted exactly when asked).
pub fn read_layout(dir: &Path, encrypt: bool) -> Result<Layout> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".conf"))
        .collect();
    names.sort();
    let mut formats = Vec::new();
    let mut roots = Vec::new();
    for name in names {
        let text = fs::read_to_string(dir.join(&name))?;
        let encrypted = setting(&text, "Encrypt").is_some_and(|v| v != "off");
        if setting(&text, "Type").is_some_and(|t| t == "root" || t.starts_with("root-")) {
            roots.push(encrypted);
        }
        formats.push((
            name,
            setting(&text, "Format").unwrap_or_default(),
            encrypted,
        ));
    }
    ensure!(
        roots == [encrypt],
        "{} must hold exactly one root partition, {}encrypted (it has {} root partition(s))",
        dir.display(),
        if encrypt { "" } else { "un" },
        roots.len()
    );
    Ok(Layout {
        dir: dir.to_path_buf(),
        formats,
    })
}

/// The layout a plan uses.
pub fn layout(shipped: &Path, encrypt: bool) -> Result<Layout> {
    read_layout(&layout_dir(shipped, encrypt), encrypt)
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

/// Whether two lists describe the same partitions. The device node is left
/// out: repart names a partition after the path of the disk it was given, and
/// the plan was made on the kernel's name for the disk while the installation
/// writes it by its stable name. Everything that decides what is on the disk
/// (number, type, label, UUID, offset, size, format) must be equal.
pub fn same_partitions(made: &[PlannedPartition], planned: &[PlannedPartition]) -> bool {
    made.len() == planned.len()
        && made.iter().zip(planned).all(|(a, b)| {
            let mut a = a.clone();
            a.path.clone_from(&b.path);
            a == *b
        })
}
