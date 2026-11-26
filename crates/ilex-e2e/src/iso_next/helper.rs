//! `iso::next_helper_usb` and `iso::next_helper_cdrom`: the root helper on the
//! real live ISO. The ISO is written to a stick (a removable USB disk, the way
//! it is used) or attached as an optical drive, beside three blank virtual
//! disks; the guest boots it through UEFI with Secure Boot, and the helper
//! answers `Probe` and `Plan` to `ilex-installer` over the system bus.
//!
//! This is the one place the helper meets what the `installer::` tier cannot
//! show: dracut's real layout of the live medium (a device-mapper `live-rw`
//! snapshot over loop devices, or an overlay directly over the squashfs), the
//! SELinux labels of the shipped files, and the session user's access to the
//! bus. The tier records that layout in its artifacts (`losetup`, every mount
//! table, the device-mapper tree) and asserts what the installer depends on:
//!
//! * the boot medium is `live_medium` (a stick; an optical drive is not a
//!   candidate disk at all) and is never a target,
//! * every blank disk is offered, with no reason and no note,
//! * `Plan` on a blank disk succeeds (it only reads and dry-runs: the disks
//!   are looked at before and after and are still blank, with no partition
//!   table and no partition), and is refused on the stick and on a disk that is
//!   too small,
//! * `ilex-installerd --self-check` passes on the ISO's own files,
//! * no SELinux denial was enforced (the ISO boots permissive today; the ones
//!   that name the helper are counted and reported).
//!
//! A third trial, `iso::next_helper_loop` (named explicitly, never run by a
//! broader filter), boots the same ISO as a *file* on another disk, the way
//! GRUB's loopback with `iso-scan/filename=` does (see `loop_file.rs`): the
//! disk holding the file is the live medium, and the blank disks are offered.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::core::{Capability, Requirement, SecurityLaunch, VmProvider};
use ilex_devkit::installer_model::{
    plan::{ErrorCode, Plan, PlanError},
    probe::{Disk, Probe},
};
use ilex_devkit::provider::LinuxProvider;
use ilex_devkit::qemu::{self, Vm, VmConfig};
use serde_json::{Value, json};

use super::{
    Guest, MEMORY, PAGE_TIMEOUT, blank_disk_of, diagnostics, field, fields, iso_path, loop_file,
    wait_for_marker,
};
use crate::common;

const BUS: &str = "org.ilex.Installer1";
const GIB: u64 = 1 << 30;
/// The VM's own disk, the second and the small one: virtio names them `vda`,
/// `vdb` and `vdc`.
const FIRST_BYTES: u64 = 40 * GIB;
const SECOND_BYTES: u64 = 41 * GIB;
const SMALL_BYTES: u64 = 2 * GIB;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Medium {
    /// The ISO as a removable USB disk: `/dev/sda` with its partitions.
    Usb,
    /// The ISO as an optical drive: `/dev/sr0`, which is no candidate disk.
    Cdrom,
    /// The ISO as a file on another disk (dracut's `iso-scan`, Ventoy): the
    /// disk that holds the file is the medium, through the loop device.
    Loop,
}

impl Medium {
    fn name(self) -> &'static str {
        match self {
            Medium::Usb => "usb",
            Medium::Cdrom => "cdrom",
            Medium::Loop => "loop",
        }
    }
    /// VM purposes stay short: they name Unix socket paths.
    fn purpose(self) -> &'static str {
        match self {
            Medium::Usb => "iso-next-usb",
            Medium::Cdrom => "iso-next-cd",
            Medium::Loop => "iso-next-loop",
        }
    }
}

/// A sparse raw disk, attached after the VM's own.
fn raw_disk(path: &Path, bytes: u64) -> Result<()> {
    let _ = fs::remove_file(path);
    fs::File::create(path)
        .and_then(|file| file.set_len(bytes))
        .with_context(|| format!("creating {}", path.display()))
}

