//! `Finish(action)`: what ends the installer's session. `restart` and
//! `poweroff` ask systemd; `save-log:<device id>` writes a tarball of what the
//! installation logged to a USB stick, so that a person who cannot read the
//! journal of a machine that will not boot can still hand the log to someone.
//!
//! The stick is not trusted and the target is never touched. In order:
//!
//! 1. the device is one a *fresh* probe lists, and the model's rule
//!    ([`check_log_device`]) holds: a removable disk, not the medium the
//!    installer started from, not the disk the installation wrote to (by id
//!    and, when that id still leads to a device, by device number), not in use
//!    by the running system, with a partition whose file system the helper
//!    writes;
//! 2. the partition is checked against sysfs (it is the partition of that disk,
//!    with the number the probe read) and claimed free with an exclusive open,
//!    as every disk is before the probe offers it;
//! 3. the tarball is made in the helper's private scratch space first (stage
//!    logs, `state.json`, the plan, `lsblk`: nothing that holds a secret, since
//!    none is ever passed where a tool could print it), so that the stick sees
//!    one sequential write;
//! 4. a child of this program, in a mount namespace of its own, mounts the
//!    partition `nodev,nosuid,noexec` at a private directory, checks that what
//!    is mounted is the device it was asked for, creates the file (never
//!    overwriting one), syncs it, and unmounts. The mount exists in that
//!    namespace only and ends with the child, whatever happens.

use crate::{
    disks, exec,
    helper::Helper,
    scratch::Scratch,
    sys, target,
    topology::{DevNum, Roots},
};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::{
    finish::{FinishAction, Saved, check_log_device},
    plan::{ErrorCode, PlanError},
    probe::Partition,
    state::State,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
    time::Duration,
};

const TOOL: Duration = Duration::from_secs(120);
/// How much of one stage's log goes into the tarball (its end: a failure is
/// there) and how much of everything.
const MEMBER_MAX: u64 = 4 << 20;
const TARBALL_MAX: usize = 48 << 20;
/// The directory every member of the tarball is in.
const FOLDER: &str = "ilex-installer-log";

impl Helper {
    /// `Finish(action)`. Refused while an installation runs. Returns the answer
    /// as JSON: `{"action": "restart"}` or the [`Saved`] record.
    pub fn finish(&self, action: &str) -> Result<String, PlanError> {
        let action = FinishAction::parse(action)?;
        let internal =
            |e: anyhow::Error| PlanError::new(ErrorCode::Internal, None, format!("{e:#}"));
        // Not while a probe, a plan or an apply works: they share the disks.
        let _busy = self.work.lock().unwrap_or_else(|e| e.into_inner());
        self.refuse_while_installing()?;
        match &action {
            FinishAction::Restart | FinishAction::PowerOff => {
                let verb = if action == FinishAction::Restart {
                    "reboot"
                } else {
                    "poweroff"
                };
                // `--no-block`: the answer goes out before the machine goes down.
                exec::run_ok("systemctl", &["--no-block", verb], TOOL).map_err(internal)?;
                eprintln!("ilex-installerd: finish: {}", action.word());
                Ok(serde_json::json!({"action": action.word()}).to_string())
            }
            FinishAction::SaveLog { device } => {
                let saved = self.save_log(device)?;
                eprintln!(
                    "ilex-installerd: finish: the log was saved to {} ({} bytes)",
                    saved.device, saved.bytes
                );
                serde_json::to_string(&saved).map_err(|e| internal(e.into()))
            }
        }
    }

