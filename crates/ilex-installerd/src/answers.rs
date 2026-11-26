//! Answers mode: an installation with no interface. `answers.toml` on a volume
//! labelled `ILEX-ANSWERS` answers what the pages of the interface ask, and
//! this client drives the helper's own methods with it, as root, exactly as an
//! interface would: `Probe`, `Plan`, `SetSecret` (a sealed memfd), `Apply`,
//! then `State` until the installation ends. Nothing here installs anything:
//! every check the helper makes for an interface, it makes for this.
//!
//! The volume is read once. Its file may hold plaintext secrets, so it is
//! mounted read-only without devices, setuid files or execution inside this
//! unit's private mount namespace, read as a bounded regular file, and
//! unmounted; its text lives in memory that is wiped, and no line this
//! program prints quotes it.

use crate::{exec, scratch::Scratch, secret};
use anyhow::{Context, Result, bail, ensure};
use ilex_installer_model::{
    answers::Answers,
    plan::{Plan, PlanError},
    probe::Probe,
    state::{Outcome, State},
};
use std::{
    fs,
    io::Read,
    os::unix::fs::FileTypeExt,
    path::{Path, PathBuf},
    time::Duration,
};
use zbus::{Connection, Proxy};
use zeroize::Zeroizing;

const NAME: &str = "org.ilex.Installer1";
const PATH: &str = "/org/ilex/Installer1";
/// The volume the answers are on.
pub const VOLUME_LABEL: &str = "ILEX-ANSWERS";
/// The file in it.
const FILE_NAME: &str = "answers.toml";
/// An answers file is a few hundred bytes.
const MAX_BYTES: u64 = 64 * 1024;
/// File systems an answers volume may carry. A FAT volume cannot: its labels
/// are at most 11 characters, and `ILEX-ANSWERS` has 12.
const FILE_SYSTEMS: &str = "ext4,ext2,xfs,btrfs,exfat,iso9660,udf";

/// What to do once the installation has succeeded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Then {
    /// Leave the machine as it is.
    Nothing,
    /// Power off: an unattended installation must not boot the medium it just
    /// installed from again.
    PowerOff,
}

pub struct Options {
    pub run_dir: PathBuf,
    /// Read the answers from this file instead of the volume (development
    /// and the `--self-check`).
    pub file: Option<PathBuf>,
    pub then: Then,
}

/// A line a person or a test harness can find on the console. It never holds
/// anything from the answers file. It goes to the journal and the console
/// (standard output) and, once, to the serial port if there is one, without
/// ever waiting for it: a port nobody reads, or one that has stopped, costs
/// this line and nothing else.
fn say(text: &str) {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let line = format!("ILEX-INSTALLER-ANSWERS {text}");
    println!("{line}");
    if let Ok(mut port) = fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open("/dev/ttyS0")
    {
        let _ = port.write(format!("{line}\r\n").as_bytes());
    }
}

/// Where the volume's device is, and nothing else: the label is a udev
/// symlink to the block device.
pub fn find_volume(by_label: &Path) -> Result<PathBuf> {
    let link = by_label.join(VOLUME_LABEL);
    let device = fs::canonicalize(&link)
        .with_context(|| format!("no volume is labelled {VOLUME_LABEL} ({})", link.display()))?;
    ensure!(
        fs::metadata(&device)?.file_type().is_block_device(),
        "{} is not a block device",
        device.display()
    );
    Ok(device)
}

/// Read `answers.toml` from `directory`: a regular file (not a link, not a
/// device) of at most [`MAX_BYTES`].
pub fn read_file(directory: &Path) -> Result<Zeroizing<Vec<u8>>> {
    read_regular(&directory.join(FILE_NAME))
}

fn read_regular(path: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("there is no {FILE_NAME} on the volume"))?;
    ensure!(
        meta.file_type().is_file(),
        "{FILE_NAME} is not a regular file (a link or something else)"
    );
    ensure!(
        meta.len() <= MAX_BYTES,
        "{FILE_NAME} is larger than {MAX_BYTES} bytes"
    );
    let mut bytes = Zeroizing::new(Vec::with_capacity(meta.len() as usize));
    fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        "{FILE_NAME} grew while it was read"
    );
    Ok(bytes)
}

/// Mount the volume read-only in a private directory, read the file, unmount.
fn read_volume(device: &Path, run_dir: &Path) -> Result<Zeroizing<Vec<u8>>> {
    let mut scratch = Scratch::new(run_dir, "answers")?;
    let mountpoint = scratch.subdir("volume")?;
    let mount_point = mountpoint.to_string_lossy().into_owned();
    let mounted = exec::run(
        "mount",
        &[
            "-t",
            FILE_SYSTEMS,
            "-o",
            "ro,nodev,nosuid,noexec",
            &device.to_string_lossy(),
            &mount_point,
        ],
        Duration::from_secs(60),
    )?;
    ensure!(
        mounted.success(),
        "the volume {} could not be mounted ({}); it needs a file system that holds a 12-character label (ext4, exFAT, ISO 9660...), not FAT",
        device.display(),
        mounted.stderr.lines().last().unwrap_or("no message")
    );
    let read = read_file(&mountpoint);
    let unmounted = exec::run("umount", &[&mount_point], Duration::from_secs(60));
    let bytes = read?;
    ensure!(
        unmounted.is_ok_and(|o| o.success()),
        "the answers volume could not be unmounted"
    );
    Ok(bytes)
}