pub(super) fn run(medium: Medium) -> Result<()> {
    let ws = common::workspace()?;
    let iso = iso_path()?;
    let artifacts = qemu::artifact_dir(&ws, "iso-next")?;
    let root = ws
        .state_dir()
        .join("iso-next-fixtures")
        .join(medium.purpose());
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    let second = root.join("second.raw");
    raw_disk(&second, SECOND_BYTES)?;
    let small = root.join("small.raw");
    raw_disk(&small, SMALL_BYTES)?;
    let memory = std::env::var("ILEX_ISO_NEXT_MEMORY").unwrap_or_else(|_| MEMORY.into());
    let mut provider = LinuxProvider::new(ws.clone())?.with_usb_medium(medium == Medium::Usb);
    let mut config = match medium {
        Medium::Usb | Medium::Cdrom => {
            let first = root.join("first.qcow2");
            blank_disk_of(&first, "40G")?;
            let mut config = VmConfig::new(medium.purpose(), &first);
            config.cdrom = Some(iso);
            config.extra_disks = vec![second, small];
            config
        }
        Medium::Loop => {
            // The ISO is a file on the VM's own disk; its boot loader is a
            // small FAT disk of its own, attached last.
            let fixture = loop_file::build(&root, &iso)?;
            save(
                &artifacts,
                medium,
                "kernel-arguments.txt",
                &format!(
                    "the ISO's own: {}\nfrom a file: {}\n",
                    fixture.original_args, fixture.args
                ),
            )?;
            provider = provider.with_esp_dir(&fixture.esp);
            let blank = root.join("blank.raw");
            raw_disk(&blank, FIRST_BYTES)?;
            let mut config = VmConfig::new(medium.purpose(), &fixture.disk);
            config.extra_disks = vec![blank, second, small];
            config
        }
    };
    config.snapshot = false;
    config.memory = memory;
    let mut spec = ilex_devkit::provider::spec(&ws, &config)?;
    // Provisioning is required, a qualified host is not (see `boot`).
    spec.security_launch = SecurityLaunch::QualificationFixture;
    spec.secure_boot = Requirement::Required;
    let caps = provider.probe(&spec)?;
    fs::write(
        artifacts.join(format!("next-helper-{}-backend.json", medium.name())),
        serde_json::to_vec_pretty(&caps)?,
    )?;
    caps.require_provisioning(Capability::FirmwareEnforcement)?;
    let vm = provider.create(spec)?;
    let mut guest = Guest { provider, vm, root };
    guest.provider.start(&mut guest.vm)?;
    let vm = guest.vm.running_mut()?;
    ensure!(
        vm.info.secure_boot,
        "the VM was not started with Secure Boot"
    );
    let outcome = observe(vm, medium, &artifacts);
    if outcome.is_err() {
        diagnostics(vm, &artifacts, &format!("helper-{}", medium.name()));
    }
    outcome
}