    fn save_log(&self, device: &str) -> Result<Saved, PlanError> {
        let internal =
            |e: anyhow::Error| PlanError::new(ErrorCode::Internal, None, format!("{e:#}"));
        let refuse = |code: ErrorCode, message: String| {
            PlanError::new(code, Some("device"), message)
        };
        let probe = self.probe_locked().map_err(internal)?;
        let state = self.state();
        let target = state.summary.as_ref().map(|s| s.disk.as_str());
        let (disk, partition) = check_log_device(&probe, device, target)?;

        // What the model cannot see: the device numbers. The stick's by-id name
        // leads to the very device the probe listed, and the installation's
        // target, if its name still leads anywhere, is not that device.
        let (_, rdev) = disks::by_id_node(device).map_err(|why| {
            refuse(
                ErrorCode::UnknownDisk,
                format!("{device} has no name that stays its own: {why}"),
            )
        })?;
        let listed = disks::node_number(&disk.path).map_err(|why| {
            refuse(
                ErrorCode::DiskChanged,
                format!("{device} is not the device the probe listed: {why}"),
            )
        })?;
        if listed != rdev {
            return Err(refuse(
                ErrorCode::DiskChanged,
                format!("{device} does not lead to the device the probe listed"),
            ));
        }
        if let Some(target) = target
            && let Ok((_, target_rdev)) = disks::by_id_node(target)
            && target_rdev == rdev
        {
            return Err(refuse(
                ErrorCode::InstallTarget,
                format!("{device} is the disk the installation was written to"),
            ));
        }
        let roots = Roots::real();
        let disk_kname = kernel_name(&disk.path);
        let part_kname = kernel_name(&partition.path);
        let part_rdev = partition_of(&roots.sys, &disk_kname, &part_kname, partition)
            .map_err(|why| refuse(ErrorCode::DiskChanged, why))?;
        match disks::claim_disk(&roots.sys, &part_kname, &partition.path) {
            disks::Claim::Free => {}
            claim => {
                return Err(refuse(
                    ErrorCode::InUse,
                    disks::claim_reason(&partition.path, &claim).unwrap_or_default(),
                ));
            }
        }

        let (name, tarball) = self.log_tarball(&state, &probe_disks_text()).map_err(internal)?;
        let mut scratch = Scratch::new(&self.config.run_dir, "savelog").map_err(internal)?;
        let source = scratch.path().join("log.tar");
        crate::install::write_file(&source, &tarball, 0o600).map_err(internal)?;
        let mountpoint = scratch.subdir("stick").map_err(internal)?;
        let exe = std::env::current_exe()
            .context("finding this program")
            .map_err(internal)?;
        let out = exec::run_limited(
            "unshare",
            &[
                "--mount",
                "--propagation",
                "private",
                "--",
                &exe.to_string_lossy(),
                "--write-log",
                &partition.path,
                &partition.filesystem,
                &mountpoint.to_string_lossy(),
                &source.to_string_lossy(),
                &name,
                &format!("{}:{}", part_rdev.0, part_rdev.1),
            ],
            Duration::from_secs(300),
            exec::Limits {
                stdout: 64 << 10,
                stderr: 64 << 10,
            },
        )
        .map_err(internal)?;
        let _ = fs::remove_file(&source);
        let written: LogWrite = match out.success() {
            true => serde_json::from_slice(&out.stdout)
                .context("reading what the writer said")
                .map_err(internal)?,
            false => {
                return Err(PlanError::new(
                    ErrorCode::Internal,
                    Some("device"),
                    format!(
                        "the log could not be written: {}",
                        out.stderr.lines().last().unwrap_or("the writer failed")
                    ),
                ));
            }
        };
        if let Some(error) = written.error {
            return Err(PlanError::new(
                ErrorCode::Internal,
                Some("device"),
                format!("the log could not be written to {device}: {error}"),
            ));
        }
        Ok(Saved {
            file: written.file,
            device: device.to_owned(),
            label: format!("{} {}", disk.model.trim(), disk.serial.trim())
                .trim()
                .to_owned(),
            bytes: written.bytes,
        })
    }

