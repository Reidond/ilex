//! The target system while it is being installed: finding what `bootc` made
//! (the one deployment below the stateroot), the files the helper adds to it,
//! the mounts it must end with none of, and the partitions repart made, found
//! again by their UUIDs and checked against sysfs before anything mounts them.

use crate::topology::{self, Mount};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::{
    catalogue::PROFILE_SCHEMA,
    plan::{DailyUser, Plan, PlanRequest, PlannedPartition},
    recovery::TpmOutcome,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

/// The stateroot `bootc install` uses unless told otherwise.
pub const STATEROOT: &str = "default";

/// The file an installation leaves on the new `/boot` (the partition labelled
/// `ilex-boot`): the digest of the request it was made from
/// ([`request_digest`]). Unattended mode reads it, read-only, to refuse
/// answers that already installed this disk (a machine that powered off after
/// an installation and boots the installer again, with the same stick in it,
/// would otherwise erase its own fresh installation, every boot).
pub const REQUEST_STAMP: &str = "ilex-install-request.sha256";

/// The partition label of the new `/boot`.
pub const BOOT_LABEL: &str = "ilex-boot";

/// What identifies an installation's request: SHA-256 over the request as the
/// plan records it (every default resolved), serialised in the model's own
/// field order. It holds nothing secret and is not derived from a secret: the
/// administrator's password is not part of a request (it travels by
/// `SetSecret`), so this digest cannot serve to guess it. (A digest of the
/// whole answers file, which holds the password in plaintext, would be a fast
/// hash of a short secret that anyone able to read `/boot` could try to
/// guess.) The consequence: two answers that differ only in the password are
/// the same request.
pub fn request_digest(request: &PlanRequest) -> String {
    let text = serde_json::to_string(&request.normalized()).unwrap_or_default();
    let mut hash = Sha256::new();
    hash.update(b"ilex-install-request-v1\n");
    hash.update(text.as_bytes());
    format!("{:x}", hash.finalize())
}

/// What `bootc install to-filesystem` deployed.
#[derive(Debug, PartialEq, Eq)]
pub struct Deployment {
    /// `<target>/ostree/deploy/<stateroot>/deploy/<checksum>.<serial>`: the
    /// root of the system as the installed machine will see it.
    pub path: PathBuf,
    /// The ostree commit checksum: 64 hexadecimal digits.
    pub checksum: String,
    /// `<target>/ostree/deploy/<stateroot>/var`: what `/var` will be.
    pub var: PathBuf,
    /// The deployment's origin file, `container-image-reference=...` and more.
    pub origin: String,
}

fn is_checksum(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The entries of a directory that are real directories (never a link), by name.
fn directories(dir: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// The single deployment of the target. Exactly one stateroot, `default`, with
/// exactly one deployment `<64 hex digits>.<serial>` that has its origin file:
/// anything else is not what a fresh `bootc install` makes, and files are not
/// written into a tree that might be somebody else's.
pub fn find_deployment(target: &Path) -> Result<Deployment> {
    let roots = directories(&target.join("ostree/deploy"))?;
    ensure!(
        roots == [STATEROOT],
        "expected the one stateroot {STATEROOT}, found {roots:?}"
    );
    let root = target.join("ostree/deploy").join(STATEROOT);
    let deploy = root.join("deploy");
    let found: Vec<String> = directories(&deploy)?
        .into_iter()
        .filter(|name| {
            name.split_once('.').is_some_and(|(checksum, serial)| {
                is_checksum(checksum)
                    && !serial.is_empty()
                    && serial.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .collect();
    ensure!(
        found.len() == 1,
        "expected one deployment below {}, found {}",
        deploy.display(),
        found.len()
    );
    let name = &found[0];
    let origin = fs::read_to_string(deploy.join(format!("{name}.origin")))
        .with_context(|| format!("reading the origin of {name}"))?;
    let var = root.join("var");
    ensure!(
        fs::symlink_metadata(&var)?.is_dir(),
        "{} is not a directory",
        var.display()
    );
    Ok(Deployment {
        path: deploy.join(name),
        checksum: name.split('.').next().unwrap_or_default().to_owned(),
        var,
        origin,
    })
}

/// `container-image-reference=` of an origin file.
pub fn origin_reference(origin: &str) -> Option<&str> {
    origin
        .lines()
        .find_map(|line| line.strip_prefix("container-image-reference="))
}

/// What the origin must name for the installed system to verify its updates:
/// `ostree-image-signed:docker://<target>` (bootc's spelling of the container
/// signature policy being enforced).
pub fn expected_origin(target_reference: &str) -> String {
    format!("ostree-image-signed:docker://{target_reference}")
}

/// The `profile` of `/etc/ilex/setup.json`, as the Anaconda chooser records
/// each choice: a public profile is its id and schema, a custom daily user has
/// both null and `"mode": "custom"` (its name and capacity are given to
/// `ilexctl home setup` as options, see [`home_setup_issue`]), and no daily
/// user is both null with no mode, which asks for no setup.
fn setup_profile(plan: &Plan) -> serde_json::Value {
    match &plan.request.daily {
        Some(DailyUser::Profile { id }) => json!({ "id": id, "schema": PROFILE_SCHEMA }),
        Some(DailyUser::Custom { .. }) => json!({ "id": null, "schema": null, "mode": "custom" }),
        None => json!({ "id": null, "schema": null }),
    }
}

/// Where the installed system's login prompt is told what to run next.
pub const HOME_SETUP_ISSUE: &str = "etc/issue.d/90-ilex-home-setup.issue";

/// The line `ilexctl home setup` removes when it is done, for a plan with a
/// daily user (the Anaconda path writes the same sentence for both kinds). A
/// custom user's name and capacity are on the command: `setup.json` carries
/// neither, as it never did.
pub fn home_setup_issue(plan: &Plan) -> Option<String> {
    let options = match plan.request.daily.as_ref()? {
        DailyUser::Profile { .. } => String::new(),
        DailyUser::Custom {
            name,
            disk_size_gib,
        } => {
            let size = disk_size_gib.map_or_else(String::new, |g| format!(" --disk-size {g}"));
            format!(" --name {name}{size}")
        }
    };
    Some(format!(
        "Sign in as the maintenance administrator and run sudo ilexctl home setup{options}.\n"
    ))
}

/// `/etc/ilex/setup.json` as the Anaconda path writes it (schema 1, the same
/// keys, sorted, two-space indent): the composition, the image the machine
/// tracks, the daily-user choice ([`setup_profile`]), and the TPM record. An unencrypted installation says
/// nothing was configured. An encrypted one says what phase 0's own completion
/// says of a disk that unlocks with its passphrase alone: `passphrase-only`,
/// with what was asked for, and a reason. The configure stage writes it before
/// anything is known of the security chip (`not-attempted`); the stage after
/// the account writes it again with what the installation did ([`TpmOutcome`])
/// and the computer's verdict.
pub fn setup_record(plan: &Plan, tpm_outcome: Option<(&str, &TpmOutcome)>) -> String {
    let tpm = match (plan.encryption.enabled, tpm_outcome) {
        // What the installation did, as `ilex-enroll`'s completion records it:
        // the computer's verdict, the reason code, and enrolled or not.
        (true, Some((capability, outcome))) => json!({
            "requested": plan.request.tpm_mode(),
            "capability": capability,
            "reason": outcome.reason,
            "status": outcome.status,
        }),
        (true, None) => json!({
            "requested": plan.request.tpm_mode(),
            "capability": "unknown",
            "reason": "not-attempted",
            "status": "passphrase-only",
        }),
        (false, _) => json!({
            "requested": "unspecified",
            "capability": "unknown",
            "status": "not-configured",
        }),
    };
    let record = json!({
        "schema": 1,
        "composition": plan.composition.tag,
        "image": plan.image.target,
        "profile": setup_profile(plan),
        "security": {
            "capabilities": {},
            "tpm": tpm,
        },
    });
    let mut text = serde_json::to_string_pretty(&record).unwrap_or_default();
    text.push('\n');
    text
}

/// An `/etc/fstab` without the lines for `/`: an ostree system's root is an
/// overlay the initramfs mounts, and a line for it makes
/// `systemd-remount-fs.service` fail and the machine boot degraded. Returns the
/// text and how many lines were dropped.
pub fn strip_root_fstab(text: &str) -> (String, usize) {
    let mut dropped = 0;
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let mut fields = line.split_whitespace();
            let root = fields.next().is_some_and(|first| !first.starts_with('#'))
                && fields.next() == Some("/");
            dropped += usize::from(root);
            !root
        })
        .collect();
    let mut out = kept.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    (out, dropped)
}

/// The unit the deployment's default target is: the name `default.target`
/// links to, from `etc/systemd/system/` (an administrator's choice) or else
/// the vendor's `usr/lib/systemd/system/` (`graphical.target`).
pub fn default_target(deployment: &Path) -> Result<String> {
    let mut link = deployment.join("etc/systemd/system/default.target");
    if fs::symlink_metadata(&link).is_err() {
        link = deployment.join("usr/lib/systemd/system/default.target");
    }
    let to = fs::read_link(&link).with_context(|| format!("reading {}", link.display()))?;
    to.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("default.target links to nothing")
}

/// Mount points that are, or hold, the target's file systems: below `target`,
/// and anywhere else a mount of one of the target's partitions was made (bootc
/// leaves `/run/bootc/storage`), deepest first, so each can be unmounted in
/// turn.
pub fn leftover_mounts(mounts: &[Mount], target: &Path, nodes: &BTreeSet<String>) -> Vec<String> {
    let mut points: Vec<String> = mounts
        .iter()
        .filter(|m| Path::new(&m.mount_point).starts_with(target) || nodes.contains(&m.source))
        .map(|m| m.mount_point.clone())
        .collect();
    points.sort_by(|a, b| {
        Path::new(b)
            .components()
            .count()
            .cmp(&Path::new(a).components().count())
            .then_with(|| b.cmp(a))
    });
    points.dedup();
    points
}

/// The mounts of this process's namespace.
pub fn current_mounts() -> Result<Vec<Mount>> {
    topology::parse_mountinfo(&fs::read_to_string("/proc/self/mountinfo")?)
}

/// The partitions repart made, found by the UUIDs the plan gave them and
/// checked against sysfs: each node is a block device that is the partition of
/// the right number of the planned disk, with the planned size. Waits for
/// udev to make the links (up to `wait`).
pub fn wait_for_partitions(
    plan: &Plan,
    sys: &Path,
    by_partuuid: &Path,
    wait: Duration,
) -> Result<Vec<(PlannedPartition, String)>> {
    let disk = plan
        .disk
        .path
        .rsplit('/')
        .next()
        .context("the planned disk has no name")?;
    let deadline = Instant::now() + wait;
    loop {
        let why = match check_partitions(plan, disk, sys, by_partuuid) {
            Ok(found) => return Ok(found),
            Err(e) => format!("{e:#}"),
        };
        if Instant::now() >= deadline {
            bail!("the new partitions did not appear as planned: {why}");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn check_partitions(
    plan: &Plan,
    disk: &str,
    sys: &Path,
    by_partuuid: &Path,
) -> Result<Vec<(PlannedPartition, String)>> {
    let mut found = Vec::new();
    for partition in &plan.partitions {
        let link = by_partuuid.join(partition.uuid.to_ascii_lowercase());
        let node = fs::canonicalize(&link)
            .with_context(|| format!("{} is not there yet", link.display()))?;
        let meta = fs::metadata(&node)?;
        ensure!(
            meta.file_type().is_block_device(),
            "{} is not a block device",
            node.display()
        );
        let name = node
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .context("a node with no name")?;
        let block = sys.join("class/block").join(&name);
        let read = |file: &str| -> Result<String> {
            Ok(fs::read_to_string(block.join(file))
                .with_context(|| format!("reading {}/{file}", block.display()))?
                .trim()
                .to_owned())
        };
        let number: u32 = read("partition")?.parse()?;
        ensure!(
            number == partition.number,
            "{} is partition {number}, planned {}",
            node.display(),
            partition.number
        );
        // The partition's parent is the planned disk: its sysfs directory is
        // below the disk's.
        let parent = fs::canonicalize(&block)?
            .parent()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        ensure!(
            parent == disk,
            "{} belongs to {parent}, not to {disk}",
            node.display()
        );
        let sectors: u64 = read("size")?.parse()?;
        ensure!(
            sectors * 512 == partition.size_bytes,
            "{} is {} bytes, planned {}",
            node.display(),
            sectors * 512,
            partition.size_bytes
        );
        let numbers = read("dev")?;
        let rdev = topology::split_dev(meta.rdev());
        ensure!(
            numbers == format!("{}:{}", rdev.0, rdev.1),
            "{} is not the device sysfs lists under its name",
            node.display()
        );
        found.push((partition.clone(), node.to_string_lossy().into_owned()));
    }
    Ok(found)
}

/// The target's compiled SELinux policy: `etc/selinux/targeted/policy/policy.<n>`
/// of the deployment, the one with the highest `n`. The labels of its files are
/// checked against it, not against the policy the live system happens to have
/// loaded (which can be older and lack types the installed system has).
pub fn selinux_policy_file(deployment: &Path) -> Result<PathBuf> {
    let dir = deployment.join("etc/selinux/targeted/policy");
    let mut best: Option<(u32, PathBuf)> = None;
    for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(version) = name
            .strip_prefix("policy.")
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if entry.file_type()?.is_file() && best.as_ref().is_none_or(|(v, _)| version > *v) {
            best = Some((version, entry.path()));
        }
    }
    best.map(|(_, path)| path)
        .with_context(|| format!("no compiled policy in {}", dir.display()))
}

/// The partitions the kernel has for `disk` (a kernel name such as `vda` or
/// `nvme0n1`): the directories of `<sys>/block/<disk>` that have a `partition`
/// file.
pub fn kernel_partitions(sys: &Path, disk: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(sys.join("block").join(disk)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("partition").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Wait until the kernel has no partition of `disk` (up to `wait`).
pub fn wait_for_no_partitions(sys: &Path, disk: &str, wait: Duration) -> Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        let left = kernel_partitions(sys, disk);
        if left.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("the kernel still has the partitions {left:?} of {disk}");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// Bytes in use on the file system holding `path`, from `statvfs`.
pub fn used_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is a valid C string and `stat` is a statvfs to fill.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    let blocks = (stat.f_blocks as u64).saturating_sub(stat.f_bfree as u64);
    Some(blocks * stat.f_frsize as u64)
}