/// The guest's own account of what it booted from and what runs the helper.
/// Everything is also saved for the PR: this is the layout the tracer has to
/// follow.
const EVIDENCE: &str = r#"set -u
section() { printf '\n=== %s\n' "$1"; }
section cmdline;            cat /proc/cmdline
section getenforce;         getenforce
section losetup;            losetup --json --list --output NAME,BACK-FILE,BACK-INO,BACK-MAJ:MIN
section "dmsetup tree";     dmsetup ls --tree
section "dmsetup table";    dmsetup table
section "mountinfo of pid 1"; cat /proc/1/mountinfo
section "mountinfo of the shell"; cat /proc/self/mountinfo
section "mountinfo of every process, without repeats"
for p in /proc/[0-9]*; do cat "$p/mountinfo" 2>/dev/null; done | sort -u
section "mount namespaces"; for p in /proc/[0-9]*; do readlink "$p/ns/mnt" 2>/dev/null; done | sort | uniq -c
section lsblk;              lsblk --paths --output NAME,KNAME,MAJ:MIN,TYPE,SIZE,TRAN,RM,RO,FSTYPE,LABEL,MOUNTPOINTS
section "lsblk json";       lsblk --json --bytes --paths --output NAME,KNAME,TYPE,SIZE,TRAN,RM,RO,FSTYPE,LABEL,PKNAME
section swaps;              cat /proc/swaps
section "/run/initramfs";   ls -la /run/initramfs /run/initramfs/isoscan /run/rootfsbase /run/overlayfs /run/ovlwork 2>&1 | head -60
section "loop sysfs"
for d in /sys/block/loop*; do [ -r "$d/loop/backing_file" ] && printf '%s %s\n' "${d##*/}" "$(cat "$d/loop/backing_file")"; done
section "dm sysfs"
for d in /sys/block/dm-*; do printf '%s name=%s slaves=%s\n' "${d##*/}" "$(cat "$d/dm/name" 2>/dev/null)" "$(ls "$d/slaves" 2>/dev/null | tr '\n' ' ')"; done
section "processes (label)"; ps -eo pid,user,label,comm | grep -E 'ilex|dbus-broker|gnome-kiosk|PID' | grep -v grep
section "helper unit";      systemctl show ilex-installerd -p MainPID -p ActiveState -p User -p PrivateMounts -p ExecStart --no-pager
section "files";            ls -lZ /usr/libexec/ilex-installer/ilex-installerd /usr/share/dbus-1/system.d/org.ilex.Installer1.conf /usr/share/dbus-1/system-services/org.ilex.Installer1.service /usr/lib/systemd/system/ilex-installerd.service /usr/share/ilex-installer/ 2>&1
section "installer user";   id ilex-installer
"#;

/// A call to the helper as `user`, in a script for the guest's root shell.
/// The request travels base64 so no quoting depends on its text.
fn busctl(user: &str, method: &str, request: Option<&str>) -> String {
    let argument = request.map_or(String::new(), |r| {
        format!(
            " s \"$(printf %s {} | base64 -d)\"",
            super::base64(r.as_bytes())
        )
    });
    format!(
        "runuser -u {user} -- env SYSTEMD_COLORS=0 SYSTEMD_PAGER= busctl --system --json=short call {BUS} /org/ilex/Installer1 {BUS} {method}{argument}"
    )
}

/// What a script printed and how it ended.
struct Ran {
    code: Option<i32>,
    text: String,
}

fn ssh(vm: &Vm, script: &str) -> Result<Ran> {
    let out = vm.ssh(script)?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(Ran {
        code: out.status.code(),
        text,
    })
}

/// The document a successful `busctl --json=short call` returned.
fn document(ran: &Ran, method: &str) -> Result<Value> {
    ensure!(
        ran.code == Some(0),
        "{method} failed (exit {:?}): {}",
        ran.code,
        ran.text
    );
    let reply: Value = serde_json::from_str(ran.text.trim())
        .with_context(|| format!("busctl's JSON for {method}: {}", ran.text))?;
    ensure!(
        reply["type"] == "s",
        "{method} did not return a string: {reply}"
    );
    serde_json::from_str(reply["data"][0].as_str().context("busctl's data")?)
        .with_context(|| format!("{method}'s document"))
}

/// The refusal a failed call carries: busctl's `Call failed: {...}`.
fn refusal(ran: &Ran) -> Result<PlanError> {
    ensure!(ran.code != Some(0), "the request was not refused");
    let json = ran
        .text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Call failed: "))
        .with_context(|| format!("busctl's error: {}", ran.text))?;
    serde_json::from_str(json).with_context(|| format!("the refusal's document: {json}"))
}

fn save(artifacts: &Path, medium: Medium, name: &str, text: &str) -> Result<()> {
    fs::write(
        artifacts.join(format!("next-helper-{}-{name}", medium.name())),
        text,
    )
    .with_context(|| format!("saving {name}"))
}

