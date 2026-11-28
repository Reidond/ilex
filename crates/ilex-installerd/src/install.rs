//! The installation itself: the stages that write the disk, run by `Apply` on a
//! thread of their own (plan 9.1, whole disk):
//!
//! partition → [encrypt →] mount → image → configure → account → finalise
//!
//! An encrypted installation's partition stage also makes the LUKS2 volume
//! (`systemd-repart` formats it with the passphrase as the key of its one
//! keyslot), and its encrypt stage opens the volume, checks the header it was
//! given and keeps the opened device for the mount stage. The passphrase goes
//! to the tools as a descriptor (`--key-file=/proc/self/fd/N`), never in an
//! argument or a log, and is wiped when the encrypt stage is over.
//!
//! Each stage is recorded before it writes anything ([`Tracker`]), logs what
//! its tools print under `/run/ilex-installer/log/<plan>/`, and fails with a
//! sentence that names the stage and the tool. A failure after the partition
//! stage leaves the disk changed and says so; there is no resuming, and the
//! next attempt starts again from a fresh plan.

use crate::{
    disks::{self, Hold},
    enrol,
    exec::{self, Exit, Flow, Job},
    fault,
    helper::Helper,
    luks,
    recovery::{Verifier, Wait},
    repart,
    secret::{self, Secret},
    stagelog::{Logs, StageLog},
    stamp, target,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use ilex_installer_model::{
    plan::{Plan, PlannedPartition},
    recovery::{self, KeyStatus, TpmOutcome, TpmStatus},
    state::Stage,
};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};
use zeroize::Zeroizing;

const TOOL: Duration = Duration::from_secs(120);
const REPART: Duration = Duration::from_secs(300);
/// A pull over a slow link takes long; a hung one must still end.
const BOOTC: Duration = Duration::from_secs(3600);
const SETFILES: Duration = Duration::from_secs(300);

/// How much `bootc` writes for each byte it downloads, measured against
/// `generic.gnome` (1,688 MiB of layers, 4,150 MiB in use on the new btrfs at
/// the end, a ratio of 2.46; the bar's estimate takes a margin): the progress
/// bar's estimate, which is why it never claims to be done before the tool says
/// so.
const WRITTEN_PER_DOWNLOADED: f64 = 2.6;

/// The stages a plan's installation goes through.
pub fn stages(plan: &Plan) -> Vec<Stage> {
    Stage::of_plan(plan)
}

pub struct Install {
    pub helper: Arc<Helper>,
    pub plan: Plan,
    pub logs: Logs,
    pub target: PathBuf,
    /// The stable name the disk is written by once the exclusive claim has
    /// been let go: `/dev/disk/by-id/<id>`, checked against the held device.
    address: String,
    /// The device nodes of the partitions this installation made.
    nodes: BTreeSet<String>,
    admin: Secret,
    /// The disk's passphrase, until the encrypt stage has used it.
    passphrase: Option<Secret>,
    /// The passphrase as the file the tools read, from before the partition
    /// stage until the encrypt stage is over.
    key: Option<luks::KeyFile>,
    /// The LUKS volume this installation made: its UUID, and whether it is
    /// open (the device-mapper name [`luks::mapping_name`] exists).
    volume: Option<Volume>,
    /// The partitions repart made, as found after the partition stage.
    found: Option<Vec<(PlannedPartition, String)>>,
    /// What the disk's stamp held before it was wiped.
    earlier: stamp::Earlier,
    /// The digests of the provisional stamp the mount stage left on the new
    /// `/boot`, when it left one.
    provisional: Option<Vec<String>>,
    /// The stage that is running (or failed).
    stage: Stage,
}

/// The encrypted volume of an installation.
struct Volume {
    uuid: String,
    open: bool,
}

/// The session on the new volume that made the recovery key, and the key's
/// slot and token.
struct Enrolment<'a> {
    session: ilex_enroll::Reconciled<'a>,
    entry: ilex_enroll::Enrollment,
}

struct Output {
    exit: Exit,
    lines: Vec<String>,
}

fn last_line(lines: &[String]) -> &str {
    lines
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or("no output", String::as_str)
}

fn mib(bytes: u64) -> String {
    format!("{} MiB", bytes >> 20)
}

/// The JSON array of `systemd-repart --json=pretty` among the lines of its
/// merged output: from a line that is `[` to the next that is `]`.
pub fn json_array(lines: &[String]) -> Result<String> {
    let start = lines
        .iter()
        .position(|l| l.trim() == "[")
        .context("systemd-repart printed no partition list")?;
    let end = lines[start..]
        .iter()
        .position(|l| l.trim() == "]")
        .context("systemd-repart's partition list is not finished")?;
    Ok(lines[start..=start + end].join("\n"))
}

/// Run the installation and record how it ended. This is the thread's body.
pub fn run(
    helper: Arc<Helper>,
    plan: Plan,
    hold: Hold,
    address: String,
    admin: Secret,
    passphrase: Option<Secret>,
    earlier: stamp::Earlier,
) {
    let logs = match Logs::new(&helper.config.run_dir, &plan.id) {
        Ok(logs) => logs,
        Err(e) => {
            helper.tracker.fail(
                Stage::Partition,
                &format!("the logs cannot be opened: {e:#}"),
            );
            return;
        }
    };
    // The plan the installation was applied from (the request names the
    // administrator, never the password): what a log saved to a stick holds
    // beside the stages' logs, and what the installed system's logs hold.
    if let Err(e) = write_file(
        &logs.dir().join("plan.json"),
        &serde_json::to_vec_pretty(&plan).unwrap_or_default(),
        0o600,
    ) {
        eprintln!("ilex-installerd: the plan was not kept with the logs: {e:#}");
    }
    let target = helper.config.run_dir.join("target");
    let mut install = Install {
        helper,
        plan,
        logs,
        target,
        address,
        nodes: BTreeSet::new(),
        admin,
        passphrase,
        key: None,
        volume: None,
        found: None,
        earlier,
        provisional: None,
        stage: Stage::Partition,
    };
    match install.stages(hold) {
        Ok(()) => install.helper.tracker.succeed(),
        Err(error) => {
            let reason = format!("{error:#}");
            eprintln!(
                "ilex-installerd: installation failed in the {} stage: {reason}",
                install.stage.name()
            );
            install.cleanup();
            install.helper.tracker.fail(install.stage, &reason);
        }
    }
    // Whatever the installation held of a recovery key is wiped now, whichever
    // way it ended.
    install.helper.recovery.clear();
    // The secret was moved into `install`: dropped (and wiped) here.
}

impl Install {
    fn begin(&mut self, stage: Stage, message: &str) -> Result<StageLog> {
        self.stage = stage;
        self.helper.tracker.start(stage, message)?;
        let mut log = self.logs.stage(stage)?;
        log.line(&format!("stage {} begins: {message}", stage.name()));
        Ok(log)
    }

    fn end(&self, stage: Stage, log: &mut StageLog) {
        log.line(&format!("stage {} ends", stage.name()));
        self.helper.tracker.done(stage);
    }