    /// The tarball's name and bytes: the stage logs and plan of the last
    /// installation, the state record, and what `lsblk` says of this computer.
    fn log_tarball(&self, state: &State, lsblk: &str) -> Result<(String, Vec<u8>)> {
        let now = sys::unix_time();
        let plan = state
            .applied
            .as_deref()
            .filter(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        let name = log_name(plan, now);
        let mut tar = Tar::default();
        let mut manifest = format!(
            "ilex installer log\ncreated: {now} (unix time)\nhelper: {}\n",
            env!("CARGO_PKG_VERSION")
        );
        if let Some(plan) = plan {
            manifest.push_str(&format!("installation: {plan}\n"));
            let dir = self.config.run_dir.join("log").join(plan);
            let mut names: Vec<_> = fs::read_dir(&dir)
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|e| e.file_name().into_string().ok())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            for file in names {
                if !member_name_ok(&file) {
                    continue;
                }
                match read_tail(&dir.join(&file), MEMBER_MAX) {
                    Ok((bytes, cut)) => {
                        if cut {
                            manifest.push_str(&format!(
                                "{file}: only its last {MEMBER_MAX} bytes are here\n"
                            ));
                        }
                        tar.add(&format!("{FOLDER}/{file}"), &bytes, now)?;
                    }
                    Err(e) => manifest.push_str(&format!("{file}: not read ({e:#})\n")),
                }
            }
        } else {
            manifest.push_str("installation: none was started in this session\n");
        }
        tar.add(
            &format!("{FOLDER}/state.json"),
            &serde_json::to_vec_pretty(state)?,
            now,
        )?;
        tar.add(&format!("{FOLDER}/lsblk.txt"), lsblk.as_bytes(), now)?;
        tar.add(&format!("{FOLDER}/MANIFEST.txt"), manifest.as_bytes(), now)?;
        let bytes = tar.finish()?;
        ensure!(
            bytes.len() <= TARBALL_MAX,
            "the log is {} bytes, more than the {TARBALL_MAX} that are kept",
            bytes.len()
        );
        Ok((name, bytes))
    }
}

/// What `lsblk` says of this computer's block devices, as text (never fails:
/// the reason is the text).
fn probe_disks_text() -> String {
    let text = exec::run_limited(
        "lsblk",
        &[
            "--paths",
            "--bytes",
            "--output",
            "NAME,SIZE,TYPE,FSTYPE,LABEL,PARTLABEL,UUID,PARTUUID,MODEL,SERIAL,TRAN,RM,RO,MOUNTPOINTS",
        ],
        TOOL,
        exec::Limits {
            stdout: 1 << 20,
            stderr: 64 << 10,
        },
    );
    match text {
        Ok(out) if out.success() => out.stdout_text(),
        Ok(out) => format!("lsblk failed: {}\n", out.stderr),
        Err(e) => format!("lsblk could not run: {e:#}\n"),
    }
}

/// `ilex-installer-log-<first eight of the plan id, or none>-<unix time>.tar`.
pub fn log_name(plan: Option<&str>, now: u64) -> String {
    let id = plan.map_or("none", |p| &p[..8.min(p.len())]);
    format!("ilex-installer-log-{id}-{now}.tar")
}

fn kernel_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_owned()
}

/// A file of the logs directory that goes into the tarball: a plain name.
pub fn member_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// The last `max` bytes of a regular file (never a link), and whether the
/// start was left out.
fn read_tail(path: &Path, max: u64) -> Result<(Vec<u8>, bool)> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(meta.is_file(), "not a regular file");
    let cut = meta.len() > max;
    if cut {
        file.seek(SeekFrom::Start(meta.len() - max))?;
    }
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes)?;
    Ok((bytes, cut))
}

/// The partition `part` of the disk `disk`, as sysfs says (its parent is the
/// disk's directory and it has the number the probe read): its device number.
pub fn partition_of(
    sys: &Path,
    disk: &str,
    part: &str,
    partition: &Partition,
) -> Result<DevNum, String> {
    let block = sys.join("class/block").join(part);
    let read = |file: &str| {
        fs::read_to_string(block.join(file))
            .map(|t| t.trim().to_owned())
            .map_err(|e| format!("{part}: {file} cannot be read ({e})"))
    };
    let number: u32 = read("partition")?
        .parse()
        .map_err(|_| format!("{part} has no partition number"))?;
    if number != partition.number {
        return Err(format!(
            "{part} is partition {number}, the probe listed {}",
            partition.number
        ));
    }
    let parent = fs::canonicalize(&block)
        .map_err(|e| format!("{part} cannot be found in sysfs ({e})"))?
        .parent()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_default();
    if parent != disk {
        return Err(format!("{part} belongs to {parent}, not to {disk}"));
    }
    let numbers = read("dev")?;
    let (major, minor) = numbers
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .ok_or_else(|| format!("{part} has no device number"))?;
    Ok((major, minor))
}