/// `Plan` on a disk by its id, as the installer's session user.
fn plan(vm: &Vm, disk: &Disk) -> Result<Ran> {
    let request = json!({
        "composition": "generic.gnome",
        "disk": disk.id,
        "mode": "whole-disk",
        "encrypt": false,
        "administrator": "ilexmaint",
    })
    .to_string();
    ssh(vm, &busctl("ilex-installer", "Plan", Some(&request)))
}

/// A plan, tried again for a minute while the guest has no route to the
/// registry yet (NetworkManager brings the link up after the page is drawn).
fn plan_when_online(vm: &Vm, disk: &Disk) -> Result<Ran> {
    let started = Instant::now();
    loop {
        let ran = plan(vm, disk)?;
        if ran.code == Some(0)
            || !matches!(refusal(&ran), Ok(e) if e.code == ErrorCode::Registry)
            || started.elapsed() > Duration::from_secs(90)
        {
            return Ok(ran);
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// What the three blank disks look like, for comparing before and after a
/// plan: their partition table and signature (none), their partitions (none),
/// and a checksum of the first and last 4 MiB (where an MBR or GPT and its
/// backup live, and the head of any file system). `sfdisk --dump` of a disk
/// with no table fails and prints nothing, which is what blank means.
fn blank_script(names: &[&str]) -> String {
    format!(
        r#"set -u
for n in {}; do
    d=/dev/$n
    [ -b "$d" ] || {{ echo "$n: not a block device"; continue; }}
    printf '%s: pttype=[%s] signature=[%s] partitions=[%s] dump=[%s] head=%s tail=%s\n' "$n" \
        "$(blkid -p -s PTTYPE -o value "$d" 2>/dev/null)" \
        "$(blkid -p -s TYPE -o value "$d" 2>/dev/null)" \
        "$(find /sys/block/$n -mindepth 1 -maxdepth 1 -name "$n[0-9]*" -printf '%f ' 2>/dev/null)" \
        "$(sfdisk --dump "$d" 2>/dev/null)" \
        "$(head -c 4194304 "$d" | sha256sum | cut -c1-16)" \
        "$(dd if="$d" bs=1M skip=$(( $(blockdev --getsz "$d") / 2048 - 4 )) count=4 status=none | sha256sum | cut -c1-16)"
done
"#,
        names.join(" ")
    )
}

/// The blank disks' state, required to be blank: no partition table, no
/// signature, no partition. Returned as printed, to be compared later.
fn blank_state(vm: &Vm, blanks: &[(String, u64); 3]) -> Result<String> {
    let names: Vec<&str> = blanks.iter().map(|(name, _)| name.as_str()).collect();
    let ran = ssh(vm, &blank_script(&names))?;
    ensure!(
        ran.code == Some(0),
        "looking at the blank disks failed: {}",
        ran.text
    );
    for name in names {
        let line = ran
            .text
            .lines()
            .find(|l| l.starts_with(&format!("{name}: ")))
            .with_context(|| format!("no line for {name}: {}", ran.text))?;
        ensure!(
            line.contains("pttype=[]")
                && line.contains("signature=[]")
                && line.contains("partitions=[]")
                && line.contains("dump=[]"),
            "/dev/{name} is not blank (it has a partition table, a signature or partitions): {line}"
        );
    }
    Ok(ran.text)
}

/// The three blank disks the harness attached, by their sizes (the guest's
/// kernel name of each, and its size).
fn blank_disks_of(probe: &Probe) -> Result<[(String, u64); 3]> {
    let find = |bytes: u64| -> Result<(String, u64)> {
        let found: Vec<&Disk> = probe
            .disks
            .iter()
            .filter(|d| d.size_bytes == bytes)
            .collect();
        ensure!(
            found.len() == 1,
            "expected one disk of {bytes} bytes, found {:?}",
            found.iter().map(|d| &d.path).collect::<Vec<_>>()
        );
        let name = found[0]
            .path
            .strip_prefix("/dev/")
            .with_context(|| format!("a disk path {}", found[0].path))?;
        Ok((name.to_owned(), bytes))
    };
    Ok([find(FIRST_BYTES)?, find(SECOND_BYTES)?, find(SMALL_BYTES)?])
}

fn disk_at<'a>(probe: &'a Probe, path: &str) -> Result<&'a Disk> {
    probe
        .disks
        .iter()
        .find(|d| d.path == path)
        .with_context(|| {
            format!(
                "the helper lists no disk {path}: {:?}",
                probe.disks.iter().map(|d| &d.path).collect::<Vec<_>>()
            )
        })
}

/// Offered: nothing uses it, nothing was missed reading it.
fn offered(probe: &Probe, path: &str, bytes: u64) -> Result<()> {
    let disk = disk_at(probe, path)?;
    ensure!(
        !disk.live_medium && disk.in_use.is_empty() && disk.notes.is_empty(),
        "{path} is marked though nothing uses it (the live system's layout was not traced, or it was traced to this disk): {disk:?}"
    );
    ensure!(
        disk.size_bytes == bytes && disk.bus == "virtio",
        "{path} is not the disk the harness attached ({bytes} bytes, virtio): {disk:?}"
    );
    Ok(())
}

fn observe(vm: &mut Vm, medium: Medium, artifacts: &Path) -> Result<()> {
    let serial = vm.info.dir.join("serial.log");
    // The session first: the helper is looked at in the state a person finds it
    // in, with the compositor and the page running beside it.
    let console = wait_for_marker(vm, &serial, "ILEX-INSTALLER", "page", 1, PAGE_TIMEOUT)?;
    ensure!(
        field(&fields(&console, "ILEX-INSTALLER", "page")?, "edition")? == "gnome",
        "unexpected edition on the console"
    );
    vm.wait_ssh(Duration::from_secs(180))
        .context("the harness's SSH key did not give a login to the live ISO")?;

    // What the system looks like, saved before anything is asserted.
    let evidence = ssh(vm, EVIDENCE)?;
    save(artifacts, medium, "layout.txt", &evidence.text)?;
    println!(
        "iso::next helper ({}): the live system's layout is in next-helper-{}-layout.txt",
        medium.name(),
        medium.name()
    );

    // The helper is bus-activated: wait for the name.
    let ready = ssh(
        vm,
        &format!(
            "for i in $(seq 1 120); do busctl --system status {BUS} >/dev/null 2>&1 && exit 0; sleep 1; done; systemctl status ilex-installerd --no-pager; journalctl -u ilex-installerd --no-pager | tail -30; exit 1"
        ),
    )?;
    ensure!(
        ready.code == Some(0),
        "the helper never owned {BUS}: {}",
        ready.text
    );

    // Probe, as the session's user.
    let ran = ssh(vm, &busctl("ilex-installer", "Probe", None))?;
    save(artifacts, medium, "probe.txt", &ran.text)?;
    let probe: Probe = serde_json::from_value(document(&ran, "Probe")?)?;
    save(
        artifacts,
        medium,
        "probe.json",
        &serde_json::to_string_pretty(&probe)?,
    )?;
    let summary: Vec<String> = probe
        .disks
        .iter()
        .map(|d| {
            format!(
                "{} {} {} bytes bus={} live_medium={} in_use={:?} notes={:?}",
                d.id, d.path, d.size_bytes, d.bus, d.live_medium, d.in_use, d.notes
            )
        })
        .collect();
    println!(
        "iso::next helper ({}): probe: {}",
        medium.name(),
        summary.join(" | ")
    );
    ensure!(
        probe.edition.id == "gnome" && probe.firmware.uefi,
        "the helper does not report the GNOME edition booted through UEFI: {:?} {:?}",
        probe.edition,
        probe.firmware
    );
    ensure!(
        probe.firmware.secure_boot == Some(true),
        "the helper does not see Secure Boot: {:?}",
        probe.firmware
    );

    // The three blank disks are offered, whatever the live system looks like.
    // They are found by their sizes, which no other disk has: virtio names
    // them in the order the firmware and the kernel see them.
    let blanks = blank_disks_of(&probe)?;
    for (name, bytes) in &blanks {
        offered(&probe, &format!("/dev/{name}"), *bytes)?;
    }
    let live: Vec<&Disk> = probe.disks.iter().filter(|d| d.live_medium).collect();
    match medium {
        Medium::Usb => {
            // The stick: a removable USB disk, the medium the system came from.
            ensure!(
                live.len() == 1 && live[0].bus == "usb" && live[0].removable,
                "the boot medium is not the one live_medium disk: {live:?}"
            );
            // Nothing is said of the stick but that it is the medium: a note
            // would be a failed scan of its EFI partition (the ESP scan reads
            // it like any other disk's), a reason a use the trace could not
            // place.
            ensure!(
                live[0].in_use.is_empty() && live[0].notes.is_empty(),
                "the live medium carries a reason or a note besides being the medium: {:?}",
                live[0]
            );
            ensure!(
                probe.disks.len() == 4,
                "expected the stick and three blank disks: {summary:?}"
            );
        }
        Medium::Cdrom => {
            // An optical drive is no installation target: it is not listed.
            ensure!(
                live.is_empty() && probe.disks.len() == 3,
                "expected exactly the three blank disks: {summary:?}"
            );
        }
        Medium::Loop => {
            // The disk the ISO file lies on is the medium, nothing else of it
            // is said (a loop device the installer cannot place would also
            // refuse every other disk, and the blank ones are offered above).
            ensure!(
                live.len() == 1 && live[0].bus == "virtio" && !live[0].removable,
                "the disk holding the ISO file is not the one live_medium disk: {live:?}"
            );
            ensure!(
                live[0].in_use.is_empty() && live[0].notes.is_empty(),
                "the live medium carries a reason or a note besides being the medium: {:?}",
                live[0]
            );
            // The host disk and the three blank ones. The boot loader's own
            // FAT disk is read-only, and a read-only disk is no target.
            ensure!(
                probe.disks.len() == 4,
                "expected the host disk and three blank disks: {summary:?}"
            );
        }
    }

    // Plan on a blank disk: the whole-disk layout through repart's dry run,
    // against the registry. Nothing is written: the disks are looked at first,
    // blank, and again after every plan.
    let blank_before = blank_state(vm, &blanks).context("the blank disks, before any plan")?;
    save(artifacts, medium, "disks-before.txt", &blank_before)?;
    let blank = disk_at(&probe, &format!("/dev/{}", blanks[0].0))?;
    let ran = plan_when_online(vm, blank)?;
    save(artifacts, medium, "plan.txt", &ran.text)?;
    let plan: Plan = serde_json::from_value(document(&ran, "Plan")?)?;
    ensure!(
        plan.disk.path == blank.path
            && plan.edition == "gnome"
            && plan.composition.tag == "generic.gnome"
            && plan.image.digest.starts_with("sha256:")
            && plan.partitions.len() >= 3
            && plan.erases.disk == blank.id,
        "unexpected plan: {plan:?}"
    );
    println!(
        "iso::next helper ({}): plan on {} -> {} {} ({} partitions)",
        medium.name(),
        blank.id,
        plan.composition.tag,
        plan.image.digest,
        plan.partitions.len()
    );

    // Refusals: the medium itself (the stick, or the disk holding the ISO
    // file), and a disk that is too small.
    if medium != Medium::Cdrom {
        let stick = live[0];
        let refused = refusal(&plan_refused(vm, stick)?)?;
        ensure!(
            refused.code == ErrorCode::LiveMedium,
            "a plan on the boot medium was refused as {:?}: {}",
            refused.code,
            refused.message
        );
    }
    let small = disk_at(&probe, &format!("/dev/{}", blanks[2].0))?;
    let refused = refusal(&plan_refused(vm, small)?)?;
    ensure!(
        refused.code == ErrorCode::DiskTooSmall,
        "a plan on a 2 GiB disk was refused as {:?}: {}",
        refused.code,
        refused.message
    );

    // The same answers after the plans: a plan leaves nothing held, and the
    // disks it looked at are what they were (no table, no partition, not a
    // byte of the first and last MiB changed).
    let ran = ssh(vm, &busctl("ilex-installer", "Probe", None))?;
    let again: Probe = serde_json::from_value(document(&ran, "Probe")?)?;
    for (name, bytes) in &blanks {
        offered(&again, &format!("/dev/{name}"), *bytes).context("after the plans")?;
    }
    let blank_after = blank_state(vm, &blanks).context("the blank disks, after the plans")?;
    save(artifacts, medium, "disks-after.txt", &blank_after)?;
    ensure!(
        blank_before == blank_after,
        "a plan changed a disk it only looked at:\n--- before\n{blank_before}--- after\n{blank_after}"
    );

    // The helper's own self-check, with the files this ISO ships.
    let check = ssh(
        vm,
        "/usr/libexec/ilex-installer/ilex-installerd --self-check",
    )?;
    save(artifacts, medium, "self-check.txt", &check.text)?;
    ensure!(
        check.code == Some(0) && check.text.contains("ILEX-INSTALLERD-CHECK OK"),
        "ilex-installerd --self-check on the ISO failed (exit {:?}): {}",
        check.code,
        check.text
    );

    // No scratch directory is left behind by the plans or by the self-check
    // (which makes its own beside them): looked for last.
    let leftovers = ssh(
        vm,
        "ls /run/ilex-installer 2>&1; find /run/ilex-installer -mindepth 1 -maxdepth 1 \\( -name 'scan-*' -o -name 'selfcheck-*' \\) 2>/dev/null",
    )?;
    ensure!(
        leftovers.code == Some(0)
            && !leftovers.text.contains("scan-")
            && !leftovers.text.contains("selfcheck-"),
        "scratch directories were left behind: {}",
        leftovers.text
    );

    // SELinux. The live ISO boots permissive (`enforcing=0`) with unlabelled
    // files, so denials are logged and nothing is stopped: none may be an
    // enforced one (`permissive=0`), and those that name the helper (its
    // process, its runtime directory, its bus name) are reported, because the
    // day the ISO enforces, each of them is a denial of the helper. The mode
    // and the label of the helper's files are recorded.
    let avc = ssh(
        vm,
        "getenforce; ls -lZ /usr/libexec/ilex-installer/ilex-installerd; journalctl -b --no-pager -o short-monotonic -g 'avc:' 2>&1 | grep -iE 'avc: +denied' || true",
    )?;
    save(artifacts, medium, "avc.txt", &avc.text)?;
    let denials: Vec<&str> = avc
        .text
        .lines()
        .filter(|l| l.contains("avc:") && l.contains("denied"))
        .collect();
    let enforced: Vec<&&str> = denials
        .iter()
        .filter(|l| l.contains("permissive=0"))
        .collect();
    ensure!(
        enforced.is_empty(),
        "SELinux denied (and enforced): {enforced:#?} (all: next-helper-{}-avc.txt)",
        medium.name()
    );
    let ours: Vec<&&str> = denials
        .iter()
        .filter(|l| {
            l.contains("ilex-installe")
                || l.contains("ilex_installer")
                || l.contains("org.ilex.Installer1")
        })
        .collect();
    println!(
        "iso::next helper ({}): SELinux {}: {} denials logged, {} naming the helper (permissive; see next-helper-{}-avc.txt)",
        medium.name(),
        avc.text.lines().next().unwrap_or("?"),
        denials.len(),
        ours.len(),
        medium.name()
    );
    Ok(())
}

/// A plan that has to be refused: no retry for the registry (a refusal for the
/// disk comes first, before the registry is asked).
fn plan_refused(vm: &Vm, disk: &Disk) -> Result<Ran> {
    let ran = plan(vm, disk)?;
    if ran.code == Some(0) {
        bail!("a plan on {} was accepted: {}", disk.path, ran.text);
    }
    Ok(ran)
}
