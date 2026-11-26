//! The installation itself: the stages that write the disk, run by `Apply` on a
//! thread of their own (plan 9.1, unencrypted whole disk):
//!
//! partition → mount → image → configure → account → finalise
//!
//! Each stage is recorded before it writes anything ([`Tracker`]), logs what
//! its tools print under `/run/ilex-installer/log/<plan>/`, and fails with a
//! sentence that names the stage and the tool. A failure after the partition
//! stage leaves the disk changed and says so; there is no resuming, and the
//! next attempt starts again from a fresh plan.

use crate::{
    disks::{self, Hold},
    exec::{self, Exit, Flow, Job},
    helper::Helper,
    repart,
    secret::{self, Secret},
    stagelog::{Logs, StageLog},
    target,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use ilex_installer_model::{
    plan::{Plan, PlannedPartition},
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

/// The stages an installation goes through. (Encryption, the recovery key and
/// the TPM add theirs in later changes; `Apply` refuses an encrypted plan
/// until then.)
pub fn stages() -> Vec<Stage> {
    Stage::UNENCRYPTED.to_vec()
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
    /// The stage that is running (or failed).
    stage: Stage,
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
pub fn run(helper: Arc<Helper>, plan: Plan, hold: Hold, address: String, admin: Secret) {
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
    let target = helper.config.run_dir.join("target");
    let mut install = Install {
        helper,
        plan,
        logs,
        target,
        address,
        nodes: BTreeSet::new(),
        admin,
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
        self.partition(hold)?;
        self.mount()?;
        self.image()?;
        let deployment =
            target::find_deployment(&self.target).context("looking for what bootc deployed")?;
        self.configure(&deployment)?;
        self.account(&deployment)?;
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
        let out = self.tool(
            log,
            &Job {
                program,
                args,
                env: &[("HOME", "/root"), ("TMPDIR", "/var/tmp")],
                stdin: None,
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
        let lines = self.checked(
            &mut log,
            "systemd-repart",
            &[
                "--dry-run=no",
                "--empty=force",
                "--pretty=no",
                "--no-pager",
                "--json=pretty",
                &definitions,
                &seed,
                &disk,
            ],
            REPART,
        )?;
        let made = repart::parse(&json_array(&lines)?, &layout)
            .context("reading what systemd-repart made")?;
        ensure!(
            repart::same_partitions(&made, &self.plan.partitions),
            "the partitions systemd-repart made are not the planned ones; the disk was erased, install again from a new plan"
        );
        self.end(Stage::Partition, &mut log);
        Ok(())
    }

    // --- mount -----------------------------------------------------------

    fn mount(&mut self) -> Result<()> {
        let mut log = self.begin(Stage::Mount, "Mounting the new partitions")?;
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
        // before they are mounted.
        let _ = self.tool(
            &mut log,
            &Job {
                program: "udevadm",
                args: &["settle", "--timeout=30"],
                env: &[],
                stdin: None,
                timeout: Duration::from_secs(40),
            },
            &mut || {},
        );
        let by_kind = |prefix: &str| -> Result<&(PlannedPartition, String)> {
            found
                .iter()
                .find(|(p, _)| p.kind.starts_with(prefix))
                .with_context(|| format!("no {prefix} partition was made"))
        };
        let (root, root_node) = by_kind("root")?;
        let (boot, boot_node) = by_kind("xbootldr")?;
        let (esp, esp_node) = by_kind("esp")?;
        make_dir(&self.target)?;
        let target = self.target.to_string_lossy().into_owned();
        self.checked(
            &mut log,
            "mount",
            &["-t", &root.filesystem, root_node, &target],
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
        self.end(Stage::Mount, &mut log);
        Ok(())
    }

    // --- image -----------------------------------------------------------

    fn image(&mut self) -> Result<()> {
        let mut log = self.begin(Stage::Image, "Downloading and installing ilex")?;
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
        let out = self.tool(
            &mut log,
            &Job {
                program: "bootc",
                args: &[
                    "install",
                    "to-filesystem",
                    "--source-imgref",
                    &source,
                    "--target-imgref",
                    &self.plan.image.target,
                    "--enforce-container-sigpolicy",
                    "--skip-finalize",
                    &target,
                ],
                env: &[("HOME", "/root"), ("TMPDIR", "/var/tmp")],
                stdin: None,
                timeout: BOOTC,
            },
            &mut tick,
        )?;
        if !out.exit.success() {
            bail!("bootc {}: {}", out.exit.describe(), last_line(&out.lines));
        }
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
            target::setup_record(&self.plan).as_bytes(),
            0o644,
        )?;
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
        let password_line = Zeroizing::new(format!("{name}:{password}\n"));
        let set = added.and_then(|_| {
            let out = self.tool(
                &mut log,
                &Job {
                    program: "chpasswd",
                    args: &["--root", &root],
                    env: &[],
                    stdin: Some(password_line.as_bytes()),
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

    // --- finalise --------------------------------------------------------

    fn finalise(&mut self, deployment: &target::Deployment) -> Result<()> {
        let mut log = self.begin(Stage::Finalise, "Finishing and unmounting")?;
        self.copy_logs(&mut log, deployment);
        self.relabel(&mut log, deployment, &["var/log"])?;
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
        self.end(Stage::Finalise, &mut log);
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

fn write_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
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