// --- the writer: a child in a mount namespace of its own ---------------------

/// What `--write-log` prints.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LogWrite {
    pub file: String,
    pub bytes: u64,
    pub error: Option<String>,
}

/// The file system type `mount` is given for what `lsblk` calls `fstype`.
fn mount_type(fstype: &str) -> Option<&'static str> {
    Some(match fstype {
        "vfat" => "vfat",
        "exfat" => "exfat",
        "ext4" => "ext4",
        "ext3" => "ext3",
        "ext2" => "ext2",
        "ntfs" => "ntfs3",
        _ => return None,
    })
}

/// The `--write-log` child: mount `device` at `mountpoint` (`nodev,nosuid,
/// noexec`), check it is the device numbered `expect`, write `source`'s bytes
/// to a new file called `name` there, sync, unmount. It runs inside
/// `unshare --mount`, so the mount is this process's alone and disappears when
/// it exits, whatever happens here.
pub fn write_log_child(
    device: &str,
    fstype: &str,
    mountpoint: &Path,
    source: &Path,
    name: &str,
    expect: &str,
) -> LogWrite {
    let mut result = LogWrite::default();
    if let Err(e) = write_log(device, fstype, mountpoint, source, name, expect, &mut result) {
        result.error = Some(format!("{e:#}"));
    }
    result
}

fn write_log(
    device: &str,
    fstype: &str,
    mountpoint: &Path,
    source: &Path,
    name: &str,
    expect: &str,
    result: &mut LogWrite,
) -> Result<()> {
    ensure!(
        device.starts_with("/dev/") && fs::symlink_metadata(mountpoint)?.is_dir(),
        "not a device and a plain directory"
    );
    ensure!(
        name.len() <= 128
            && name.ends_with(".tar")
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            && !name.starts_with('.'),
        "the file name is not one the helper made"
    );
    let kind = mount_type(fstype).with_context(|| format!("{fstype} is not written to"))?;
    let (major, minor) = expect
        .split_once(':')
        .and_then(|(a, b)| Some((a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)))
        .context("the expected device number")?;
    ensure!(
        disks::node_number(device).map_err(anyhow::Error::msg)? == (major, minor),
        "{device} is not the partition that was checked"
    );
    let at = mountpoint.to_string_lossy().into_owned();
    let mount = exec::run(
        "mount",
        &[
            "-t",
            kind,
            "-o",
            "rw,nodev,nosuid,noexec,sync",
            device,
            &at,
        ],
        TOOL,
    )?;
    ensure!(
        mount.success(),
        "mount failed: {}",
        mount.stderr.lines().last().unwrap_or("no message")
    );
    // From here every path must reach the unmount.
    let written = (|| -> Result<()> {
        let mounts = target::current_mounts()?;
        ensure!(
            mounts
                .iter()
                .any(|m| Path::new(&m.mount_point) == mountpoint
                    && (m.major, m.minor) == (major, minor)),
            "what is mounted is not the partition that was checked"
        );
        let bytes = fs::read(source)?;
        let mut file = None;
        let mut chosen = String::new();
        for n in 0..10 {
            let candidate = if n == 0 {
                name.to_owned()
            } else {
                format!("{}-{n}.tar", name.trim_end_matches(".tar"))
            };
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(mountpoint.join(&candidate))
            {
                Ok(f) => {
                    file = Some(f);
                    chosen = candidate;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).context("creating the log file"),
            }
        }
        let mut file = file.context("every name the log could have is taken")?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        // SAFETY: `syncfs` on a descriptor this process owns.
        let rc = unsafe { libc::syncfs(std::os::fd::AsRawFd::as_raw_fd(&file)) };
        ensure!(
            rc == 0,
            "syncing the stick: {}",
            std::io::Error::last_os_error()
        );
        result.file = chosen;
        result.bytes = bytes.len() as u64;
        Ok(())
    })();
    let unmounted = exec::run("umount", &[&at], TOOL);
    written?;
    match unmounted {
        Ok(out) if out.success() => Ok(()),
        Ok(out) => bail!(
            "the log was written but umount failed: {}",
            out.stderr.lines().last().unwrap_or("no message")
        ),
        Err(e) => bail!("the log was written but umount could not run: {e:#}"),
    }
}