    fn stages(&mut self, hold: Hold) -> Result<()> {
        // The passphrase as a file for the tools, made before anything is
        // written (a failure here leaves the disk as it was).
        if let Some(passphrase) = &self.passphrase {
            self.key = Some(luks::KeyFile::new(passphrase)?);
        }
        self.partition(hold)?;
        let mut prepared = None;
        if self.plan.request.encrypt {
            self.encrypt()?;
            // The recovery key is made while the volume is open and the
            // passphrase still here: `ilex-enroll` unlocks the header with it.
            if self.plan.encryption.recovery_key {
                prepared = self.prepare_enrolment();
            }
        }
        // Whatever the stages above held of the passphrase is wiped now: the
        // key file overwritten, the secret's pages zeroed. (`prepared` keeps
        // its own copy, for the recovery key and the security chip, and wipes
        // it when it goes out of scope below.)
        self.key = None;
        self.passphrase = None;
        let mut enrolment = prepared.as_ref().and_then(|p| self.make_recovery_key(p));
        self.mount()?;
        self.image()?;
        let deployment =
            target::find_deployment(&self.target).context("looking for what bootc deployed")?;
        self.configure(&deployment)?;
        self.account(&deployment)?;
        self.recovery_and_chip(&deployment, prepared.as_ref(), enrolment.as_mut())?;
        self.finalise(&deployment)
    }

    /// Run a tool, logging its command line and every line it prints.
    fn tool(&self, log: &mut StageLog, job: &Job<'_>, tick: &mut dyn FnMut()) -> Result<Output> {
        log.command(job.program, job.args);
        let tracker = &self.helper.tracker;
        let mut lines = Vec::new();
        let exit = exec::run_job(
            job,
            &mut |line| {
                log.line(line);
                tracker.line(line);
                if lines.len() < 2000 {
                    lines.push(line.to_owned());
                }
                Flow::Continue
            },
            tick,
        )?;
        log.line(&format!("{} {}", job.program, exit.describe()));
        Ok(Output { exit, lines })
    }