/// What a refused call says: the helper's JSON refusal when the error carries
/// one, else the error itself.
fn describe(error: &zbus::Error) -> String {
    if let zbus::Error::MethodError(_, Some(message), _) = error
        && let Ok(refusal) = serde_json::from_str::<PlanError>(message)
    {
        return format!(
            "{:?}{}: {}",
            refusal.code,
            refusal
                .field
                .map_or_else(String::new, |f| format!(" ({f})")),
            refusal.message
        );
    }
    error.to_string()
}

async fn call<R>(
    proxy: &Proxy<'_>,
    method: &'static str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType + Sync),
) -> Result<R>
where
    R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
{
    proxy
        .call(method, body)
        .await
        .map_err(|e| anyhow::anyhow!("{method}: {}", describe(&e)))
}

fn text_of(state: &State) -> String {
    match &state.outcome {
        Some(Outcome::Failed { stage, reason }) => format!("failed in {}: {reason}", stage.name()),
        Some(Outcome::Success) => "succeeded".into(),
        None => state.message.clone(),
    }
}

/// Run an unattended installation. Ok only when it succeeded; whatever else
/// happened, the last `ILEX-INSTALLER-ANSWERS` line says `result=` and why
/// (a refusal of the helper, a file that is not valid by its position, never
/// by its text).
pub fn run(options: &Options) -> Result<()> {
    let outcome = zbus::block_on(drive(options));
    if let Err(error) = &outcome {
        say(&format!(
            "result=failed {}",
            exec::clean_line(format!("{error:#}").as_bytes())
        ));
    }
    outcome
}

async fn drive(options: &Options) -> Result<()> {
    // The answers.
    let text = match &options.file {
        Some(file) => read_regular(file)?,
        None => {
            let device = find_volume(Path::new("/dev/disk/by-label"))?;
            say(&format!("volume={}", device.display()));
            read_volume(&device, &options.run_dir)?
        }
    };
    let answers = Answers::parse(std::str::from_utf8(&text).context("answers.toml is not UTF-8")?)?;
    drop(text);

    let connection = Connection::system()
        .await
        .context("connecting to the system bus")?;
    let proxy = Proxy::new(&connection, NAME, PATH, NAME).await?;
    let probe: String = call(&proxy, "Probe", &()).await?;
    let probe: Probe = serde_json::from_str(&probe).context("reading the probe")?;
    let selected = probe
        .selection
        .as_ref()
        .map_or_else(|| probe.detection.tag.clone(), |s| s.tag.clone());
    let request = answers
        .to_request(&selected)
        .map_err(|e| anyhow::anyhow!("answers.toml: {}", e.message))?;
    // The volume must not be on the disk the installation would erase.
    if let Some(disk) = probe.disks.iter().find(|d| d.id == request.disk)
        && options.file.is_none()
        && let Ok(device) = find_volume(Path::new("/dev/disk/by-label"))
    {
        let device = device.to_string_lossy();
        ensure!(
            disk.path != device && disk.partitions.iter().all(|p| p.path != device),
            "the {VOLUME_LABEL} volume is on {}, the disk the answers would erase",
            request.disk
        );
    }
    let plan: String = call(&proxy, "Plan", &(serde_json::to_string(&request)?,)).await?;
    let plan: Plan = serde_json::from_str(&plan).context("reading the plan")?;
    say(&format!(
        "plan=ready disk={} composition={} image={} partitions={}",
        plan.disk.id,
        plan.composition.tag,
        plan.image.digest,
        plan.partitions.len()
    ));
    for warning in &plan.warnings {
        say(&format!("warning={}", exec::clean_line(warning.as_bytes())));
    }

    // The password crosses as a sealed memfd, never as a string.
    let memfd = secret::sealed_memfd(answers.administrator.password.expose().as_bytes())?;
    let _: () = call(
        &proxy,
        "SetSecret",
        &(plan.id.as_str(), "admin-password", zbus::Fd::from(&memfd)),
    )
    .await?;
    drop(memfd);
    let confirm = answers.install.confirm_erase.clone().unwrap_or_default();
    drop(answers);
    let state: String = call(&proxy, "Apply", &(plan.id.as_str(), confirm.as_str())).await?;
    let _: State = serde_json::from_str(&state).context("reading the state")?;
    say("apply=started");

    // Follow it. State is the contract: it is what an interface that restarts
    // re-attaches with, so it is also what this follows.
    let mut last = (String::new(), -1.0f64);
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let state: String = call(&proxy, "State", &()).await?;
        let state: State = serde_json::from_str(&state).context("reading the state")?;
        let stage = state.stage.name().to_owned();
        if stage != last.0 || (state.progress - last.1).abs() >= 0.1 {
            say(&format!(
                "stage={stage} progress={:.2} message={}",
                state.progress, state.message
            ));
            last = (stage, state.progress);
        }
        match &state.outcome {
            Some(Outcome::Success) if !state.active => {
                say("result=success");
                if options.then == Then::PowerOff {
                    say("power=off");
                    let _ = exec::run("systemctl", &["poweroff"], Duration::from_secs(30));
                }
                return Ok(());
            }
            Some(Outcome::Failed { .. }) if !state.active => {
                bail!("the installation {}", text_of(&state));
            }
            _ => {}
        }
    }
}
