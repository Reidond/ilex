//! Layouts with `systemd-repart`. The definitions ship in the installer
//! (`/usr/lib/ilex-installer/repart/{whole-disk,free-space}[-encrypted]`), never
//! in the image. A plan is repart's own dry run of them: it writes nothing.
//!
//! Whole-disk mode makes a new table (`--empty=force`) holding the layout.
//! Free-space mode adds ilex's `/boot` and root to the table that is there
//! (`--empty=refuse`: repart is additive, and a table it does not recognise is
//! not touched), and what repart says about every partition that was already
//! there is checked against the probe: each one *unchanged*, at the same offset
//! and size, so that nothing of another system is grown, moved or relabelled.

use crate::exec;
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::{
    plan::{Mode, PlannedPartition},
    probe::Partition,
};
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
    /// Add to the disk's table beside what is there, instead of erasing it.
    pub free_space: bool,
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
pub fn layout_dir(shipped: &Path, mode: Mode, encrypt: bool) -> PathBuf {
    shipped.join(match (mode, encrypt) {
        (Mode::WholeDisk, false) => "whole-disk",
        (Mode::WholeDisk, true) => "whole-disk-encrypted",
        (Mode::FreeSpace, false) => "free-space",
        (Mode::FreeSpace, true) => "free-space-encrypted",
    })
}

/// Read a layout directory: its files, what each makes, and that it is a
/// whole layout (one root partition, encrypted exactly when asked; and in
/// free-space mode no ESP, which is the disk's own).
pub fn read_layout(dir: &Path, mode: Mode, encrypt: bool) -> Result<Layout> {
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
        ensure!(
            mode == Mode::WholeDisk || setting(&text, "Type").is_none_or(|t| t != "esp"),
            "{}: a free-space layout must not define an EFI system partition: the disk's own is shared",
            dir.join(&name).display()
        );
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
        free_space: mode == Mode::FreeSpace,
        formats,
    })
}

/// The layout a plan uses.
pub fn layout(shipped: &Path, mode: Mode, encrypt: bool) -> Result<Layout> {
    read_layout(&layout_dir(shipped, mode, encrypt), mode, encrypt)
}

/// Parse `systemd-repart --json=pretty` into planned partitions, numbered from 1.
///
/// In free-space mode `existing` is the partitions the probe found and the
/// output lists them too (`file` null, `unchanged`): each must be there
/// exactly once, where and as large as the probe says, and nothing else may be
/// listed but ilex's own new partitions. Whole-disk mode lists only new ones.
pub fn parse(
    json: &str,
    layout: &Layout,
    existing: &[(u64, u64)],
) -> Result<Vec<PlannedPartition>> {
    let rows: Value = serde_json::from_str(json).context("reading repart's plan")?;
    let rows = rows.as_array().context("repart's plan is not a list")?;
    let mut partitions = Vec::new();
    let mut seen_existing = 0;
    for row in rows {
        if layout.free_space && row["file"].is_null() {
            // A partition that was there: repart must leave it exactly alone.
            let offset = row["offset"].as_u64().context("repart row has no offset")?;
            let size = row["raw_size"]
                .as_u64()
                .context("repart row has no raw_size")?;
            ensure!(
                row["activity"].as_str() == Some("unchanged"),
                "repart plans to {} the partition at byte {offset}, which is not ilex's",
                row["activity"].as_str().unwrap_or("change")
            );
            ensure!(
                existing
                    .iter()
                    .any(|&(start, bytes)| start == offset && bytes == size),
                "repart lists a partition at byte {offset} ({size} bytes) that the probe did not find"
            );
            seen_existing += 1;
            continue;
        }
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
    if layout.free_space {
        ensure!(
            seen_existing == existing.len(),
            "repart lists {seen_existing} of the {} partitions that are on the disk",
            existing.len()
        );
        ensure!(
            partitions.len() == layout.formats.len(),
            "repart plans {} new partitions, not the {} of ilex's layout",
            partitions.len(),
            layout.formats.len()
        );
    }
    partitions.sort_by_key(|p| p.number);
    Ok(partitions)
}

/// Where each of these partitions starts and how large it is: what repart's
/// output must say of the partitions that were there.
pub fn spans(partitions: &[Partition]) -> Vec<(u64, u64)> {
    partitions
        .iter()
        .map(|p| (p.start_bytes, p.size_bytes))
        .collect()
}

/// What `--empty=` says: whole-disk mode creates a new table unconditionally,
/// free-space mode adds to the one that is there.
pub fn empty_mode(layout: &Layout) -> &'static str {
    if layout.free_space {
        "--empty=refuse"
    } else {
        "--empty=force"
    }
}

/// Dry-run the layout on `device`: on paper only, erasing it (whole-disk) or
/// adding to it (free-space; `existing` is what the probe found there).
pub fn dry_run(
    layout: &Layout,
    device: &str,
    seed: &str,
    existing: &[(u64, u64)],
) -> Result<Vec<PlannedPartition>> {
    let args = [
        "--dry-run=yes",
        empty_mode(layout),
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
    parse(&output.stdout_text(), layout, existing)
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