    /// A tool that must succeed; its output on success.
    fn checked(
        &self,
        log: &mut StageLog,
        program: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<Vec<String>> {
        self.checked_with(log, program, args, timeout, &[])
    }

    /// [`Install::checked`] for a tool that reads the passphrase's key file
    /// through a descriptor it inherits.
    fn checked_with(
        &self,
        log: &mut StageLog,
        program: &str,
        args: &[&str],
        timeout: Duration,
        inherit: &[std::os::fd::RawFd],
    ) -> Result<Vec<String>> {
        let out = self.tool(
            log,
            &Job {
                program,
                args,
                env: &[("HOME", "/root"), ("TMPDIR", "/var/tmp")],
                stdin: None,
                inherit,
                timeout,
            },
            &mut || {},
        )?;
        if !out.exit.success() {
            bail!(
                "{program} {}: {}",
                out.exit.describe(),
                last_line(&out.lines)
            );
        }
        Ok(out.lines)
    }

    // --- partition -------------------------------------------------------

    fn partition(&mut self, hold: Hold) -> Result<()> {
        let mut log = self.begin(Stage::Partition, "Writing the partition table")?;
        let layout = repart::layout(&self.helper.config.definitions, self.plan.request.encrypt)?;
        let definitions = format!("--definitions={}", layout.dir.display());
        let seed = format!("--seed={}", self.plan.seed);
        let kernel_name = self.plan.disk.path.clone();
        // The disk is addressed by its stable name from here, so that a disk
        // that is swapped for another taking the kernel's name cannot be the
        // one wiped: the name leads to the device held, verified as late as it
        // can be, and a replacement has a name of its own.
        let disk = self.address.clone();
        disks::by_id_address(&self.plan.disk.id, &hold)
            .map_err(|why| anyhow!("{} no longer names the held disk: {why}", self.address))?;
        // The wipe takes the disk's stamp with it, and an installation that
        // fails after it (the pull, say) leaves a new, empty `ilex-boot` for a
        // retry in this boot to read: the digests the stamp held are kept in
        // `/run` first, and the stamp of whatever installation ends in this
        // boot is made from them too. Nothing is written to the disk yet, so a
        // failure to keep them ends the installation here, with the disk as it
        // was.
        self.keep_earlier(&mut log)?;
        // The disk is as good as written from here: recorded first, so that a
        // helper that dies before it returns leaves a record that says so.
        self.helper.tracker.disk_changed()?;
        // systemd-repart formats the new partitions through their own device
        // nodes, which the kernel refuses while this process holds the whole
        // disk exclusively (EBUSY from mkfs.vfat, measured): the claim ends
        // here, as late as it can.
        log.line(&format!(
            "releasing the exclusive claim on {kernel_name}; the disk is {disk} from now on"
        ));
        let held = hold.rdev;
        hold.release();
        // A disk that has partitions must lose them first. `systemd-repart
        // --empty=force` wipes the table but not the kernel's partitions, and
        // then cannot add the new first partition over the old one ("Failed to
        // create new partition: Device or resource busy", measured with a
        // second run on a disk the first had partitioned). `wipefs --all`
        // removes the table's signatures and has the kernel forget the
        // partitions, which is what a person expects whole-disk mode to do to
        // a disk that was used.
        self.checked(&mut log, "wipefs", &["--all", &disk], TOOL)?;
        let kname = kernel_name
            .rsplit('/')
            .next()
            .unwrap_or(&kernel_name)
            .to_owned();
        target::wait_for_no_partitions(Path::new("/sys"), &kname, Duration::from_secs(20))?;
        // udev has seen the signatures go and the partitions with them: let it
        // finish removing and making its links before anything is written, so
        // that repart does not race it (a link of the old contents vanishes
        // only now).
        let _ = self.tool(
            &mut log,
            &Job {
                program: "udevadm",
                args: &["settle", "--timeout=30"],
                env: &[],
                stdin: None,
                inherit: &[],
                timeout: Duration::from_secs(40),
            },
            &mut || {},
        );
        // The disk's stable name is a hardware identity, so it is still there
        // and still leads to the disk that was held; if it is not, the disk is
        // gone or another took its place, and nothing more is written to it.
        let (_, now) = disks::by_id_node(&self.plan.disk.id).map_err(|why| {
            anyhow!(
                "{} is gone after the disk was wiped ({why}); the disk was erased, install again from a new plan",
                self.address
            )
        })?;
        ensure!(
            now == held,
            "{} no longer leads to the disk that was wiped ({}:{} against {}:{}); the disk was erased, install again from a new plan",
            self.address,
            now.0,
            now.1,
            held.0,
            held.1
        );
        // The passphrase is the key of the new volume's one keyslot: repart
        // reads it from the descriptor it inherits, and nothing names it.
        let key_file = self.key.as_ref().map(|key| (key.path(), key.raw()));
        let key_arg = key_file
            .as_ref()
            .map(|(path, _)| format!("--key-file={path}"));
        let inherit: Vec<std::os::fd::RawFd> = key_file.iter().map(|(_, fd)| *fd).collect();
        let mut args = vec![
            "--dry-run=no",
            "--empty=force",
            "--pretty=no",
            "--no-pager",
            "--json=pretty",
            definitions.as_str(),
            seed.as_str(),
        ];
        if let Some(arg) = &key_arg {
            args.push(arg);
        }
        args.push(&disk);
        let lines = self.checked_with(&mut log, "systemd-repart", &args, REPART, &inherit)?;
        let made = repart::parse(&json_array(&lines)?, &layout)
            .context("reading what systemd-repart made")?;
        ensure!(
            repart::same_partitions(&made, &self.plan.partitions),
            "the partitions systemd-repart made are not the planned ones; the disk was erased, install again from a new plan"
        );
        self.end(Stage::Partition, &mut log);
        Ok(())
    }

    /// Keep the digests of the stamp the disk held in [`stamp::EARLIER`], the
    /// last thing before it is wiped.
    fn keep_earlier(&self, log: &mut StageLog) -> Result<()> {
        if self.earlier.digests.is_empty() {
            return Ok(());
        }
        stamp::keep_earlier(&self.helper.config.earlier_file, &self.earlier.digests)
            .context("keeping the digests of the disk's stamp before it is wiped")?;
        log.line(&format!(
            "kept the {} digest{} of the disk's earlier stamp in {}, to carry them into the stamp of whatever installation ends in this boot",
            self.earlier.digests.len(),
            if self.earlier.digests.len() == 1 { "" } else { "s" },
            self.helper.config.earlier_file.display()
        ));
        Ok(())
    }

    // --- the partitions --------------------------------------------------

    /// The partitions repart made, found by their UUIDs and checked against
    /// sysfs, once: the first stage that needs them looks (and lets udev finish
    /// its probing of the new file systems), the later ones are given the same
    /// list.
    fn partitions(&mut self, log: &mut StageLog) -> Result<Vec<(PlannedPartition, String)>> {
        if let Some(found) = &self.found {
            return Ok(found.clone());
        }
        let found = target::wait_for_partitions(
            &self.plan,
            Path::new("/sys"),
            Path::new("/dev/disk/by-partuuid"),
            Duration::from_secs(30),
        )?;
        for (partition, node) in &found {
            log.line(&format!(
                "partition {} ({}) is {node}: {} bytes at {}",
                partition.number, partition.kind, partition.size_bytes, partition.start_bytes
            ));
            self.nodes.insert(node.clone());
        }
        // udev has the links; let its probing of the new file systems finish
        // before they are opened or mounted.
        let _ = self.tool(
            log,
            &Job {
                program: "udevadm",
                args: &["settle", "--timeout=30"],
                env: &[],
                stdin: None,
                inherit: &[],
                timeout: Duration::from_secs(40),
            },
            &mut || {},
        );
        self.found = Some(found.clone());
        Ok(found)
    }

    // --- encrypt ---------------------------------------------------------

    /// The new volume, opened: repart made it (the passphrase is the key of
    /// its one keyslot) and closed it again. The volume's UUID is read, its
    /// header is checked against what the installed system's unlock stack
    /// accepts, and it is opened as `luks-<uuid>`, the name the installed
    /// system's initramfs gives it; the mount stage mounts that device, not
    /// the partition.
    fn encrypt(&mut self) -> Result<()> {
        let mut log = self.begin(Stage::Encrypt, "Opening the encrypted volume")?;
        let found = self.partitions(&mut log)?;
        let (root, node) = found
            .iter()
            .find(|(p, _)| p.kind.starts_with("root"))
            .context("no root partition was made")?;
        ensure!(
            root.encrypted,
            "the root partition {node} was not planned as an encrypted one"
        );
        let uuid =
            luks::parse_uuid(&self.checked(&mut log, "cryptsetup", &["luksUUID", node], TOOL)?)?;
        // The header, whole: its JSON is one long line, which the output of a
        // job (cut at 300 characters a line) cannot carry.
        log.command("cryptsetup", &["luksDump", "--dump-json-metadata", node]);
        let dump = exec::run(
            "cryptsetup",
            &["luksDump", "--dump-json-metadata", node],
            TOOL,
        )?;
        ensure!(
            dump.success(),
            "cryptsetup luksDump failed: {}",
            dump.stderr.lines().last().unwrap_or("no message")
        );
        let header = luks::check_header(&dump.stdout_text())
            .context("the volume repart made is not what the installed system unlocks")?;
        log.line(&format!(
            "LUKS2 volume {uuid}: {}, {}-bit payload key, one keyslot derived with {}, {}-byte sectors",
            header.cipher, header.key_bits, header.kdf, header.sector_size
        ));
        let key = self
            .key
            .as_ref()
            .context("the passphrase is not available to open the volume with")?;
        let key_arg = key.path();
        let name = luks::mapping_name(&uuid);
        self.volume = Some(Volume {
            uuid: uuid.clone(),
            open: false,
        });
        self.checked_with(
            &mut log,
            "cryptsetup",
            &[
                "open",
                "--type",
                "luks2",
                "--allow-discards",
                "--key-file",
                &key_arg,
                node,
                &name,
            ],
            Duration::from_secs(300),
            &[key.raw()],
        )?;
        if let Some(volume) = &mut self.volume {
            volume.open = true;
        }
        let mapper = luks::mapping_node(&uuid);
        self.nodes.insert(mapper.clone());
        let _ = self.tool(
            &mut log,
            &Job {
                program: "udevadm",
                args: &["settle", "--timeout=30"],
                env: &[],
                stdin: None,
                inherit: &[],
                timeout: Duration::from_secs(40),
            },
            &mut || {},
        );
        ensure!(
            fs::metadata(&mapper)
                .is_ok_and(|m| std::os::unix::fs::FileTypeExt::is_block_device(&m.file_type())),
            "{mapper} is not a block device after the volume was opened"
        );
        // Whatever repart formatted inside is the planned root file system.
        let lines = self.checked(
            &mut log,
            "blkid",
            &[
                "--probe",
                "--output",
                "value",
                "--match-tag",
                "TYPE",
                &mapper,
            ],
            TOOL,
        )?;
        ensure!(
            last_line(&lines) == root.filesystem,
            "the opened volume holds {:?}, not a {} file system",
            last_line(&lines),
            root.filesystem
        );
        self.end(Stage::Encrypt, &mut log);
        Ok(())
    }

    /// Close the opened volume (after everything of it is unmounted), trying
    /// again while something still holds the device.
    fn close_volume(&mut self, log: &mut StageLog) -> Result<()> {
        let Some(volume) = &mut self.volume else {
            return Ok(());
        };
        if !volume.open {
            return Ok(());
        }
        let name = luks::mapping_name(&volume.uuid);
        let mut last = String::new();
        for attempt in 0..6 {
            log.command("cryptsetup", &["close", &name]);
            let out = exec::run("cryptsetup", &["close", &name], TOOL)?;
            if out.success() || !Path::new(&luks::mapping_node(&volume.uuid)).exists() {
                volume.open = false;
                log.line(&format!("closed {name}"));
                return Ok(());
            }
            last = out.stderr.lines().last().unwrap_or("no message").to_owned();
            if attempt < 5 {
                thread::sleep(Duration::from_secs(1));
            }
        }
        bail!("cryptsetup close {name}: {last}")
    }

    // --- mount -----------------------------------------------------------

    fn mount(&mut self) -> Result<()> {
        let mut log = self.begin(Stage::Mount, "Mounting the new partitions")?;
        let found = self.partitions(&mut log)?;
        let by_kind = |prefix: &str| -> Result<&(PlannedPartition, String)> {
            found
                .iter()
                .find(|(p, _)| p.kind.starts_with(prefix))
                .with_context(|| format!("no {prefix} partition was made"))
        };
        let (root, root_partition) = by_kind("root")?;
        // The root of an encrypted installation is the opened volume.
        let root_node = match &self.volume {
            Some(volume) => luks::mapping_node(&volume.uuid),
            None => root_partition.clone(),
        };
        let (boot, boot_node) = by_kind("xbootldr")?;
        let (esp, esp_node) = by_kind("esp")?;
        make_dir(&self.target)?;
        let target = self.target.to_string_lossy().into_owned();
        self.checked(
            &mut log,
            "mount",
            &["-t", &root.filesystem, &root_node, &target],
            TOOL,
        )?;
        let boot_dir = self.target.join("boot");
        make_dir(&boot_dir)?;
        self.checked(
            &mut log,
            "mount",
            &[
                "-t",
                &boot.filesystem,
                boot_node,
                &boot_dir.to_string_lossy(),
            ],
            TOOL,
        )?;
        let esp_dir = boot_dir.join("efi");
        make_dir(&esp_dir)?;
        self.checked(
            &mut log,
            "mount",
            &[
                "-t",
                &esp.filesystem,
                "-o",
                "umask=0077,shortname=winnt",
                esp_node,
                &esp_dir.to_string_lossy(),
            ],
            TOOL,
        )?;
        // The disk's old stamp went with the wipe, and the record of what this
        // boot saw and wiped lives in `/run`: a reboot before the image is in
        // (the pull fails, the person powers off) would lose them all. They
        // are on the new `/boot` from here.
        self.provisional_stamp(&mut log)?;
        self.end(Stage::Mount, &mut log);
        Ok(())
    }

    // --- image -----------------------------------------------------------

    fn image(&mut self) -> Result<()> {
        let mut log = self.begin(Stage::Image, "Downloading and installing ilex")?;
        // The `installer::` and `iso::next_unattended_reboot` trials' fault
        // (the fixed credential ilex.installer.fault=image; it can only make
        // the installer fail): the stage fails as a pull that fails does, with
        // the disk wiped, the new `/boot` mounted and nothing deployed.
        if fault::requested("image") {
            log.line("fault=image (a test credential): the image stage fails before bootc is run");
            bail!("fault=image (a test credential): the image stage is made to fail");
        }
        let source = format!("registry:{}", self.plan.image.source);
        let target = self.target.to_string_lossy().into_owned();
        let expected = (self.plan.image.download_bytes as f64 * WRITTEN_PER_DOWNLOADED) as u64;
        let tracker = &self.helper.tracker;
        let root = self.target.clone();
        let mut tick = || {
            if let Some(used) = target::used_bytes(&root) {
                // An estimate, and honest about it: never full before the
                // tool says it is done.
                let fraction = (used as f64 / expected.max(1) as f64).min(0.97);
                tracker.progress(
                    fraction,
                    Some(&format!(
                        "Downloading and installing ilex: {} written of about {}",
                        mib(used),
                        mib(expected)
                    )),
                );
            }
        };
        // An encrypted root: bootc knows nothing of a volume it did not make,
        // so it is told how the installed system finds and unlocks it (and in
        // which layout the passphrase is typed: the keymap of the console).
        let kargs: Vec<String> = self
            .volume
            .as_ref()
            .map(|volume| luks::kernel_args(&volume.uuid, &self.plan.request.keyboard))
            .unwrap_or_default()
            .into_iter()
            .map(|karg| format!("--karg={karg}"))
            .collect();
        let mut args = vec![
            "install",
            "to-filesystem",
            "--source-imgref",
            source.as_str(),
            "--target-imgref",
            self.plan.image.target.as_str(),
            "--enforce-container-sigpolicy",
            "--skip-finalize",
        ];
        args.extend(kargs.iter().map(String::as_str));
        args.push(&target);
        let out = self.tool(
            &mut log,
            &Job {
                program: "bootc",
                args: &args,
                env: &[("HOME", "/root"), ("TMPDIR", "/var/tmp")],
                stdin: None,
                inherit: &[],
                timeout: BOOTC,
            },
            &mut tick,
        )?;
        if !out.exit.success() {
            bail!("bootc {}: {}", out.exit.describe(), last_line(&out.lines));
        }
        self.provisional_intact(&mut log)?;
        self.helper
            .tracker
            .progress(0.99, Some("Checking what was installed"));
        // What was installed is what the plan pinned and what the machine will
        // verify its updates against.
        let deployment = target::find_deployment(&self.target)?;
        let wanted = target::expected_origin(&self.plan.image.target);
        ensure!(
            target::origin_reference(&deployment.origin) == Some(wanted.as_str()),
            "the installed system tracks {:?}, not {wanted}",
            target::origin_reference(&deployment.origin)
        );
        let repo = format!("--repo={}", self.target.join("ostree/repo").display());
        let lines = self.checked(
            &mut log,
            "ostree",
            &[
                "show",
                "--print-metadata-key=ostree.manifest-digest",
                &repo,
                &deployment.checksum,
            ],
            TOOL,
        )?;
        let digest = last_line(&lines).trim().trim_matches('\'').to_owned();
        ensure!(
            digest == self.plan.image.digest,
            "the installed image is {digest}, not the planned {}",
            self.plan.image.digest
        );
        log.line(&format!("installed {digest}, tracking {wanted}"));
        self.end(Stage::Image, &mut log);
        Ok(())
    }

    // --- configure -------------------------------------------------------

    fn configure(&mut self, deployment: &target::Deployment) -> Result<()> {
        let mut log = self.begin(
            Stage::Configure,
            "Setting up language, keyboard, time zone and host name",
        )?;
        let root = format!("--root={}", deployment.path.display());
        let request = &self.plan.request;
        // locale.conf, vconsole.conf (with the keymap's X11 layout, as
        // systemd-localed keeps it), localtime and hostname.
        self.checked(
            &mut log,
            "systemd-firstboot",
            &[
                &root,
                "--force",
                &format!("--locale={}", request.language),
                &format!("--keymap={}", request.keyboard),
                &format!("--timezone={}", request.time_zone),
                &format!("--hostname={}", request.host_name),
            ],
            TOOL,
        )?;
        let etc = deployment.path.join("etc");
        let vconsole = fs::read_to_string(etc.join("vconsole.conf")).unwrap_or_default();
        ensure!(
            vconsole
                .lines()
                .any(|l| l == format!("KEYMAP={}", request.keyboard)),
            "vconsole.conf does not carry the keymap {}",
            request.keyboard
        );
        // The reserved record the first boot reads (`ilexctl home setup` and
        // the profile service decide what to do from it).
        let setup = etc.join("ilex");
        make_dir_mode(&setup, 0o755)?;
        write_file(
            &setup.join("setup.json"),
            target::setup_record(&self.plan, None).as_bytes(),
            0o644,
        )?;
        // The encrypted root's line of /etc/crypttab: the shape Anaconda
        // writes, which the installed system's unlock stack checks.
        if let Some(volume) = &self.volume {
            let line = luks::crypttab_line(&volume.uuid);
            write_file(&etc.join("crypttab"), line.as_bytes(), 0o600)?;
            log.line(&format!("wrote /etc/crypttab: {}", line.trim_end()));
        }
        // No line for `/`: the root is an overlay the initramfs mounts.
        match fs::read_to_string(etc.join("fstab")) {
            Ok(text) => {
                let (kept, dropped) = target::strip_root_fstab(&text);
                if dropped > 0 {
                    write_file(&etc.join("fstab"), kept.as_bytes(), 0o644)?;
                    log.line(&format!("removed {dropped} line(s) for / from /etc/fstab"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("reading /etc/fstab"),
        }
        // A shell means a graphical system: the image says so itself (its
        // default target is graphical.target); this is the check, and the
        // repair if a composition ever ships without.
        let graphical = !self.plan.composition.shells.is_empty();
        let current = target::default_target(&deployment.path)?;
        log.line(&format!("the default target is {current}"));
        if graphical && current != "graphical.target" {
            let link = etc.join("systemd/system/default.target");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink("/usr/lib/systemd/system/graphical.target", &link)
                .context("making graphical.target the default")?;
            log.line("made graphical.target the default (the image did not)");
        }
        self.end(Stage::Configure, &mut log);
        Ok(())
    }

    // --- account ---------------------------------------------------------

    fn account(&mut self, deployment: &target::Deployment) -> Result<()> {
        let mut log = self.begin(Stage::Account, "Creating the administrator account")?;
        let name = self.plan.request.administrator.clone();
        let root = deployment.path.to_string_lossy().into_owned();
        let password = secret::admin_password(&self.admin)?;
        // The deployment's /var is an empty directory the machine mounts the
        // stateroot's var over: do that here, so the home lands in /var/home.
        // And a /proc: the tools below chroot into the deployment, and without
        // one libselinux cannot tell that SELinux is on, let alone permissive:
        // pam_unix then asks it whether `passwd` may change a password,
        // is told no for a process of the live system's unlabeled domain, and
        // `chpasswd` fails with "Authentication token manipulation error"
        // (measured on the ISO's own layout; with SELinux disabled, or with
        // /proc, it works).
        let var = deployment.path.join("var");
        let proc = deployment.path.join("proc");
        self.checked(
            &mut log,
            "mount",
            &[
                "--bind",
                &deployment.var.to_string_lossy(),
                &var.to_string_lossy(),
            ],
            TOOL,
        )?;
        let proc_mounted = self.checked(
            &mut log,
            "mount",
            &[
                "-t",
                "proc",
                "-o",
                "nosuid,nodev,noexec",
                "proc",
                &proc.to_string_lossy(),
            ],
            TOOL,
        );
        let added = proc_mounted.and_then(|_| {
            self.checked(
                &mut log,
                "useradd",
                &[
                    "--root",
                    &root,
                    "--create-home",
                    "--groups",
                    "wheel",
                    "--shell",
                    "/bin/bash",
                    &name,
                ],
                TOOL,
            )
        });
        // chpasswd reads `name:password` from its standard input only: the
        // password is in no argument and in no log.
        // Built in a buffer of exactly its length, so that it never grows
        // (a `format!` reallocates and leaves the earlier copies, unwiped, in
        // freed heap).
        let mut password_line = Zeroizing::new(Vec::with_capacity(name.len() + password.len() + 2));
        password_line.extend_from_slice(name.as_bytes());
        password_line.push(b':');
        password_line.extend_from_slice(password.as_bytes());
        password_line.push(b'\n');
        let set = added.and_then(|_| {
            let out = self.tool(
                &mut log,
                &Job {
                    program: "chpasswd",
                    args: &["--root", &root],
                    env: &[],
                    stdin: Some(password_line.as_slice()),
                    inherit: &[],
                    timeout: TOOL,
                },
                &mut || {},
            )?;
            ensure!(
                out.exit.success(),
                "chpasswd {}: {}",
                out.exit.describe(),
                last_line(&out.lines)
            );
            Ok(())
        });
        drop(password_line);
        // The mounts end whatever happened above.
        let proc_unmounted = if proc_is_mounted(&proc) {
            self.checked(&mut log, "umount", &[&proc.to_string_lossy()], TOOL)
                .map(|_| ())
        } else {
            Ok(())
        };
        let unmounted = self.checked(&mut log, "umount", &[&var.to_string_lossy()], TOOL);
        set?;
        proc_unmounted?;
        unmounted?;
        // The account exists, with a password hash, in the wheel group.
        let etc = deployment.path.join("etc");
        let passwd = fs::read_to_string(etc.join("passwd"))?;
        ensure!(
            passwd.lines().any(|l| l.starts_with(&format!("{name}:"))),
            "{name} is not in /etc/passwd"
        );
        let shadow = fs::read_to_string(etc.join("shadow"))?;
        ensure!(
            shadow
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{name}:")))
                .and_then(|rest| rest.split(':').next())
                .is_some_and(|hash| hash.starts_with('$')),
            "{name} has no password hash in /etc/shadow"
        );
        let group = fs::read_to_string(etc.join("group"))?;
        ensure!(
            group.lines().any(|l| {
                l.starts_with("wheel:")
                    && l.rsplit(':')
                        .next()
                        .is_some_and(|m| m.split(',').any(|u| u == name))
            }),
            "{name} is not in the wheel group"
        );
        self.relabel(&mut log, deployment, &["var/home"])?;
        self.end(Stage::Account, &mut log);
        Ok(())
    }

    /// Label what the helper wrote after `bootc` labeled the image, with the
    /// deployment's own file contexts (`bootc` labels what it installs). The
    /// files in the deployment's `/etc` are looked up as `/etc/...` by taking
    /// the deployment's path off the name (`setfiles -r`); the home directory
    /// and the logs are in the stateroot's `var`, which is `/var` on the
    /// machine, so they are looked up with the stateroot's path taken off.
    fn relabel(
        &self,
        log: &mut StageLog,
        deployment: &target::Deployment,
        var: &[&str],
    ) -> Result<()> {
        let contexts = deployment
            .path
            .join("etc/selinux/targeted/contexts/files/file_contexts");
        if !contexts.is_file() {
            log.line("the image has no SELinux file contexts: nothing to relabel");
            return Ok(());
        }
        let contexts = contexts.to_string_lossy().into_owned();
        // The contexts are validated against the target's own policy: the live
        // system's loaded one can be older and lack types the target has (the
        // flatpak helper's, measured: `setfiles` then fails on the whole file
        // with "context ... is invalid" and "Invalid argument").
        let policy = target::selinux_policy_file(&deployment.path)?
            .to_string_lossy()
            .into_owned();
        let stateroot = deployment
            .var
            .parent()
            .context("the stateroot's var has no parent")?
            .to_path_buf();
        let existing = |base: &Path, relative: &[&str]| -> Vec<String> {
            relative
                .iter()
                .map(|r| base.join(r))
                .filter(|p| fs::symlink_metadata(p).is_ok())
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        };
        // All of the deployment's `/etc`, not the files the helper knows it
        // wrote: the tools it runs write more (`useradd` leaves `/etc/.pwd.lock`,
        // which `systemd`'s dynamic users and GDM then cannot open if it is
        // `etc_t`: `rpm-ostreed.service` failed with status 217/USER on the
        // first boot of an installation that labeled only a list of names).
        let etc = existing(&deployment.path, &["etc"]);
        let var = existing(&stateroot, var);
        for (root, paths) in [(&deployment.path, etc), (&stateroot, var)] {
            if paths.is_empty() {
                continue;
            }
            let root = root.to_string_lossy().into_owned();
            let mut args = vec![
                "-c",
                policy.as_str(),
                "-F",
                "-v",
                "-r",
                root.as_str(),
                contexts.as_str(),
            ];
            args.extend(paths.iter().map(String::as_str));
            self.checked(log, "setfiles", &args, SETFILES)?;
        }
        Ok(())
    }

    // --- recovery key and security chip ---------------------------------------

    /// Open `ilex-enroll`'s view of the new volume. A failure is not the
    /// installation's: the disk is encrypted and opens with its passphrase, so
    /// the installation goes on without a recovery key and says why.
    fn prepare_enrolment(&mut self) -> Option<enrol::Prepared> {
        let mut log = self.logs.stage(Stage::Recovery).ok()?;
        let result = (|| -> Result<enrol::Prepared> {
            let volume = self
                .volume
                .as_ref()
                .context("the encrypted volume is not known")?;
            let passphrase = self
                .passphrase
                .as_ref()
                .context("the passphrase is not available for the recovery key")?;
            let scratch = self
                .helper
                .config
                .run_dir
                .join("enroll")
                .join(&self.plan.id);
            let _ = fs::remove_dir_all(&scratch);
            let prepared = enrol::Prepared::new(
                &volume.uuid,
                passphrase,
                &scratch,
                self.plan.encryption.tpm.will_enroll,
            )?;
            prepared.preflight()?;
            Ok(prepared)
        })();
        match result {
            Ok(prepared) => {
                log.line("the volume is ready for a recovery key");
                Some(prepared)
            }
            Err(error) => {
                log.line(&format!("no recovery key can be made: {error:#}"));
                None
            }
        }
    }

    /// Add the recovery key to the volume and offer it to the interface. Not
    /// in the log, the plan or the state: only that it was made.
    fn make_recovery_key<'a>(&mut self, prepared: &'a enrol::Prepared) -> Option<Enrolment<'a>> {
        let mut log = self.logs.stage(Stage::Recovery).ok()?;
        let result = (|| -> Result<Enrolment<'a>> {
            let mut session = prepared.session()?;
            let (entry, key) = session.recovery(false)?;
            let key = secret::Secret::from_bytes(key.bytes())?;
            ensure!(
                std::str::from_utf8(key.expose()).is_ok_and(recovery::is_key),
                "the recovery key is not in the form this installer shows"
            );
            // What the person types back must open the key's own keyslot (phase
            // 0's `authenticate_slot` after its comparison): the check runs on
            // the bus thread that takes the typed key, through the volume's one
            // `Device`.
            let device = Arc::clone(&prepared.device);
            let slot = entry.slot;
            let verify: Verifier = Box::new(move |typed| {
                device.authenticate_slot(ilex_enroll::secret_fd(typed.expose())?, slot)
            });
            self.helper.recovery.offer(&self.plan.id, key, verify);
            self.helper
                .tracker
                .set_security(|security| security.recovery_key = Some(KeyStatus::Waiting));
            Ok(Enrolment { session, entry })
        })();
        match result {
            Ok(enrolment) => {
                log.line(&format!(
                    "added a recovery key (token {}, keyslot {}); it is shown to the person while the image downloads",
                    enrolment.entry.token, enrolment.entry.slot
                ));
                Some(enrolment)
            }
            Err(error) => {
                log.line(&format!("no recovery key was made: {error:#}"));
                None
            }
        }
    }

    /// After the account: wait for the person to type the whole key back (or
    /// to go on without it), then enrol the security chip, and write what happened where the
    /// installed system reads it (`setup.json` and `enrollment.json`). Neither
    /// step fails the installation: the system is installed and opens with its
    /// passphrase, and the outcome says so.
    fn recovery_and_chip(
        &mut self,
        deployment: &target::Deployment,
        prepared: Option<&enrol::Prepared>,
        mut enrolment: Option<&mut Enrolment<'_>>,
    ) -> Result<()> {
        if !self.plan.encryption.enabled {
            return Ok(());
        }
        let planned = self.plan.encryption.tpm.clone();
        let mut outcome = TpmOutcome {
            status: TpmStatus::PassphraseOnly,
            reason: planned.reason.clone(),
        };
        if self.plan.encryption.recovery_key {
            let mut log = self.begin(
                Stage::Recovery,
                "Waiting for you to confirm the recovery key",
            )?;
            let confirmed = match enrolment.as_deref_mut() {
                Some(e) => {
                    log.line("waiting for the whole recovery key to be typed back");
                    match self.helper.recovery.wait_acknowledged(&mut || {}) {
                        Wait::Acknowledged => match e.session.acknowledge(e.entry) {
                            Ok(()) => {
                                log.line("the recovery key is confirmed and recorded");
                                true
                            }
                            Err(error) => {
                                log.line(&format!("the confirmation was not recorded: {error:#}"));
                                outcome.reason = "enrollment-failed".into();
                                false
                            }
                        },
                        // Phase 0's record of a key that was made and never
                        // confirmed: `recovery` unacknowledged and the attempt
                        // kept (`record_enrolment` writes it), so that `ilex-enroll
                        // complete --reissue-recovery` on the installed system
                        // reconciles that slot and makes a key to replace it.
                        Wait::Declined => {
                            log.line(&format!(
                                "the person went on without a recovery key: token {} and keyslot {} were never confirmed and stay recorded for `ilex-enroll complete --reissue-recovery`",
                                e.entry.token, e.entry.slot
                            ));
                            outcome.reason = "recovery-key-failed".into();
                            false
                        }
                        Wait::Abandoned => {
                            log.line("the confirmation was abandoned");
                            outcome.reason = "enrollment-failed".into();
                            false
                        }
                    }
                }
                None => {
                    log.line("there is no recovery key to confirm");
                    outcome.reason = "recovery-key-failed".into();
                    false
                }
            };
            self.end(Stage::Recovery, &mut log);
            // The security chip, when the plan said it would be enrolled.
            let capability = ilex_enroll::eligibility(&deployment.path);
            if planned.will_enroll {
                let mut log = self.begin(Stage::Tpm, "Setting up automatic unlock")?;
                if let (true, Some(e)) = (confirmed, enrolment.as_deref_mut()) {
                    if capability.2 == "pcr7-sha256" {
                        match e.session.enroll_tpm(false) {
                            Ok(entry) => {
                                log.line(&format!(
                                    "enrolled the security chip (token {}, keyslot {}) to PCR 7 of the SHA-256 bank, no PIN",
                                    entry.token, entry.slot
                                ));
                                outcome = TpmOutcome {
                                    status: TpmStatus::EnrolledUnverified,
                                    reason: capability.2.to_owned(),
                                };
                            }
                            Err(error) => {
                                log.line(&format!("the security chip was not enrolled: {error:#}"));
                                outcome.reason = "enrollment-failed".into();
                            }
                        }
                    } else {
                        log.line(&format!(
                            "the security chip is not enrolled: {}",
                            capability.2
                        ));
                        outcome.reason = capability.2.to_owned();
                    }
                } else if !confirmed {
                    log.line("the security chip is not enrolled without a confirmed recovery key");
                }
                self.end(Stage::Tpm, &mut log);
            }
            if let (Some(e), Some(p)) = (enrolment, prepared) {
                self.record_enrolment(deployment, p, e, &outcome, &capability)?;
            }
        }
        self.helper
            .tracker
            .set_security(|security| security.tpm = Some(outcome.clone()));
        // `setup.json`, as phase 0 keeps it: what was asked for, what this
        // computer could do and what the installation did.
        let verdict = ilex_enroll::probe().verdict;
        let setup = deployment.path.join("etc/ilex/setup.json");
        write_file(
            &setup,
            target::setup_record(&self.plan, Some((verdict, &outcome))).as_bytes(),
            0o644,
        )?;
        Ok(())
    }

    /// The record the installed system reads (`ilex-security-verify`,
    /// `ilexctl security repair`): `enrollment.json` in the new `/var`, the
    /// volume's `crypttab` line with the security chip's option.
    fn record_enrolment(
        &self,
        deployment: &target::Deployment,
        prepared: &enrol::Prepared,
        enrolment: &mut Enrolment<'_>,
        outcome: &TpmOutcome,
        capability: &(
            ilex_enroll::Probe,
            std::collections::BTreeMap<String, String>,
            &'static str,
        ),
    ) -> Result<()> {
        let mut log = self.logs.stage(Stage::Recovery)?;
        let value = &mut enrolment.session.state.value;
        value["probe"] = serde_json::to_value(&capability.0)?;
        value["target_tools"] = serde_json::to_value(&capability.1)?;
        value["reason"] = serde_json::json!(outcome.reason);
        if outcome.status != TpmStatus::EnrolledUnverified {
            value["state"] = serde_json::json!("unavailable");
        }
        let lib = deployment.var.join("lib/ilex");
        make_dir_mode(&deployment.var.join("lib"), 0o755)?;
        make_dir_mode(&lib, 0o755)?;
        let record = lib.join("security/enrollment.json");
        ilex_enroll::write_state(&record, &enrolment.session.state.value)
            .context("writing the enrolment record into the new /var")?;
        log.line("the enrolment record is in the new system's /var/lib/ilex/security");
        // The line `ilex-enroll`'s own completion writes for the root volume.
        match ilex_enroll::local::configure_crypttab(&deployment.path, prepared.uuid()) {
            Ok(()) => log.line("/etc/crypttab names the security chip for the root volume"),
            Err(error) => log.line(&format!("/etc/crypttab was not changed: {error:#}")),
        }
        let _ = fs::remove_dir_all(&prepared.scratch);
        Ok(())
    }

    // --- finalise --------------------------------------------------------

    fn finalise(&mut self, deployment: &target::Deployment) -> Result<()> {
        let mut log = self.begin(Stage::Finalise, "Finishing and unmounting")?;
        self.copy_logs(&mut log, deployment);
        self.stamp(&mut log, deployment)?;
        self.relabel(&mut log, deployment, &["var/log", "var/lib/ilex"])?;
        self.checked(&mut log, "sync", &[], TOOL)?;
        // Free space the new file systems can discard; not every device
        // supports it, and none of it matters for the install.
        for dir in [
            self.target.join("boot/efi"),
            self.target.join("boot"),
            self.target.clone(),
        ] {
            let _ = self.tool(
                &mut log,
                &Job {
                    program: "fstrim",
                    args: &["--verbose", &dir.to_string_lossy()],
                    env: &[],
                    stdin: None,
                    inherit: &[],
                    timeout: Duration::from_secs(300),
                },
                &mut || {},
            );
        }
        // The directories the helper made as mount points are in the file
        // systems below them, unlabeled until labeled: `boot/efi` in the /boot
        // partition and `boot` in the root (systemd-logind could not look at
        // `/boot/efi`: `unlabeled_t`). Each is labeled once what is mounted on
        // it is gone, so that only the directory is touched.
        for point in ["boot/efi", "boot"] {
            let at = self.target.join(point);
            self.checked(&mut log, "umount", &[&at.to_string_lossy()], TOOL)?;
            self.relabel_mount_point(&mut log, deployment, &at)?;
        }
        ensure!(
            self.unmount(&mut log),
            "the target file systems could not be unmounted"
        );
        // The opened volume is the last thing of the target: closed, the disk
        // holds the encrypted root and nothing of it is readable.
        self.close_volume(&mut log)?;
        self.end(Stage::Finalise, &mut log);
        Ok(())
    }

    /// The digests this installation carries into its stamp, apart from its own:
    /// what the disk's stamp held before it was wiped, from this attempt and from
    /// any earlier attempt of this boot that was wiped and failed (the file
    /// [`stamp::EARLIER`], kept just before each wipe: the disk of a retry holds
    /// nothing of it), and what answers mode saw in this boot and handed over
    /// (the file [`stamp::SEEN`]: answers that did not install and must not become
    /// new answers because the person installed by hand). A record that cannot be
    /// read is not carried, and the log says so.
    fn carried_and_seen(&self, log: &mut StageLog) -> (Vec<String>, Vec<String>) {
        let kept = match stamp::read_record(&self.helper.config.earlier_file) {
            Ok(kept) => kept,
            Err(why) => {
                log.line(&format!(
                    "the record of the stamps this boot's installations wiped could not be read ({why:#}), so it is not carried"
                ));
                Vec::new()
            }
        };
        // What the first attempt kept is older than what this one read.
        let carried = stamp::union(&kept, &self.earlier.digests);
        let seen = match stamp::read_record(&self.helper.config.seen_file) {
            Ok(seen) => seen,
            Err(why) => {
                log.line(&format!(
                    "the record of the answers seen in this boot could not be read ({why:#}), so it is not carried"
                ));
                Vec::new()
            }
        };
        (carried, seen)
    }

    /// Leave the provisional stamp on the new `/boot` ([`stamp::provisional`]):
    /// the digests that must not erase whatever this boot installs, without this
    /// request's own unless an earlier stamp held it, written as soon as the
    /// partition is mounted and before the image is pulled, then checkpointed
    /// ([`Install::checkpoint_boot`]). It is what a reboot finds if the
    /// installation does not finish: the disk's old digests and the refused
    /// answers stay refused, and these answers, being the request being
    /// installed, stay retryable (unless the disk's stamp held their digest, as
    /// when a person's plan equals answers refused for this disk). No digest to
    /// keep writes nothing. The finalise stage overwrites it ([`Install::stamp`]).
    ///
    /// A failure to write it ends the installation: the disk is already wiped, and
    /// going on without the protection against erasing an installation is worse
    /// than a message.
    fn provisional_stamp(&mut self, log: &mut StageLog) -> Result<()> {
        let (carried, seen) = self.carried_and_seen(log);
        let own = target::request_digest(&self.plan.request);
        let digests = stamp::provisional(&carried, &seen, &own);
        let file = self.target.join("boot").join(target::REQUEST_STAMP);
        let written = stamp::write_provisional(&file, &digests).with_context(|| {
            format!(
                "keeping the digests of the earlier stamps on the new /boot ({})",
                target::REQUEST_STAMP
            )
        })?;
        if written {
            self.checkpoint_boot(log)?;
            self.provisional = Some(digests.clone());
            log.line(&format!(
                "recorded {} digest{} in the provisional /boot/{} (the earlier stamps' and the answers seen in this boot; this request's own only where an earlier stamp held it), so that a reboot before the image is in does not lose them",
                digests.len(),
                if digests.len() == 1 { "" } else { "s" },
                target::REQUEST_STAMP
            ));
        } else {
            log.line("no digest to keep on the new /boot yet: no provisional stamp is written");
        }
        Ok(())
    }

    /// Checkpoint the journal of the new `/boot` ([`stamp::checkpoint`]), as the
    /// last thing after a stamp is written to it: answers mode reads the stamp with
    /// `ro,noload`, and `syncfs` leaves the stamp's directory entry and inode in
    /// the journal and, on a device with a volatile write cache, not necessarily in
    /// their places. An error ends the installation, like a stamp that cannot be
    /// written: going on would leave a wiped disk whose protection against
    /// erasing an installation may not survive a power cut.
    fn checkpoint_boot(&self, log: &mut StageLog) -> Result<()> {
        let boot = self.target.join("boot");
        stamp::checkpoint(&boot).with_context(|| {
            format!(
                "making the stamp ({}) on the new /boot readable without a journal replay",
                target::REQUEST_STAMP
            )
        })?;
        log.line(&format!(
            "checkpointed the journal of the new /boot (EXT4_IOC_CHECKPOINT): /boot/{} is in its place on the device, where `ro,noload` finds it after a power cut",
            target::REQUEST_STAMP
        ));
        Ok(())
    }

    /// `bootc` has run over the `/boot` the provisional stamp is on: the file
    /// must be what was written. A later stage that fails leaves whatever is
    /// there, and no stamp would be no protection.
    fn provisional_intact(&self, log: &mut StageLog) -> Result<()> {
        let Some(digests) = &self.provisional else {
            return Ok(());
        };
        let file = self.target.join("boot").join(target::REQUEST_STAMP);
        let now = fs::read_to_string(&file)
            .with_context(|| format!("bootc ran, and {} cannot be read", file.display()))?;
        ensure!(
            stamp::parse(&now).is_ok_and(|held| held == *digests),
            "bootc changed the provisional stamp on the new /boot ({}): it holds {:?}",
            target::REQUEST_STAMP,
            now
        );
        log.line(&format!(
            "the provisional stamp on the new /boot is as it was written after bootc ({} digest{})",
            digests.len(),
            if digests.len() == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    /// Leave the digests of the requests that must not erase this installation
    /// on the new `/boot` ([`target::REQUEST_STAMP`], one per line), where
    /// unattended mode finds them (read-only) on a later boot of the installer.
    /// They are what [`Install::carried_and_seen`] gathers, then this
    /// installation's own request. Written last-but-one, once the image and the
    /// account are in place, as a new file renamed over the provisional stamp
    /// ([`Install::provisional_stamp`]) the mount stage left (a reset leaves one
    /// or the other, never an empty file), labelled and checkpointed: an
    /// installation that did not get this far has that stamp, which does not hold
    /// its own digest unless an earlier stamp did, and may be run again.
    fn stamp(&self, log: &mut StageLog, deployment: &target::Deployment) -> Result<()> {
        let file = self.target.join("boot").join(target::REQUEST_STAMP);
        let own = target::request_digest(&self.plan.request);
        if let Some(why) = &self.earlier.unreadable {
            // The person confirmed the erase of this disk: what could not be
            // read is not carried, and the log says so.
            log.line(&format!(
                "the disk's earlier stamp could not be read before it was wiped, so its digests are not carried: {why}"
            ));
        }
        let (carried, seen) = self.carried_and_seen(log);
        let digests = stamp::merge(&carried, &seen, &own);
        // A new file renamed over the provisional one (never truncated in
        // place: a reset would leave it empty), labelled, and only then
        // checkpointed, since the label is a journalled change of the inode too.
        stamp::write_finished(&file, &digests)?;
        self.relabel_mount_point(log, deployment, &file)?;
        self.checkpoint_boot(log)?;
        log.line(&format!(
            "recorded {} digest{} in /boot/{} (this request's, {} carried from the disk's earlier stamps, {} of answers seen in this boot)",
            digests.len(),
            if digests.len() == 1 { "" } else { "s" },
            target::REQUEST_STAMP,
            carried.len(),
            seen.len()
        ));
        Ok(())
    }

    /// Label one directory of the target (a mount point whose file system has
    /// just been unmounted) with the deployment's contexts, the target being
    /// the root of the lookup.
    fn relabel_mount_point(
        &self,
        log: &mut StageLog,
        deployment: &target::Deployment,
        at: &Path,
    ) -> Result<()> {
        let contexts = deployment
            .path
            .join("etc/selinux/targeted/contexts/files/file_contexts");
        if !contexts.is_file() {
            return Ok(());
        }
        let policy = target::selinux_policy_file(&deployment.path)?;
        let (policy, contexts, root, at) = (
            policy.to_string_lossy().into_owned(),
            contexts.to_string_lossy().into_owned(),
            self.target.to_string_lossy().into_owned(),
            at.to_string_lossy().into_owned(),
        );
        self.checked(
            log,
            "setfiles",
            &["-c", &policy, "-F", "-v", "-r", &root, &contexts, &at],
            SETFILES,
        )?;
        Ok(())
    }

    /// The logs and the plan (no secrets: the plan names the administrator,
    /// never the password) into the new system's `/var/log/ilex-installer/`.
    fn copy_logs(&self, log: &mut StageLog, deployment: &target::Deployment) {
        let dest = deployment.var.join("log/ilex-installer");
        let result = (|| -> Result<()> {
            fs::create_dir_all(&dest)?;
            fs::set_permissions(&dest, fs::Permissions::from_mode(0o755))?;
            for entry in fs::read_dir(self.logs.dir())? {
                let entry = entry?;
                fs::copy(entry.path(), dest.join(entry.file_name()))?;
            }
            fs::write(
                dest.join("plan.json"),
                serde_json::to_vec_pretty(&self.plan)?,
            )?;
            Ok(())
        })();
        match result {
            Ok(()) => log.line(&format!("logs copied to {}", dest.display())),
            Err(e) => log.line(&format!("the logs were not copied: {e:#}")),
        }
    }

    /// Unmount everything of the target, deepest first, trying again while a
    /// mount is busy. True when none is left.
    fn unmount(&self, log: &mut StageLog) -> bool {
        for attempt in 0..6 {
            let Ok(mounts) = target::current_mounts() else {
                return false;
            };
            let left = target::leftover_mounts(&mounts, &self.target, &self.nodes);
            if left.is_empty() {
                return true;
            }
            for point in left {
                log.command("umount", &[&point]);
                if let Ok(out) = exec::run("umount", &[point.as_str()], TOOL)
                    && !out.success()
                {
                    log.line(&format!("umount {point}: {}", out.stderr));
                }
            }
            if attempt < 5 {
                thread::sleep(Duration::from_secs(1));
            }
        }
        target::current_mounts().is_ok_and(|mounts| {
            target::leftover_mounts(&mounts, &self.target, &self.nodes).is_empty()
        })
    }

    /// After a failure: leave nothing mounted.
    fn cleanup(&mut self) {
        if let Ok(mut log) = self.logs.stage(self.stage) {
            log.line("cleaning up after a failure");
            let ended = self.unmount(&mut log);
            log.line(if ended {
                "nothing of the target is mounted"
            } else {
                "some mounts of the target could not be removed"
            });
            if ended && let Err(why) = self.close_volume(&mut log) {
                log.line(&format!(
                    "the encrypted volume could not be closed: {why:#}"
                ));
            }
        }
    }
}

/// Is something mounted at `path`?
fn proc_is_mounted(path: &Path) -> bool {
    target::current_mounts()
        .is_ok_and(|mounts| mounts.iter().any(|m| Path::new(&m.mount_point) == path))
}

fn make_dir(path: &Path) -> Result<()> {
    make_dir_mode(path, 0o700)
}

fn make_dir_mode(path: &Path, mode: u32) -> Result<()> {
    match fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(anyhow!(e)).with_context(|| format!("creating {}", path.display())),
    }
}

pub(crate) fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