// --- a tar writer ------------------------------------------------------------

/// A POSIX ustar archive of regular files, in memory: one writer, no
/// dependency, and nothing in it that a member's name can steer out of the
/// folder it extracts to.
#[derive(Default)]
pub struct Tar {
    bytes: Vec<u8>,
}

impl Tar {
    pub fn add(&mut self, name: &str, data: &[u8], mtime: u64) -> Result<()> {
        ensure!(
            !name.is_empty()
                && name.len() <= 100
                && !name.starts_with('/')
                && name
                    .split('/')
                    .all(|part| !part.is_empty() && part != "." && part != "..")
                && name.bytes().all(|b| (0x20..0x7f).contains(&b)),
            "{name:?} is not a name a log member may have"
        );
        ensure!(data.len() < 1 << 33, "{name} is too big for a tar member");
        let mut header = [0u8; 512];
        let put = |header: &mut [u8; 512], at: usize, text: &[u8]| {
            header[at..at + text.len()].copy_from_slice(text);
        };
        put(&mut header, 0, name.as_bytes());
        put(&mut header, 100, b"0000644\0");
        put(&mut header, 108, b"0000000\0");
        put(&mut header, 116, b"0000000\0");
        put(&mut header, 124, format!("{:011o}\0", data.len()).as_bytes());
        put(&mut header, 136, format!("{mtime:011o}\0").as_bytes());
        put(&mut header, 148, b"        ");
        header[156] = b'0';
        put(&mut header, 257, b"ustar\0");
        put(&mut header, 263, b"00");
        put(&mut header, 265, b"root");
        put(&mut header, 297, b"root");
        let sum: u32 = header.iter().map(|b| u32::from(*b)).sum();
        put(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());
        self.bytes.extend_from_slice(&header);
        self.bytes.extend_from_slice(data);
        let padding = (512 - data.len() % 512) % 512;
        self.bytes.extend(std::iter::repeat_n(0u8, padding));
        Ok(())
    }

    /// The archive with its two zero blocks.
    pub fn finish(mut self) -> Result<Vec<u8>> {
        self.bytes.extend(std::iter::repeat_n(0u8, 1024));
        Ok(self.bytes)
    }
}

/// The members of an archive [`Tar`] wrote, for the self-check: names and
/// contents, with each header's checksum verified.
pub fn untar(bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>> {
    let mut members = Vec::new();
    let mut at = 0;
    while at + 512 <= bytes.len() {
        let header = &bytes[at..at + 512];
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let text = |from: usize, to: usize| {
            String::from_utf8_lossy(&header[from..to])
                .trim_end_matches(['\0', ' '])
                .to_owned()
        };
        let mut copy = [0u8; 512];
        copy.copy_from_slice(header);
        copy[148..156].copy_from_slice(b"        ");
        let sum: u32 = copy.iter().map(|b| u32::from(*b)).sum();
        ensure!(
            u32::from_str_radix(&text(148, 154), 8)? == sum,
            "a header's checksum is wrong"
        );
        ensure!(
            &header[257..262] == b"ustar" && header[156] == b'0',
            "not a ustar regular file"
        );
        let size = usize::from_str_radix(&text(124, 135), 8)?;
        let body = bytes
            .get(at + 512..at + 512 + size)
            .context("a member is cut short")?;
        members.push((text(0, 100), body.to_vec()));
        at += 512 + size.div_ceil(512) * 512;
    }
    Ok(members)
}

/// Print what the `--write-log` child did, as the parent reads it.
pub fn run_write_log(args: &[std::path::PathBuf]) -> Result<()> {
    let [device, fstype, mountpoint, source, name, expect] = args else {
        bail!("--write-log takes DEVICE FSTYPE MOUNTPOINT SOURCE NAME MAJOR:MINOR");
    };
    let result = write_log_child(
        &device.to_string_lossy(),
        &fstype.to_string_lossy(),
        mountpoint,
        source,
        &name.to_string_lossy(),
        &expect.to_string_lossy(),
    );
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}
