//! `installer::` tier: the new installer's root helper, without an ISO.
//!
//! The next installer's live image (`ILEX_INSTALLER_IMAGE`, built by
//! `ilexctl iso --next`'s image stage) boots as an ephemeral VM with an SSH
//! layer added for the harness and eight disks: a blank 40 GiB one, one
//! carrying a fake other-OS layout, one carrying an old Linux installation (an
//! LVM volume group and an MD array, inactive), an 8 GiB one, and four the
//! running system is using (an ISO mounted by its label, an ISO file attached to
//! a loop device from a mounted partition as dracut's iso-scan does, Ventoy's
//! device-mapper device, and a disk with a mounted partition, swap and a stacked
//! device). The
//! tests call `org.ilex.Installer1` with `busctl` as the unprivileged
//! `ilex-installer` user, exactly as the interface will, and assert the JSON;
//! another user must be refused by the bus policy. The tests share one VM and
//! one helper, which some of them restart: the group runs them one at a time.
//! Nothing in this group installs: no test of it writes to the helper's disks.
//! (`installer::policy`, `installer::install` and `installer::interrupted`,
//! in `installer/apply.rs`, do, in UEFI VMs of their own.)
//!
//! * `installer::probe`: the helper's self-check; what it reports about this
//!   VM and its disks, the live medium and the disks in use among them, and
//!   the uses the mount table does not show, each reproduced for real (a loop
//!   device on a block device node, a deleted node on a tmpfs, an unmounted
//!   loop device on a lazily unmounted file system, dracut's live layout with
//!   either mount unmounted lazily, zram writeback, an ext4's external
//!   journal, a mount under a private subtree, a file system unmounted lazily
//!   that a process still holds);
//!   detection and the boot override through the real bus path (the SMBIOS
//!   identity and kernel command line are fixtures the unit is pointed at: a
//!   VM cannot carry an MSI board); the bus policy and bus activation.
//! * `installer::plan`: whole-disk plans (plain and encrypted) against
//!   repart's own dry run, the registry's manifest, every refusal a request
//!   can cause, and the proof that planning wrote nothing.
//! * `installer::finish`: `Finish`'s refusals through the real bus path: a
//!   malformed action, a log written to a fixed disk, to the medium the system
//!   started from (both ways it can be the live medium), or to a device that is
//!   not there; another user is refused by the bus policy; no disk changes and
//!   the helper still answers. (Restart and power off would end this VM, and a
//!   stick that is removable needs a VM with USB: `iso::next_problem` writes the
//!   log for real.)
//! * `installer::ui`: the interface's own checks on the live image, without a
//!   display of its own: `ilex-installer --self-check` (the flow's rules on
//!   the simulation's fixtures, the image's keyboard catalogue, the edition
//!   record), `--simulate` refused on a real boot, and every page and error
//!   state drawn to PNG files under GNOME Kiosk's headless mode: light, dark
//!   and at 150% scale (`--self-check --render`).
//!
//! The ISO's own boot (the live medium, Secure Boot, the session) is `iso::next…`.

use std::fs;
use std::process::Command;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::bcvk::{EphemeralOpts, EphemeralVm, Output};
use ilex_devkit::installer_model::{plan::Plan, probe::Probe};
use ilex_devkit::qemu::base64;
use ilex_devkit::{Workspace, cmd, ownership};
use libtest_mimic::Trial;
use serde_json::{Value, json};

use crate::common::{self, Group, trial};

pub(crate) mod apply;

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// A change to a JSON document, for a variant of a fixture or a request.
type Edit = Box<dyn Fn(&mut Value)>;
/// A detection case: its name, SMBIOS fields and kernel command line.
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, &'a str);

/// Disks attached to the VM: stable ids `virtio-<name>` (bcvk names the
/// virtio serial), and their sizes.
const DISKS: [(&str, u64); 8] = [
    ("blank", 40 * GIB),
    ("layout", 48 * GIB),
    ("oldlinux", 36 * GIB),
    ("small", 8 * GIB),
    ("live", GIB),
    ("isoscan", 2 * GIB),
    ("ventoy", 2 * GIB),
    ("busy", 2 * GIB),
];
const BUS: &str = "org.ilex.Installer1";
const MEMORY: &str = "4G";

/// The helper is one process on one bus name and some tests restart it with
/// fixtures: the tests of the group take turns (`cargo ilexctl test
/// installer::` does not need `--test-threads 1`).
static GROUP: Group<Mutex<EphemeralVm>> = Group::new("installer", boot);

/// How many of the `installer::` trials run in the group's VM: the ones that
/// write a disk boot their own and are counted out by the caller.
pub fn expect(tests: usize) {
    GROUP.expect(tests);
}

pub fn shutdown() {
    GROUP.finish();
}

pub fn trials() -> Vec<Trial> {
    let mut trials = vec![
        trial("installer::probe", || {
            GROUP.run(|vm| probe(&vm.lock().unwrap_or_else(PoisonError::into_inner)))
        }),
        trial("installer::plan", || {
            GROUP.run(|vm| plan(&vm.lock().unwrap_or_else(PoisonError::into_inner)))
        }),
        trial("installer::ui", || {
            GROUP.run(|vm| ui(&vm.lock().unwrap_or_else(PoisonError::into_inner)))
        }),
        trial("installer::finish", || {
            GROUP.run(|vm| finish(&vm.lock().unwrap_or_else(PoisonError::into_inner)))
        }),
    ];
    // The trials that write a disk boot VMs of their own (UEFI), not this
    // group's.
    trials.extend(apply::trials());
    trials
}

// --- the VM ---------------------------------------------------------------

/// What the harness boots: the live image the ISO is made from.
fn live_image() -> Result<String> {
    match std::env::var("ILEX_INSTALLER_IMAGE") {
        Ok(image) if !image.is_empty() => Ok(image),
        _ => bail!(
            "ILEX_INSTALLER_IMAGE is not set: run `cargo ilexctl test installer::`, which builds the live image first"
        ),
    }
}

/// The live image has no SSH server of its own on a real boot (a released ISO
/// must not open a port): sshd starts only once root's `authorized_keys` has
/// been provisioned, which bcvk does with its `tmpfiles.extra` credential. The
/// harness adds openssh-server in a layer of its own and keeps the kiosk
/// session from starting in a VM without a display. The layer is this
/// checkout's: its tag carries the checkout's slug, the image it extends and the
/// layer's own recipe (a changed recipe is a different tag), and the image is
/// labelled with the checkout's owner like every other image the tooling builds.
///
/// The live image is only ever booted as a live ISO, so it has no bootc install
/// configuration either; the trials that write it to a disk (`bcvk to-disk`,
/// for a UEFI boot) need one, and the layer adds it.
fn with_ssh(ws: &Workspace, live: &str) -> Result<String> {
    let id =
        cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Id}}", live]))
            .with_context(|| format!("the installer image {live} is not in local storage"))?;
    let recipe = format!(
        "FROM {live}\n\
         RUN dnf -y install --setopt=install_weak_deps=0 openssh-server dbus-daemon && \\\n\
             systemctl enable sshd.service && \\\n\
             systemctl mask ilex-installer-session.service ilex-installer-fallback.service && \\\n\
             mkdir -p /usr/lib/bootc/install && printf '[install.filesystem.root]\\ntype = \"xfs\"\\n' > /usr/lib/bootc/install/10-installer-test.toml\n\
         LABEL ilex.installer-test=\"ssh\"\n"
    );
    let tag = format!(
        "localhost/ilex-installer-next-test:{}.{}.{}",
        ws.slug,
        &id[..12.min(id.len())],
        // "sha256:" and the first eight digits of the recipe's.
        &ilex_devkit::core::digest(recipe.as_bytes())[7..15]
    );
    if ownership::image_exists(&tag)? {
        // An existing tag is reused only when this checkout built it.
        ownership::require_image(ws, &tag)?;
        return Ok(tag);
    }
    let transaction = ownership::ImageTransaction::new(ws, &tag)?;
    let owner = ws.identity()?.owner;
    let dir = ws.state_dir().join("installer");
    fs::create_dir_all(&dir)?;
    let containerfile = dir.join("Containerfile.ssh");
    fs::write(&containerfile, &recipe)?;
    cmd::run(
        Command::new("podman")
            .args(["build", "--network=host", "--file"])
            .arg(&containerfile)
            .args(["--tag", transaction.temporary()])
            .args(["--label", &format!("ilex.checkout-owner={owner}")])
            .arg(&dir),
    )?;
    transaction.promote(ws)?;
    Ok(tag)
}

fn boot() -> Result<Mutex<EphemeralVm>> {
    let ws = common::workspace()?;
    let image = with_ssh(&ws, &live_image()?)?;
    let dir = ws.state_dir().join("installer");
    fs::create_dir_all(&dir)?;
    let mut opts = EphemeralOpts {
        memory: MEMORY.into(),
        ..EphemeralOpts::default()
    };
    for (name, size) in DISKS {
        let path = dir.join(format!("{name}.img"));
        // Fresh every run: a plan must see blank disks.
        let _ = fs::remove_file(&path);
        fs::File::create(&path)?.set_len(size)?;
        opts.extra_args.push("--mount-disk-file".into());
        opts.extra_args.push(format!("{}:{name}", path.display()));
    }
    let name = ws.vm_name("installer");
    let vm = EphemeralVm::boot(&name, &image, &opts)?;
    eprintln!(
        "ilex: {name} is up from {image} (VM slot {})",
        vm.slot_index()
    );
    prepare_layout_disk(&vm)?;
    prepare_old_linux_disk(&vm)?;
    prepare_medium_disks(&vm)?;
    wait_for_helper(&vm)?;
    Ok(Mutex::new(vm))
}

/// A GPT disk the way a dual-boot machine looks: an ESP with Windows' and
/// Ubuntu's loader directories, the reserved partition, a LUKS volume and
/// a Linux data partition with a known label.
fn prepare_layout_disk(vm: &EphemeralVm) -> Result<()> {
    let script = r#"set -eu
d=/dev/disk/by-id/virtio-layout
sfdisk --quiet "$d" <<'EOF'
label: gpt
size=300MiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name="EFI system partition"
size=16MiB, type=E3C9E316-0B5C-4DB8-817D-F92DF00215AE, name="Microsoft reserved"
size=2GiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="Basic data partition"
size=4GiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="keep"
EOF
udevadm settle
for n in 1 2 3 4; do test -b "$d-part$n"; done
mkfs.vfat -F32 -n SYSTEM "$d-part1" >/dev/null
mount "$d-part1" /mnt
mkdir -p /mnt/EFI/Microsoft/Boot /mnt/EFI/BOOT /mnt/EFI/ubuntu
umount /mnt
printf 'ilex-test-key' > /run/layout-key
cryptsetup luksFormat --batch-mode --type luks2 --pbkdf pbkdf2 --pbkdf-force-iterations 1000 --key-file /run/layout-key "$d-part3"
rm -f /run/layout-key
mkfs.ext4 -q -L keepme "$d-part4"
udevadm settle
blkid -p -o value -s TYPE "$d-part3"
"#;
    let out = vm
        .ssh(&format!(
            "printf %s {} | base64 -d | bash -se",
            base64(script.as_bytes())
        ))?
        .ok("preparing the layout disk")?;
    ensure!(
        out.out() == "crypto_LUKS",
        "libblkid does not take the volume for LUKS: {:?}",
        out.out()
    );
    Ok(())
}

/// A disk the way an earlier Linux installation leaves it: a `/boot`, an LVM
/// volume group (a root and a swap volume) and an MD RAID 1 array, all
/// deactivated. Nothing here is activated by the live image, so the disk has
/// no holders and is offered like any other.
fn prepare_old_linux_disk(vm: &EphemeralVm) -> Result<()> {
    let script = r#"set -eu
d=/dev/disk/by-id/virtio-oldlinux
sfdisk --quiet "$d" <<'PARTITIONS'
label: gpt
size=1GiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="old boot"
size=1GiB, type=E6D6D379-F507-44C2-A23C-238F2A3DF928, name="old lvm"
size=512MiB, type=A19D880F-05FC-4D3B-A006-743F0F84911E, name="old raid 1"
size=512MiB, type=A19D880F-05FC-4D3B-A006-743F0F84911E, name="old raid 2"
PARTITIONS
udevadm settle
for n in 1 2 3 4; do test -b "$d-part$n"; done
mkfs.ext4 -q -L oldboot "$d-part1"
pvcreate -ff -y "$d-part2" >/dev/null
vgcreate vgold "$d-part2" >/dev/null
lvcreate -y -L 256M -n root vgold >/dev/null
lvcreate -y -L 128M -n swap vgold >/dev/null
mkfs.ext4 -q -L oldroot /dev/vgold/root
mkswap -q /dev/vgold/swap
mdadm --create /dev/md0 --run --level=1 --raid-devices=2 --metadata=1.2 "$d-part3" "$d-part4" >/dev/null 2>&1
mkfs.ext4 -q -L olddata /dev/md0
udevadm settle
vgchange -an vgold >/dev/null
mdadm --stop /dev/md0 >/dev/null
udevadm settle
blkid -p -o value -s TYPE "$d-part2" "$d-part3"
"#;
    let out = vm
        .ssh(&format!(
            "printf %s {} | base64 -d | bash -se",
            base64(script.as_bytes())
        ))?
        .ok("preparing the old Linux disk")?;
    ensure!(
        printed(&out).trim() == "LVM2_member\nlinux_raid_member",
        "libblkid does not take the volumes for LVM and MD: {:?}",
        printed(&out)
    );
    Ok(())
}

/// Four disks the running system is using (and the zram swap every live boot
/// has, which uses none), set up the way the situations they stand for look to
/// the kernel:
///
/// * `live`: an ISO 9660 file system straight on the disk, mounted by its
///   `/dev/disk/by-label` name as given (no canonical device name in the mount
///   table), the way an optical medium looks.
/// * `isoscan`: an ext4 partition holding `live.iso`, mounted where dracut's
///   iso-scan mounts the disk it found the ISO on, with the ISO attached to a
///   loop device that is mounted where dracut mounts the medium.
/// * `ventoy`: a device-mapper device over a partition holding an ISO (what
///   Ventoy makes of the ISO file), mounted by `/dev/mapper/ventoy`.
/// * `busy`: a mounted partition, active swap, and a device-mapper device
///   stacked on a third partition: in use, though not the medium.
fn prepare_medium_disks(vm: &EphemeralVm) -> Result<()> {
    let script = r#"set -eu
d=/dev/disk/by-id
modprobe dm_mod 2>/dev/null || true
mkdir -p /run/ilex-test/iso /run/ilex-test/medium /run/ilex-test/ventoy /run/ilex-test/busy /run/initramfs/isoscan /run/initramfs/live
echo 'ilex installer fixture' > /run/ilex-test/iso/README

# live: ISO 9660 on the whole disk, mounted by label, name not canonicalised.
# (xorriso will not write through a symbolic link: name the device itself)
xorrisofs -quiet -V ILEXMEDIUM -o "$(readlink -f "$d/virtio-live")" /run/ilex-test/iso
udevadm trigger --action=change "$d/virtio-live"
udevadm settle
test -e /dev/disk/by-label/ILEXMEDIUM
mount --no-canonicalize -t iso9660 -o ro /dev/disk/by-label/ILEXMEDIUM /run/ilex-test/medium

# isoscan: the ISO is a file on a mounted partition, attached to a loop device.
sfdisk --quiet "$d/virtio-isoscan" <<'EOF'
label: gpt
size=1GiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="host"
EOF
udevadm settle
mkfs.ext4 -q -L isohost "$d/virtio-isoscan-part1"
mount "$d/virtio-isoscan-part1" /run/initramfs/isoscan
xorrisofs -quiet -V ILEXISO -o /run/initramfs/isoscan/live.iso /run/ilex-test/iso
loop=$(losetup --find --show --read-only /run/initramfs/isoscan/live.iso)
echo "$loop" > /run/ilex-test/medium.loop
mount -t iso9660 -o ro "$loop" /run/initramfs/live

# ventoy: a device-mapper device over a partition that holds an ISO.
sfdisk --quiet "$d/virtio-ventoy" <<'EOF'
label: gpt
size=64MiB, type=EBD0A0A2-B9E5-4433-87C0-68B6B72699C7, name="Ventoy"
size=512MiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="spare"
EOF
udevadm settle
xorrisofs -quiet -V ILEXVENTOY -o "$(readlink -f "$d/virtio-ventoy-part1")" /run/ilex-test/iso
sectors=$(blockdev --getsz "$d/virtio-ventoy-part1")
dmsetup create ventoy --table "0 $sectors linear $(readlink -f "$d/virtio-ventoy-part1") 0"
udevadm settle
mount -t iso9660 -o ro /dev/mapper/ventoy /run/ilex-test/ventoy

# busy: a mounted partition, swap, and a device stacked on a partition.
sfdisk --quiet "$d/virtio-busy" <<'EOF'
label: gpt
size=256MiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="data"
size=256MiB, type=0657FD6D-A4AB-43C4-84E5-0933C84B4F4F, name="swap"
size=256MiB, type=0FC63DAF-8483-4772-8E79-3D69D8477DE4, name="stacked"
EOF
udevadm settle
mkfs.ext4 -q -L busydata "$d/virtio-busy-part1"
mount "$d/virtio-busy-part1" /run/ilex-test/busy
mkswap -q "$d/virtio-busy-part2"
swapon "$d/virtio-busy-part2"
sectors=$(blockdev --getsz "$d/virtio-busy-part3")
dmsetup create busystack --table "0 $sectors linear $(readlink -f "$d/virtio-busy-part3") 0"
udevadm settle

# a live boot's own swap: zram, as zram-generator makes it, with no writeback
# device; it is a use of RAM and never makes a disk look in use
grep -q '^/dev/zram' /proc/swaps || {
    modprobe zram
    id=$(cat /sys/class/zram-control/hot_add)
    echo 256M > /sys/block/zram$id/disksize
    mkswap -q /dev/zram$id
    swapon --priority 100 /dev/zram$id
}
grep -E 'ILEXMEDIUM|isoscan|initramfs/live|ventoy|ilex-test/busy' /proc/self/mountinfo
cat /proc/swaps
"#;
    let out = vm
        .ssh(&format!(
            "printf %s {} | base64 -d | bash -se",
            base64(script.as_bytes())
        ))?
        .ok("preparing the disks the system is using")?;
    let mounts = printed(&out);
    // The by-label mount keeps its source as given: the helper must resolve it.
    ensure!(
        mounts.contains("iso9660 /dev/disk/by-label/ILEXMEDIUM")
            && mounts.contains("/run/initramfs/live")
            && mounts.contains("iso9660 /dev/mapper/ventoy")
            && mounts.contains("/run/initramfs/isoscan")
            && mounts.contains("/dev/zram"),
        "the fixture mounts are not what they should be:\n{mounts}"
    );
    println!("     fixture mounts:\n{}", mounts.trim_end());
    Ok(())
}

/// The shell that waits for the helper to take its bus name (up to two minutes).
const WAIT_FOR_HELPER: &str = "for i in $(seq 1 120); do busctl --system status org.ilex.Installer1 >/dev/null 2>&1 && break; sleep 1; done";

fn wait_for_helper(vm: &EphemeralVm) -> Result<()> {
    let out = vm.ssh(&format!(
        "{WAIT_FOR_HELPER}; busctl --system status {BUS} >/dev/null 2>&1 || {{ systemctl status ilex-installerd --no-pager; journalctl -u ilex-installerd --no-pager | tail -30; exit 1; }}"
    ))?;
    ensure!(
        out.success(),
        "the helper never took {BUS}:\n{}",
        printed(&out)
    );
    Ok(())
}

// --- calling the helper -----------------------------------------------------

/// Everything a guest command printed. bcvk runs it on a terminal, which
/// merges stderr into stdout and may colour it: strip both.
fn printed(out: &Output) -> String {
    let mut text = String::new();
    let raw = format!("{}{}", out.stdout, out.stderr);
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\x1b' => {
                // A CSI sequence: ESC [ ... final byte.
                if chars.peek() == Some(&'[') {
                    chars.next();
                    for d in chars.by_ref() {
                        if d.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            c => text.push(c),
        }
    }
    text
}

/// One guest script's result.
struct Ran {
    code: i32,
    text: String,
}

/// Run several guest scripts in one SSH session. (Every `bcvk ephemeral ssh`
/// leaves a podman exec session behind until the VM goes, and each holds an
/// inotify instance: a tier that makes hundreds of calls runs into the host's
/// default limit of 128, so related checks share a session.)
fn batch(vm: &EphemeralVm, scripts: &[String]) -> Result<Vec<Ran>> {
    let mut master = String::new();
    for (i, script) in scripts.iter().enumerate() {
        master.push_str(&format!("f{i}() {{\n{script}\n}}\n"));
    }
    for i in 0..scripts.len() {
        master.push_str(&format!(
            "printf '@@BEGIN %s\\n' {i}; out=$(f{i} 2>&1); rc=$?; printf '%s\\n@@END %s %s\\n' \"$out\" {i} \"$rc\"\n"
        ));
    }
    let out = vm
        .ssh(&format!(
            "printf %s {} | base64 -d | bash -s",
            base64(master.as_bytes())
        ))?
        .ok("batched guest scripts")?;
    let text = printed(&out);
    let mut results = Vec::new();
    for i in 0..scripts.len() {
        let begin = format!("@@BEGIN {i}\n");
        let start = text
            .find(&begin)
            .with_context(|| format!("script {i} did not start:\n{text}"))?
            + begin.len();
        let end_marker = format!("@@END {i} ");
        let end = start
            + text[start..]
                .find(&end_marker)
                .with_context(|| format!("script {i} did not finish:\n{text}"))?;
        let code = text[end + end_marker.len()..]
            .lines()
            .next()
            .and_then(|l| l.trim().parse().ok())
            .context("a script's exit code")?;
        results.push(Ran {
            code,
            text: text[start..end].trim_end().to_owned(),
        });
    }
    Ok(results)
}

/// The document a successful `busctl --json=short call` returned.
fn document(ran: &Ran, method: &str) -> Result<Value> {
    ensure!(
        ran.code == 0,
        "{method} failed (exit {}): {}",
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

/// The JSON of a refusal: busctl's `Call failed: {...}`.
fn refusal_document(ran: &Ran) -> Result<Value> {
    ensure!(ran.code != 0, "the request was not refused: {}", ran.text);
    let json = ran
        .text
        .lines()
        .find_map(|line| line.trim().strip_prefix("Call failed: "))
        .with_context(|| format!("busctl's error: {}", ran.text))?;
    serde_json::from_str(json).with_context(|| format!("the refusal's document: {}", ran.text))
}

/// Refused by the bus itself.
fn denied(ran: &Ran) -> bool {
    ran.code != 0 && ran.text.contains("Access denied")
}

/// `busctl` as `user`, in a script for the guest's root shell. The request
/// travels base64 so no quoting depends on its text.
fn busctl(user: &str, method: &str, request: Option<&str>) -> String {
    let argument = request.map_or(String::new(), |r| {
        format!(" s \"$(printf %s {} | base64 -d)\"", base64(r.as_bytes()))
    });
    format!(
        "runuser -u {user} -- env SYSTEMD_COLORS=0 SYSTEMD_PAGER= busctl --system --json=short call {BUS} /org/ilex/Installer1 {BUS} {method}{argument}"
    )
}

/// A successful call: the JSON document the method returned.
fn call(vm: &EphemeralVm, method: &str, request: Option<&str>) -> Result<Value> {
    let out = vm.ssh(&busctl("ilex-installer", method, request))?;
    ensure!(
        out.success(),
        "{method} as ilex-installer failed (exit {:?}): {}",
        out.code,
        printed(&out).trim()
    );
    let text = printed(&out);
    let reply: Value = serde_json::from_str(text.trim()).with_context(|| {
        format!(
            "busctl's JSON ({} bytes): {} ... {}",
            text.len(),
            text.chars().take(200).collect::<String>(),
            text.chars()
                .rev()
                .take(300)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<String>()
        )
    })?;
    ensure!(
        reply["type"] == "s",
        "{method} did not return a string: {reply}"
    );
    serde_json::from_str(reply["data"][0].as_str().context("busctl's data")?)
        .with_context(|| format!("{method}'s document"))
}

fn probe_document(vm: &EphemeralVm) -> Result<Probe> {
    Ok(serde_json::from_value(call(vm, "Probe", None)?)?)
}

fn plan_request(edit: impl FnOnce(&mut Value)) -> String {
    let mut request = json!({
        "composition": "generic.gnome",
        "disk": "virtio-blank",
        "mode": "whole-disk",
        "encrypt": false,
        "administrator": "ilexmaint",
    });
    edit(&mut request);
    request.to_string()
}

/// Where the fixtures the helper is pointed at live. (Not `/run/ilex-test`,
/// where the disks the system uses are mounted: nothing here removes a tree
/// that something is mounted below.)
const FIXTURES: &str = "/run/ilex-fixture";

/// Restart the helper, failing the script if the restart fails: a test must
/// never go on against the helper it meant to replace. (An explicit restart
/// counts against the unit's start limit, so a run of them is first forgiven.)
const RESTART: &str = "systemctl reset-failed ilex-installerd 2>/dev/null; systemctl restart ilex-installerd || exit 1";

/// A script that restarts the helper with `args` after its name (a unit
/// drop-in) and waits until it has taken its bus name.
fn restart_helper(args: &str) -> String {
    format!(
        "mkdir -p /run/systemd/system/ilex-installerd.service.d\nprintf '[Service]\\nExecStart=\\nExecStart=/usr/libexec/ilex-installer/ilex-installerd {args}\\n' > /run/systemd/system/ilex-installerd.service.d/fixture.conf\nsystemctl daemon-reload\n{RESTART}\n{WAIT_FOR_HELPER}\n"
    )
}

/// A script that makes a catalogue fixture in `{FIXTURES}/<dir>`: the shipped
/// catalogue with `edit` (Python over `t`) applied, and an edition record of its
/// own beside it, which `--table` must bring along.
fn table_fixture(dir: &str, edit: &str) -> String {
    format!(
        "mkdir -p {FIXTURES}/{dir}\npython3 - <<'PY'\nimport json\nt = json.load(open('/usr/share/ilex-installer/compositions.json'))\n{edit}\njson.dump(t, open('{FIXTURES}/{dir}/compositions.json', 'w'))\njson.dump({{'schema': 1, 'edition': 'gnome', 'display-name': 'Fixture GNOME'}}, open('{FIXTURES}/{dir}/edition.json', 'w'))\nPY\n"
    )
}

/// A script that points the helper at SMBIOS and kernel command line
/// fixtures (a unit drop-in), waits for it and probes it as `ilex-installer`.
/// `setup` runs after the fixture directory is made and `extra` is added to the
/// helper's arguments.
fn fixture_probe(dmi: &[(&str, &str)], cmdline: &str, setup: &str, extra: &str) -> String {
    let mut script = format!("rm -rf {FIXTURES}; mkdir -p {FIXTURES}/dmi\n{setup}");
    for (field, value) in dmi {
        script.push_str(&format!(
            "printf '%s\\n' {} > {FIXTURES}/dmi/{field}\n",
            shell_quote(value)
        ));
    }
    script.push_str(&format!(
        "printf '%s\\n' {} > {FIXTURES}/cmdline\n",
        shell_quote(cmdline)
    ));
    script.push_str(&restart_helper(&format!(
        "--dmi-dir {FIXTURES}/dmi --cmdline {FIXTURES}/cmdline {extra}"
    )));
    script.push_str(&busctl("ilex-installer", "Probe", None));
    script
}

/// No `scan-*` or `selfcheck-*` directory is left in the helper's run
/// directory: its scratch directories are removed with the call that made them.
fn no_scratch_left(vm: &EphemeralVm) -> Result<bool> {
    Ok(vm
        .ssh("test -d /run/ilex-installer && test -z \"$(ls -A /run/ilex-installer | grep -E '^(scan|selfcheck)-')\"")?
        .success())
}

/// The script that puts the helper back as the image ships it.
const RESTORE_HELPER: &str = "rm -rf /run/ilex-fixture /run/systemd/system/ilex-installerd.service.d; systemctl daemon-reload; systemctl reset-failed ilex-installerd 2>/dev/null; systemctl restart ilex-installerd || exit 1";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// --- installer::probe -------------------------------------------------------

fn probe(vm: &EphemeralVm) -> Result<()> {
    // The self-check beside the running helper: the helper probes in a loop
    // (mounting each ESP it finds) while the self-check runs its own probe.
    // Neither shares a mount point or a scratch directory with the other.
    let check = vm
        .ssh(&format!(
            "( for i in $(seq 1 6); do {} >/dev/null || echo PROBE-FAILED; done ) & loop=$!; /usr/libexec/ilex-installer/ilex-installerd --self-check; rc=$?; wait $loop; exit $rc",
            busctl("ilex-installer", "Probe", None)
        ))?
        .ok("ilex-installerd --self-check beside the helper")?;
    ensure!(
        check.stdout.contains("ILEX-INSTALLERD-CHECK OK") && !check.stdout.contains("PROBE-FAILED"),
        "self-check: {}",
        check.stdout
    );
    println!("     {}", check.out());
    ensure!(
        no_scratch_left(vm)?,
        "a scan or self-check left its scratch directory behind"
    );

    let probe = probe_document(vm)?;
    ensure!(probe.schema == 1, "probe schema {}", probe.schema);
    ensure!(
        probe.edition.id == "gnome" && probe.edition.name == "ilex Glabra",
        "edition {:?}",
        probe.edition
    );

    // This VM, as the guest itself reports it.
    let guest = vm
        .ssh("test -d /sys/firmware/efi && echo uefi || echo bios; for f in sys_vendor product_name board_vendor board_name; do printf '%s=%s\\n' $f \"$(cat /sys/class/dmi/id/$f 2>/dev/null)\"; done; awk '/MemTotal/ {print $2*1024}' /proc/meminfo")?
        .ok("guest facts")?;
    let mut lines = guest.out().lines();
    ensure!(
        (lines.next() == Some("uefi")) == probe.firmware.uefi,
        "firmware: {:?}",
        probe.firmware
    );
    let identity = &probe.detection.identity;
    for (field, value) in [
        ("sys_vendor", &identity.sys_vendor),
        ("product_name", &identity.product_name),
        ("board_vendor", &identity.board_vendor),
        ("board_name", &identity.board_name),
    ] {
        let wanted = lines
            .next()
            .and_then(|l| l.strip_prefix(&format!("{field}=")))
            .context("guest DMI line")?;
        ensure!(
            wanted.trim() == value,
            "{field}: probe {value:?}, guest {wanted:?}"
        );
    }
    let total: u64 = lines.next().context("guest memory")?.trim().parse()?;
    ensure!(
        probe.memory.total_bytes == total && probe.memory.available_bytes <= total,
        "memory {:?} against {total}",
        probe.memory
    );
    ensure!(
        probe.detection.matches.is_empty()
            && probe.detection.tag == probe.catalogue.default
            && probe.detection.reason == "no machine configuration matches this computer",
        "detection {:?}",
        probe.detection
    );
    ensure!(
        probe.selection.as_ref().is_some_and(
            |s| s.tag == "generic.gnome" && s.target == "ghcr.io/reidond/ilex:generic.gnome"
        ) && probe.selection_error.is_none(),
        "selection {:?}",
        probe.selection
    );
    ensure!(
        probe.enrollment["schema"] == 1 && probe.enrollment["verdict"] == "unavailable",
        "enrollment {}",
        probe.enrollment
    );

    // The catalogue is this edition's: its default shell, every offer.
    let offered: Vec<&str> = probe
        .catalogue
        .compositions
        .iter()
        .map(|c| c.tag.as_str())
        .collect();
    ensure!(
        offered.contains(&"generic.gnome")
            && probe
                .catalogue
                .compositions
                .iter()
                .all(|c| c.shells == ["gnome"]),
        "catalogue offers {offered:?}"
    );

    // The disks, by their stable ids, in id order.
    let ids: Vec<&str> = probe.disks.iter().map(|d| d.id.as_str()).collect();
    ensure!(
        ids == [
            "virtio-blank",
            "virtio-busy",
            "virtio-isoscan",
            "virtio-layout",
            "virtio-live",
            "virtio-oldlinux",
            "virtio-small",
            "virtio-ventoy"
        ],
        "disk ids {ids:?}"
    );
    let find = |name: &str| -> Result<&ilex_devkit::installer_model::probe::Disk> {
        probe
            .disks
            .iter()
            .find(|d| d.id == format!("virtio-{name}"))
            .with_context(|| format!("no disk {name}"))
    };
    for (name, size) in DISKS {
        let disk = find(name)?;
        ensure!(
            disk.size_bytes == size
                && disk.bus == "virtio"
                && !disk.removable
                && disk.sector_size == 512,
            "{name}: {disk:?}"
        );
    }
    // Nothing uses the four the tests plan on (the old installation's volume
    // group and array are inactive: the live image does not activate them).
    for name in ["blank", "layout", "oldlinux", "small"] {
        let disk = find(name)?;
        ensure!(
            !disk.live_medium && disk.in_use.is_empty() && disk.notes.is_empty(),
            "{name} is marked: {disk:?}"
        );
    }
    // The live medium, found however it is reached: an ISO mounted by its
    // label (the mount table names the symlink, not the disk), an ISO file on
    // a mounted partition attached to a loop device (dracut's iso-scan), a
    // device-mapper device over a partition (Ventoy).
    for name in ["live", "isoscan", "ventoy"] {
        let disk = find(name)?;
        ensure!(disk.live_medium, "{name} is not the live medium: {disk:?}");
    }
    ensure!(
        find("live")?.in_use.is_empty() && find("isoscan")?.in_use.is_empty(),
        "the live medium is also reported in use: {:?} {:?}",
        find("live")?.in_use,
        find("isoscan")?.in_use
    );
    ensure!(
        find("ventoy")?
            .in_use
            .iter()
            .any(|r| r.contains("is used by dm-")),
        "the device built on Ventoy's partition is not reported: {:?}",
        find("ventoy")?.in_use
    );
    for name in ["live", "isoscan", "ventoy", "busy"] {
        let disk = find(name)?;
        println!(
            "     {}: live medium {}, in use {:?}",
            disk.id, disk.live_medium, disk.in_use
        );
    }
    // In use but not the medium: a mounted partition, swap, a stacked device.
    let busy = find("busy")?;
    ensure!(
        !busy.live_medium
            && busy.in_use.len() == 3
            && busy
                .in_use
                .iter()
                .any(|r| r.ends_with("is mounted at /run/ilex-test/busy"))
            && busy.in_use.iter().any(|r| r.ends_with("is in use as swap"))
            && busy.in_use.iter().any(|r| r.contains("is used by dm-")),
        "busy: {busy:?}"
    );
    let blank = find("blank")?;
    ensure!(
        blank.partition_table.is_none()
            && blank.partitions.is_empty()
            && blank.other_systems.is_empty()
            && blank.free_extents.len() == 1
            && blank.free_extents[0].start_bytes == MIB
            && blank.free_extents[0].size_bytes == 40 * GIB - 2 * MIB,
        "blank: {blank:?}"
    );
    let layout = find("layout")?;
    ensure!(
        layout.partition_table.as_deref() == Some("gpt"),
        "layout table {:?}",
        layout.partition_table
    );
    let parts: Vec<(u32, u64, u64, &str)> = layout
        .partitions
        .iter()
        .map(|p| (p.number, p.start_bytes, p.size_bytes, p.filesystem.as_str()))
        .collect();
    ensure!(
        parts
            == [
                (1, MIB, 300 * MIB, "vfat"),
                (2, 301 * MIB, 16 * MIB, ""),
                (3, 317 * MIB, 2 * GIB, "crypto_LUKS"),
                (4, 2365 * MIB, 4 * GIB, "ext4"),
            ],
        "layout partitions {parts:?}"
    );
    ensure!(
        layout.partitions[3].filesystem_label == "keepme"
            && layout.partitions[0].label == "EFI system partition",
        "layout labels"
    );
    // After the last partition: to the last whole MiB before GPT's backup table.
    let tail_start = 2365 * MIB + 4 * GIB;
    ensure!(
        layout.free_extents.len() == 1
            && layout.free_extents[0].start_bytes == tail_start
            && layout.free_extents[0].size_bytes == 48 * GIB - MIB - tail_start,
        "layout free space {:?}",
        layout.free_extents
    );
    let kinds: Vec<(&str, &str)> = layout
        .other_systems
        .iter()
        .map(|s| (s.kind.as_str(), s.description.as_str()))
        .collect();
    for wanted in [
        ("efi-loader", "Windows Boot Manager (EFI/Microsoft)"),
        ("efi-loader", "EFI loader directory EFI/ubuntu"),
        ("windows", "Microsoft reserved partition"),
        ("encrypted", "LUKS-encrypted volume"),
        ("linux", "ext4 filesystem \"keepme\""),
    ] {
        ensure!(
            kinds.contains(&wanted),
            "{wanted:?} not found among {kinds:?}"
        );
    }
    ensure!(kinds.len() == 5, "other systems {kinds:?}");
    ensure!(
        find("small")?.partitions.is_empty(),
        "small disk is not blank"
    );
    // The old Linux installation: found for what it is, held by nothing.
    let old = find("oldlinux")?;
    ensure!(
        old.partition_table.as_deref() == Some("gpt") && old.partitions.len() == 4,
        "oldlinux: {old:?}"
    );
    for wanted in [
        "LVM2_member filesystem",
        "linux_raid_member filesystem",
        "ext4 filesystem \"oldboot\"",
    ] {
        ensure!(
            old.other_systems
                .iter()
                .any(|s| s.kind == "linux" && s.description.starts_with(wanted)),
            "{wanted} not found among {:?}",
            old.other_systems
        );
    }

    // Detection through the real bus path: the helper reads its identity from
    // a fixture directory and its command line from a fixture file. One
    // session runs every case, then puts the helper back.
    const MSI: &str = "Micro-Star International Co., Ltd.";
    const BOARD: &str = "MEG X570 UNIFY (MS-7C35)";
    let cases: [Case; 8] = [
        (
            "system and board",
            vec![
                ("sys_vendor", MSI),
                ("product_name", "MS-7C35"),
                ("board_vendor", MSI),
                ("board_name", BOARD),
            ],
            "quiet",
        ),
        (
            "board only",
            vec![("board_vendor", MSI), ("board_name", BOARD)],
            "quiet",
        ),
        (
            "system only",
            vec![("sys_vendor", MSI), ("product_name", "MS-7C35")],
            "quiet",
        ),
        (
            "case",
            vec![
                ("sys_vendor", "micro-star international co., ltd."),
                ("product_name", "MS-7C35"),
            ],
            "quiet",
        ),
        ("a vendor alone", vec![("sys_vendor", MSI)], "quiet"),
        (
            "boot override",
            vec![("sys_vendor", MSI), ("product_name", "MS-7C35")],
            "quiet ilex.composition=generic.gnome",
        ),
        ("unknown override", vec![], "ilex.composition=generic.kde"),
        ("no fixture", vec![], "quiet"),
    ];
    let mut scripts: Vec<String> = cases
        .iter()
        .map(|(_, dmi, cmdline)| fixture_probe(dmi, cmdline, "", ""))
        .collect();
    // Detection is made on the whole catalogue and then mapped into the
    // edition, with the edition record the table brings along (`--table`):
    // another machine with the same rule, whose only composition belongs to
    // another edition, is still a conflict; a detected machine whose only
    // composition is another edition's is reported as having none here.
    let msi = [("sys_vendor", MSI), ("product_name", "MS-7C35")];
    scripts.push(fixture_probe(
        &msi,
        "quiet",
        &table_fixture(
            "conflict",
            "t['machines']['other'] = {'description': 'Another', 'match': [dict(t['machines']['ms-7c35']['match'][0])]}\nt['shells']['ilex'] = {'label': 'ilex', 'description': 'fixture'}\nt['compositions'].append({'tag': 'other.ilex', 'machine': 'other', 'shells': ['ilex']})",
        ),
        &format!("--table {FIXTURES}/conflict/compositions.json"),
    ));
    scripts.push(fixture_probe(
        &msi,
        "quiet",
        &table_fixture(
            "elsewhere",
            "t['shells']['ilex'] = {'label': 'ilex', 'description': 'fixture'}\nt['compositions'] = [c for c in t['compositions'] if c['tag'] != 'ms-7c35.gnome'] + [{'tag': 'ms-7c35.ilex', 'machine': 'ms-7c35', 'shells': ['ilex']}]",
        ),
        &format!("--table {FIXTURES}/elsewhere/compositions.json"),
    ));
    scripts.push(RESTORE_HELPER.into());
    let ran = batch(vm, &scripts)?;
    ensure!(
        ran.last().is_some_and(|r| r.code == 0),
        "restoring the helper: {}",
        ran.last().map_or("", |r| &r.text)
    );
    wait_for_helper(vm)?;
    let names = cases
        .iter()
        .map(|(name, _, _)| *name)
        .chain(["a conflict across editions", "no composition here"]);
    let probes: Vec<Probe> = ran
        .iter()
        .zip(names)
        .map(|(ran, name)| {
            serde_json::from_value(document(ran, name)?).with_context(|| name.to_string())
        })
        .collect::<Result<_>>()?;
    let [
        full,
        board_only,
        system_only,
        lower,
        half,
        boot,
        unknown,
        _,
        conflict,
        elsewhere,
    ] = &probes[..]
    else {
        bail!("{} fixture probes", probes.len());
    };
    ensure!(
        conflict.edition.name == "Fixture GNOME"
            && elsewhere.edition.name == "Fixture GNOME"
            && full.edition.name == "ilex Glabra",
        "the edition record beside --table: {:?} / {:?}",
        conflict.edition,
        elsewhere.edition
    );
    ensure!(
        conflict.detection.tag == "generic.gnome"
            && conflict.detection.reason
                == "matches more than one machine configuration: ms-7c35, other"
            && conflict.detection.matches == ["ms-7c35", "other"]
            && conflict
                .catalogue
                .compositions
                .iter()
                .all(|c| c.shells == ["gnome"]),
        "a conflict with a machine of another edition: {:?}",
        conflict.detection
    );
    ensure!(
        elsewhere.detection.tag == "generic.gnome"
            && elsewhere.detection.reason
                == "detected machine ms-7c35 has no composition in the gnome edition"
            && elsewhere.detection.matches == ["ms-7c35"],
        "a machine with no composition in this edition: {:?}",
        elsewhere.detection
    );
    ensure!(
        full.detection.tag == "ms-7c35.gnome"
            && full.detection.reason == "detected"
            && full.detection.matches == ["ms-7c35"],
        "system and board: {:?}",
        full.detection
    );
    ensure!(
        full.selection.as_ref().is_some_and(|s| s.serial_line()
            == "ILEX-COMPOSITION ms-7c35.gnome (detected) installs ghcr.io/reidond/ilex:ms-7c35.gnome"),
        "serial line {:?}",
        full.selection
    );
    ensure!(
        board_only.detection.tag == "ms-7c35.gnome",
        "board only: {:?}",
        board_only.detection
    );
    ensure!(
        system_only.detection.tag == "ms-7c35.gnome",
        "system only: {:?}",
        system_only.detection
    );
    ensure!(
        lower.detection.tag == "generic.gnome" && lower.detection.matches.is_empty(),
        "case: {:?}",
        lower.detection
    );
    ensure!(
        half.detection.matches.is_empty(),
        "a vendor alone: {:?}",
        half.detection
    );
    ensure!(
        boot.detection.tag == "ms-7c35.gnome"
            && boot
                .selection
                .as_ref()
                .is_some_and(|s| s.tag == "generic.gnome" && s.reason == "chosen at boot"),
        "boot override: {:?}",
        boot.selection
    );
    ensure!(
        unknown.selection.is_none()
            && unknown.selection_error.as_deref().is_some_and(
                |e| e.starts_with("unknown composition 'generic.kde'; offered: generic.gnome")
            ),
        "unknown override: {:?} / {:?}",
        unknown.selection,
        unknown.selection_error
    );

    // The bus policy: only root and ilex-installer. One session: the
    // allowed callers, then every refusal.
    let mut scripts = vec![
        busctl("ilex-installer", "State", None),
        format!("busctl --system --json=short call {BUS} /org/ilex/Installer1 {BUS} State"),
        format!("busctl --no-pager --system status {BUS}"),
    ];
    let mut denied_calls = Vec::new();
    for user in ["nobody", "daemon"] {
        for method in ["Probe", "State"] {
            scripts.push(busctl(user, method, None));
            denied_calls.push(format!("{user} calling {method}"));
        }
        scripts.push(busctl(user, "Plan", Some("{}")));
        denied_calls.push(format!("{user} calling Plan"));
    }
    scripts.push(format!(
        "runuser -u nobody -- env SYSTEMD_COLORS=0 SYSTEMD_PAGER= busctl --system introspect {BUS} /org/ilex/Installer1"
    ));
    denied_calls.push("nobody introspecting".into());
    let ran = batch(vm, &scripts)?;
    let state = document(&ran[0], "State")?;
    ensure!(
        state["stage"] == "idle" && state["schema"] == 1,
        "state {state}"
    );
    ensure!(
        ran[1].code == 0 && ran[1].text.contains("idle"),
        "root's State: {}",
        ran[1].text
    );
    ensure!(
        ran[2]
            .text
            .lines()
            .any(|l| l.trim_start().starts_with("UID=0")),
        "the helper is not root: {}",
        ran[2].text
    );
    for (name, ran) in denied_calls.iter().zip(&ran[3..]) {
        ensure!(denied(ran), "{name}: exit {}, {}", ran.code, ran.text);
    }

    // Bus activation: with the helper stopped, the installer's own user starts
    // it by calling it; with it stopped again, a user the policy refuses is
    // denied. (dbus-broker starts an activatable service for any call to its
    // name and then applies the policy, so a stopped helper is started even by
    // a denied call: this is observed and reported, not asserted. Starting it is
    // harmless: it is started at boot anyway and answers only the allowed.)
    let stopped = "systemctl stop ilex-installerd; systemctl is-active ilex-installerd || true";
    let ran = batch(
        vm,
        &[
            stopped.into(),
            busctl("ilex-installer", "State", None),
            "systemctl is-active ilex-installerd".into(),
            stopped.into(),
            busctl("nobody", "Probe", None),
            "sleep 2; systemctl is-active ilex-installerd || true".into(),
            busctl("ilex-installer", "State", None),
        ],
    )?;
    ensure!(
        ran[0].text.trim() == "inactive" && ran[3].text.trim() == "inactive",
        "the helper did not stop: {} / {}",
        ran[0].text,
        ran[3].text
    );
    ensure!(
        document(&ran[1], "State")?["stage"] == "idle" && ran[2].text.trim() == "active",
        "the installer's call did not start the helper: {} / {}",
        ran[1].text,
        ran[2].text
    );
    ensure!(
        denied(&ran[4]),
        "nobody calling a stopped helper: exit {}, {}",
        ran[4].code,
        ran[4].text
    );
    println!(
        "     a denied call to the stopped helper: {} (the helper is {})",
        if denied(&ran[4]) {
            "Access denied"
        } else {
            "not denied"
        },
        ran[5].text.trim()
    );
    ensure!(
        document(&ran[6], "State")?["stage"] == "idle",
        "the helper does not answer after a refused call: {}",
        ran[6].text
    );

    old_linux_activation(vm)?;
    // One SSH session each: a long batch of them ended an SSH session on a step
    // that passes alone.
    for case in detached_uses().iter().chain(&held_uses()) {
        run_uses(vm, std::slice::from_ref(case))?;
    }
    Ok(())
}

/// The refusal of a plan request on a computer that booted through UEFI: the
/// tier's VM did not, so the helper is pointed at a firmware directory fixture
/// for the call (a BIOS boot is refused before anything else) and put back.
fn plan_refusal_on_uefi(vm: &EphemeralVm, request: &str) -> Result<Value> {
    let ran = batch(
        vm,
        &[
            format!(
                "rm -rf {FIXTURES}; mkdir -p {FIXTURES}/efi\n{}",
                restart_helper(&format!("--efi-dir {FIXTURES}/efi"))
            ),
            busctl("ilex-installer", "Plan", Some(request)),
            RESTORE_HELPER.into(),
        ],
    )?;
    wait_for_helper(vm)?;
    ensure!(
        ran[0].code == 0 && ran[2].code == 0,
        "pointing the helper at a firmware fixture and back: {} / {}",
        ran[0].text,
        ran[2].text
    );
    refusal_document(&ran[1])
}

/// Run a script in the guest and return what it printed.
fn guest_script(vm: &EphemeralVm, script: &str, what: &str) -> Result<String> {
    let out = vm
        .ssh(&format!(
            "printf %s {} | base64 -d | bash -s",
            base64(script.as_bytes())
        ))?
        .ok(what)?;
    Ok(printed(&out))
}

/// A disk of a probe, by name.
fn probed<'a>(
    probe: &'a Probe,
    name: &str,
) -> Result<&'a ilex_devkit::installer_model::probe::Disk> {
    probe
        .disks
        .iter()
        .find(|d| d.id == format!("virtio-{name}"))
        .with_context(|| format!("no disk {name}"))
}

/// The disks nothing uses are still offered: no `live_medium`, no `in_use`, no
/// `notes`. (A use that cannot be traced would mark every one of them.)
fn offered(probe: &Probe, names: &[&str]) -> Result<()> {
    for name in names {
        let disk = probed(probe, name)?;
        ensure!(
            !disk.live_medium && disk.in_use.is_empty() && disk.notes.is_empty(),
            "{name} is marked though nothing of it is in use: {disk:?}"
        );
    }
    Ok(())
}

/// A use that cannot be traced makes every disk "in use" (the installer cannot
/// tell whether it holds the medium or something in use): the named disks carry
/// that reason, whatever else they carry.
fn refused_everywhere(probe: &Probe, names: &[&str]) -> Result<()> {
    for name in names {
        let disk = probed(probe, name)?;
        ensure!(
            disk.in_use
                .iter()
                .any(|r| r.contains("cannot tell whether this disk holds the medium")),
            "{name} is offered though a use could not be traced: {disk:?}"
        );
    }
    Ok(())
}

/// What a reproduction asserts of the probe taken while it is in place, given
/// what its setup printed.
type Check<'a> = Box<dyn Fn(&str, &Probe) -> Result<()> + 'a>;

/// Whether a disk of a probe carries a reason containing `text`.
fn reason(disk: &ilex_devkit::installer_model::probe::Disk, text: &str) -> bool {
    disk.in_use.iter().any(|r| r.contains(text))
}

/// One reproduction of a use the mount table does not show, for real. Its
/// `setup` script runs in the guest, then the probe is read and, when `plan_on`
/// names a disk, the code its refusal must carry and a part of its message, a
/// plan on that disk is asked for (the helper run on a UEFI firmware directory
/// fixture for the call, then put back); `restore` always puts the guest back
/// as it was, and the shared state is checked again afterwards.
struct Use<'a> {
    what: &'a str,
    setup: String,
    restore: String,
    plan_on: Option<(&'a str, &'a str, &'a str)>,
    check: Check<'a>,
}

impl<'a> Use<'a> {
    fn new(
        what: &'a str,
        setup: &str,
        restore: &str,
        plan_on: Option<(&'a str, &'a str, &'a str)>,
        check: impl Fn(&str, &Probe) -> Result<()> + 'a,
    ) -> Self {
        Self {
            what,
            setup: setup.to_owned(),
            restore: restore.to_owned(),
            plan_on,
            check: Box::new(check),
        }
    }

    /// Setup, probe, [helper on a UEFI fixture, plan, helper back], restore,
    /// probe: where the restore is among them.
    fn scripts(&self) -> (Vec<String>, usize) {
        let mut scripts = vec![self.setup.clone(), busctl("ilex-installer", "Probe", None)];
        if let Some((disk, _, _)) = self.plan_on {
            scripts.push(format!(
                "rm -rf {FIXTURES}; mkdir -p {FIXTURES}/efi\n{}",
                restart_helper(&format!("--efi-dir {FIXTURES}/efi"))
            ));
            scripts.push(busctl(
                "ilex-installer",
                "Plan",
                Some(&plan_request(|r| {
                    r["disk"] = json!(format!("virtio-{disk}"))
                })),
            ));
            scripts.push(format!("{RESTORE_HELPER}\n{WAIT_FOR_HELPER}"));
        }
        let restore_at = scripts.len();
        scripts.push(self.restore.clone());
        scripts.push(busctl("ilex-installer", "Probe", None));
        (scripts, restore_at)
    }

    fn verify(&self, ran: &[Ran], restore_at: usize) -> Result<()> {
        let what = self.what;
        // The guest is put back whatever the checks find: read them after.
        ensure!(
            ran[restore_at].code == 0,
            "restoring the guest after {what} failed: {}",
            ran[restore_at].text
        );
        let result = (|| -> Result<()> {
            ensure!(ran[0].code == 0, "setting up failed: {}", ran[0].text);
            let probe: Probe = serde_json::from_value(document(&ran[1], "Probe")?)?;
            (self.check)(&ran[0].text, &probe)?;
            if let Some((_, code, text)) = self.plan_on {
                let refusal = refusal_document(&ran[3])?;
                ensure!(
                    refusal["code"] == code
                        && refusal["field"] == "disk"
                        && refusal["message"]
                            .as_str()
                            .is_some_and(|m| m.contains(text)),
                    "a plan on the disk was not refused as {code} with {text:?}: {refusal}"
                );
            }
            Ok(())
        })();
        result.with_context(|| what.to_owned())?;
        let probe: Probe = serde_json::from_value(document(&ran[restore_at + 1], "Probe")?)?;
        offered(&probe, &["blank", "layout", "oldlinux", "small"])?;
        for name in ["live", "isoscan", "ventoy"] {
            ensure!(
                probed(&probe, name)?.live_medium,
                "after {what}, {name} is no longer the live medium"
            );
        }
        ensure!(
            probed(&probe, "isoscan")?.in_use.is_empty(),
            "after {what}, the live medium is also in use"
        );
        Ok(())
    }
}

/// Run reproductions in one SSH session (each `bcvk ephemeral ssh` leaves a
/// podman exec session behind that holds an inotify instance until the VM goes,
/// and the host's default limit is 128: the tier shares one VM), each put back
/// before the next; then read what each found.
fn run_uses(vm: &EphemeralVm, uses: &[Use]) -> Result<()> {
    let mut scripts = Vec::new();
    let mut spans = Vec::new();
    for case in uses {
        let (own, restore_at) = case.scripts();
        spans.push((scripts.len(), restore_at));
        scripts.extend(own);
    }
    let ran = batch(vm, &scripts)?;
    let mut failures = Vec::new();
    for (case, (start, restore_at)) in uses.iter().zip(spans) {
        let end = start + case.scripts().0.len();
        if let Err(e) = case.verify(&ran[start..end], restore_at) {
            failures.push(format!("{e:#}"));
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("\n\n"));
    Ok(())
}

/// Uses of a disk the mount table does not show, each reproduced with real
/// devices and mounts, and what the helper makes of them. A disk is offered
/// only if every use of block storage is traced and none reaches it; what is
/// traced is not offered, and what is not touched stays offered:
///
/// * a loop device attached to a block device node (`losetup /dev/vde4`): the
///   kernel records the device of `/dev` and the node's inode, not the disk;
/// * a loop device attached to a file that nothing mounts, whose file system
///   was unmounted lazily (the kernel then prints a name relative to the
///   detached mount, `/x.img`, which `/` contains);
/// * dracut's live layout (an ISO file on a mounted partition, attached to a
///   loop device mounted as the medium) with either mount unmounted lazily.
fn detached_uses() -> Vec<Use<'static>> {
    let mut uses = Vec::new();
    // A loop device on a partition's device node.
    uses.push(Use::new(
        "a loop device on a block device node",
        r#"set -eu
node=$(readlink -f /dev/disk/by-id/virtio-layout-part4)
loop=$(losetup --find --show --read-only "$node")
echo "$loop" > /run/ilex-test/node.loop
echo "node $node backing [$(cat /sys/block/$(basename "$loop")/loop/backing_file)]"
losetup --json --list --output NAME,BACK-FILE,BACK-INO,BACK-MAJ:MIN
"#,
        r#"loop=$(cat /run/ilex-test/node.loop 2>/dev/null || true)
[ -z "$loop" ] || losetup -d "$loop" || true
rm -f /run/ilex-test/node.loop
"#,
        Some(("layout", "in-use", "")),
        |made, probe| {
            ensure!(
                made.contains("backing [/dev/vd"),
                "the kernel names the node differently: {made}"
            );
            let layout = probed(probe, "layout")?;
            ensure!(
                !layout.live_medium && reason(layout, "is attached to /dev/vd"),
                "a loop device on the layout disk's partition is not reported: {layout:?}"
            );
            offered(probe, &["blank", "oldlinux", "small"])?;
            println!(
                "     loop device on a block device node: virtio-layout live medium {}, in use {:?}",
                layout.live_medium, layout.in_use
            );
            // (not a target: a plan on it is refused `in-use`)
            Ok(())
        },
    ));
    // A loop device nothing mounts, on a file whose file system is detached.
    uses.push(Use::new(
        "an unmounted loop device on a file of a lazily unmounted file system",
        r#"set -eu
d=/dev/disk/by-id/virtio-small
mkfs.ext4 -q -F -L smallfs "$d"
mkdir -p /run/ilex-test/small
mount "$d" /run/ilex-test/small
truncate -s 16M /run/ilex-test/small/x.img
loop=$(losetup --find --show --read-only /run/ilex-test/small/x.img)
echo "$loop" > /run/ilex-test/small.loop
umount -l /run/ilex-test/small
! mountpoint -q /run/ilex-test/small
echo "backing [$(cat /sys/block/$(basename "$loop")/loop/backing_file)] mounted [$(grep -c "^$loop " /proc/mounts || true)]"
"#,
        r#"loop=$(cat /run/ilex-test/small.loop 2>/dev/null || true)
[ -z "$loop" ] || losetup -d "$loop" || true
rm -f /run/ilex-test/small.loop
umount -l /run/ilex-test/small 2>/dev/null || true
rmdir /run/ilex-test/small 2>/dev/null || true
# an exclusive open fails while anything still holds the device (the file system
# a lazy unmount left behind goes with the loop device, a moment later)
for i in $(seq 1 60); do dd if=/dev/zero of=/dev/disk/by-id/virtio-small bs=1M count=8 oflag=excl conv=fsync status=none 2>/dev/null && break; sleep 1; done
udevadm settle
"#,
        None,
        |made, probe| {
            ensure!(
                made.contains("backing [/x.img] mounted [0]"),
                "the kernel names the detached file differently: {made}"
            );
            let small = probed(probe, "small")?;
            ensure!(
                !small.live_medium && reason(small, "is attached to /x.img"),
                "the unmounted loop device's disk is not reported: {small:?}"
            );
            // The file is not at that name any more and its file system (ext4) can
            // hold a block device node: the kernel's numbers do not say which disk
            // the loop device is really on, so no disk is offered.
            refused_everywhere(probe, &["blank", "layout", "oldlinux", "small"])?;
            println!(
                "     unmounted loop device on a lazily unmounted file system: virtio-small in use {:?}; every disk refused",
                small.in_use
            );
            Ok(())
        },
    ));
    // dracut's live layout: the disk the ISO file is on (mounted at
    // `/run/initramfs/isoscan`, as the medium's loop device is at
    // `/run/initramfs/live`), with the host disk's mount unmounted lazily, the
    // medium's, and then both.
    // A loop device keeps the name the kernel gave its file when the file
    // system holding it was detached (`/live.iso`, relative to the mount that
    // is gone), whatever is mounted there afterwards: putting the layout back
    // means attaching the ISO again.
    let restore_live = r#"d=/dev/disk/by-id
loop=$(cat /run/ilex-test/medium.loop 2>/dev/null || true)
umount /run/initramfs/live 2>/dev/null || true
[ -z "$loop" ] || losetup -d "$loop" || true
umount /run/initramfs/isoscan 2>/dev/null || true
# a file system unmounted lazily goes when the loop device that held its file does
for i in $(seq 1 60); do dd if=/dev/zero of="$d/virtio-isoscan-part1" bs=512 count=0 oflag=excl conv=notrunc status=none 2>/dev/null && break; sleep 1; done
mount "$d/virtio-isoscan-part1" /run/initramfs/isoscan
loop=$(losetup --find --show --read-only /run/initramfs/isoscan/live.iso)
echo "$loop" > /run/ilex-test/medium.loop
mount -t iso9660 -o ro "$loop" /run/initramfs/live
mountpoint -q /run/initramfs/isoscan && mountpoint -q /run/initramfs/live && echo restored
"#;
    uses.push(Use::new(
        "dracut's host disk mount unmounted lazily",
        "set -eu\numount -l /run/initramfs/isoscan\n! mountpoint -q /run/initramfs/isoscan\ncat /sys/block/$(basename \"$(findmnt -n -o SOURCE /run/initramfs/live)\")/loop/backing_file\n",
        restore_live,
        Some(("isoscan", "live-medium", "")),
        |made, probe| {
            // The kernel prints the file relative to the detached mount.
            ensure!(
                made.trim().ends_with("/live.iso") && !made.contains("isoscan"),
                "the kernel names the detached file differently: {made}"
            );
            let iso = probed(probe, "isoscan")?;
            ensure!(
                iso.live_medium,
                "the live medium is not found through its loop device's numbers: {iso:?}"
            );
            // The medium's loop device is on a file that is not at its name any
            // more, on a file system (ext4) that can hold a device node: the
            // medium is still known, and nothing else is offered.
            refused_everywhere(probe, &["blank", "layout", "oldlinux", "small"])?;
            println!(
                "     host disk mount unmounted lazily: virtio-isoscan live medium {}, in use {:?}; every disk refused",
                iso.live_medium, iso.in_use
            );
            Ok(())
        },
    ));
    uses.push(Use::new(
        "dracut's medium mount unmounted lazily",
        "set -eu\numount -l /run/initramfs/live\n! mountpoint -q /run/initramfs/live\nlosetup -j /run/initramfs/isoscan/live.iso -O NAME -n\n",
        restore_live,
        Some(("isoscan", "live-medium", "")),
        |made, probe| {
            ensure!(
                made.trim().starts_with("/dev/loop"),
                "the loop device is gone: {made}"
            );
            let iso = probed(probe, "isoscan")?;
            ensure!(
                iso.live_medium && iso.in_use.is_empty(),
                "the host disk is not the live medium: {iso:?}"
            );
            offered(probe, &["blank", "layout", "oldlinux", "small"])?;
            println!(
                "     medium mount unmounted lazily: virtio-isoscan live medium {}, in use {:?}",
                iso.live_medium, iso.in_use
            );
            Ok(())
        },
    ));
    uses.push(Use::new(
        "both of dracut's mounts unmounted lazily",
        "set -eu\numount -l /run/initramfs/live\numount -l /run/initramfs/isoscan\n! mountpoint -q /run/initramfs/live\n! mountpoint -q /run/initramfs/isoscan\nlosetup -j /run/initramfs/isoscan/live.iso -O NAME -n 2>&1 || losetup -l -O NAME,BACK-FILE\n",
        restore_live,
        Some(("isoscan", "in-use", "")),
        |_, probe| {
            // Only the attached loop device is left of the medium: its disk is
            // in use (not offered) and nothing else is touched.
            let iso = probed(probe, "isoscan")?;
            ensure!(
                reason(iso, "is attached to /live.iso"),
                "the loop device that is all that is left of the medium is not reported: {iso:?}"
            );
            refused_everywhere(probe, &["blank", "layout", "oldlinux", "small"])?;
            println!(
                "     both mounts unmounted lazily: virtio-isoscan live medium {}, in use {:?}",
                iso.live_medium, iso.in_use
            );
            Ok(())
        },
    ));
    uses
}

/// The disks of the uses below, put back after each: the partitions of the
/// disks the plan tests use are never changed, and the 8 GiB disk is scratch.
const WAIT_FREE_SMALL: &str = r#"for i in $(seq 1 60); do dd if=/dev/zero of=/dev/disk/by-id/virtio-small bs=1M count=8 oflag=excl conv=fsync status=none 2>/dev/null && break; sleep 1; done
udevadm settle
"#;

/// What the kernel says of a whole disk when it is opened exclusively, for
/// the reproductions below to print (the installer's own last look is the same
/// open).
const CLAIM_OF_SMALL: &str = r#"if dd if=/dev/zero of=/dev/disk/by-id/virtio-small bs=512 count=0 oflag=excl conv=notrunc status=none 2>/dev/null; then echo "claim free"; else echo "claim busy"; fi"#;

/// Uses of a disk that the trace of mounts, swap and loop devices is blind to
/// or that the host's own tables hide from the helper, each reproduced with
/// real devices, mounts and processes, and what the helper makes of them:
///
/// * a block device node made on a tmpfs, attached to a loop device and
///   deleted: the kernel records an inode of the tmpfs, which could be a node
///   of any disk, so no disk is offered;
/// * zram writing back to a disk (the kernel claims it, and shows no link);
/// * an ext4 whose journal is on a disk of its own (in no mount option);
/// * a mount made under a `--make-private` subtree of the host's namespace,
///   which the helper's own (`PrivateMounts=yes`) never sees;
/// * a file system unmounted lazily that a process still holds, which no table
///   lists: the exclusive open of the disk is refused by the kernel.
fn held_uses() -> Vec<Use<'static>> {
    let mut uses = Vec::new();
    uses.push(Use::new(
        "a deleted block device node on a tmpfs attached to a loop device",
        r#"set -eu
mkdir -p /run/ilex-test/tmpfs-node
mount -t tmpfs -o mode=755 none /run/ilex-test/tmpfs-node
echo "vfs options [$(grep ' /run/ilex-test/tmpfs-node ' /proc/self/mountinfo | cut -d' ' -f6)]"
node=$(readlink -f /dev/disk/by-id/virtio-layout-part4)
cp -a "$node" /run/ilex-test/tmpfs-node/n
test -b /run/ilex-test/tmpfs-node/n
loop=$(losetup --find --show --read-only /run/ilex-test/tmpfs-node/n)
echo "$loop" > /run/ilex-test/tmpfs-node.loop
rm /run/ilex-test/tmpfs-node/n
echo "backing [$(cat /sys/block/$(basename "$loop")/loop/backing_file)]"
"#,
        r#"loop=$(cat /run/ilex-test/tmpfs-node.loop 2>/dev/null || true)
[ -z "$loop" ] || losetup -d "$loop" || true
rm -f /run/ilex-test/tmpfs-node.loop
umount /run/ilex-test/tmpfs-node 2>/dev/null || true
rmdir /run/ilex-test/tmpfs-node 2>/dev/null || true
"#,
        Some(("blank", "in-use", "cannot tell whether this disk")),
        |made, probe| {
            ensure!(
                !made.contains("nodev") && made.contains("tmpfs-node/n (deleted)]"),
                "the fixture is not a deleted node on a tmpfs without nodev: {made}"
            );
            refused_everywhere(probe, &["blank", "layout", "oldlinux", "small"])?;
            println!(
                "     deleted block node on a tmpfs, on a loop device: every disk refused ({:?})",
                probed(probe, "blank")?.in_use
            );
            Ok(())
        },
    ));
    uses.push(Use::new(
        "zram writing back to a disk",
        &format!(
            r#"set -eu
modprobe zram 2>/dev/null || true
id=$(cat /sys/class/zram-control/hot_add)
echo "$id" > /run/ilex-test/zram.id
echo /dev/disk/by-id/virtio-small > /sys/block/zram$id/backing_dev
echo 64M > /sys/block/zram$id/disksize
echo "zram$id backing [$(cat /sys/block/zram$id/backing_dev)]"
{CLAIM_OF_SMALL}
"#
        ),
        &format!(
            r#"id=$(cat /run/ilex-test/zram.id 2>/dev/null || true)
if [ -n "$id" ]; then echo 1 > /sys/block/zram$id/reset || true; echo "$id" > /sys/class/zram-control/hot_remove || true; fi
rm -f /run/ilex-test/zram.id
{WAIT_FREE_SMALL}"#
        ),
        Some(("small", "in-use", "writes back to")),
        |made, probe| {
            ensure!(
                made.contains("backing [/dev/vd") && made.contains("claim busy"),
                "the kernel does not hold the writeback disk: {made}"
            );
            let small = probed(probe, "small")?;
            ensure!(
                !small.live_medium && reason(small, "writes back to /dev/vd"),
                "zram's writeback disk is not reported: {small:?}"
            );
            offered(probe, &["blank", "layout", "oldlinux"])?;
            println!(
                "     zram writeback: virtio-small in use {:?}",
                small.in_use
            );
            Ok(())
        },
    ));
    uses.push(Use::new(
        "an ext4 with an external journal",
        &format!(
            r#"set -eu
d=/dev/disk/by-id
mkdir -p /run/ilex-test/extj
wipefs -a -q "$d/virtio-small"
mke2fs -q -F -b 4096 -O journal_dev "$d/virtio-small"
mke2fs -q -F -b 4096 -t ext4 -J device="$d/virtio-small" -L extjournal "$d/virtio-oldlinux-part1"
mount "$d/virtio-oldlinux-part1" /run/ilex-test/extj
echo "options [$(grep ' /run/ilex-test/extj ' /proc/self/mountinfo | sed 's/.* - //')]"
echo "jbd2 [$(ls /proc/fs/jbd2 | tr '
' ' ')]"
{CLAIM_OF_SMALL}
"#
        ),
        &format!(
            r#"umount /run/ilex-test/extj 2>/dev/null || true
rmdir /run/ilex-test/extj 2>/dev/null || true
{WAIT_FREE_SMALL}wipefs -a -q /dev/disk/by-id/virtio-small || true
mkfs.ext4 -q -F -L oldboot /dev/disk/by-id/virtio-oldlinux-part1
udevadm settle
"#
        ),
        Some(("small", "in-use", "holds the journal")),
        |made, probe| {
            // The mount options say nothing of the journal: only /proc/fs/jbd2 does.
            ensure!(
                made.contains("options [")
                    && !made.contains("journal")
                    && made.contains("claim busy"),
                "the fixture is not an external journal the mount options hide: {made}"
            );
            let small = probed(probe, "small")?;
            ensure!(
                !small.live_medium && reason(small, "holds the journal of a mounted file system"),
                "the journal's disk is not reported: {small:?}"
            );
            ensure!(
                reason(
                    probed(probe, "oldlinux")?,
                    "is mounted at /run/ilex-test/extj"
                ),
                "the file system's disk is not reported: {:?}",
                probed(probe, "oldlinux")?
            );
            offered(probe, &["blank", "layout"])?;
            println!(
                "     external journal: virtio-small in use {:?}, virtio-oldlinux in use {:?}",
                small.in_use,
                probed(probe, "oldlinux")?.in_use
            );
            Ok(())
        },
    ));
    uses.push(Use::new(
        "a mount made under a private subtree",
        r#"set -eu
d=/dev/disk/by-id
mkdir -p /run/ilex-test/private
mount --bind /run/ilex-test/private /run/ilex-test/private
mount --make-private /run/ilex-test/private
mkdir -p /run/ilex-test/private/data
mount -o ro "$d/virtio-layout-part4" /run/ilex-test/private/data
pid=$(systemctl show -p MainPID --value ilex-installerd)
echo "host sees $(grep -c ' /run/ilex-test/private/data ' /proc/1/mountinfo) helper sees $(nsenter --target "$pid" --mount grep -c ' /run/ilex-test/private/data ' /proc/self/mountinfo || true)"
"#,
        r#"umount /run/ilex-test/private/data 2>/dev/null || true
umount /run/ilex-test/private 2>/dev/null || true
rmdir /run/ilex-test/private/data /run/ilex-test/private 2>/dev/null || true
"#,
        Some(("layout", "in-use", "/run/ilex-test/private/data")),
        |made, probe| {
            ensure!(
                made.trim().ends_with("host sees 1 helper sees 0"),
                "the mount is visible to the helper's own namespace (the fixture proves nothing): {made}"
            );
            let layout = probed(probe, "layout")?;
            ensure!(
                !layout.live_medium && reason(layout, "is mounted at /run/ilex-test/private/data"),
                "a mount the helper's own namespace does not have is not reported: {layout:?}"
            );
            offered(probe, &["blank", "oldlinux", "small"])?;
            println!(
                "     mount under a private subtree: virtio-layout in use {:?}",
                layout.in_use
            );
            Ok(())
        },
    ));
    uses.push(Use::new(
        "a file system unmounted lazily that a process still holds",
        r#"set -eu
d=/dev/disk/by-id
mkfs.ext4 -q -F -L lazyheld "$d/virtio-small"
mkdir -p /run/ilex-test/lazyheld
mount "$d/virtio-small" /run/ilex-test/lazyheld
cd /run/ilex-test/lazyheld
setsid sh -c 'echo $$ > /run/ilex-test/lazyheld.pid; exec sleep 3600' </dev/null >/dev/null 2>&1 &
for i in $(seq 1 50); do [ -s /run/ilex-test/lazyheld.pid ] && break; sleep 0.1; done
cd /
umount -l /run/ilex-test/lazyheld
! mountpoint -q /run/ilex-test/lazyheld
echo "mounted [$(grep -c lazyheld /proc/self/mountinfo || true)] held by $(cat /run/ilex-test/lazyheld.pid)"
"#,
        &format!(
            r#"pid=$(cat /run/ilex-test/lazyheld.pid 2>/dev/null || true)
[ -z "$pid" ] || kill "$pid" 2>/dev/null || true
rm -f /run/ilex-test/lazyheld.pid
rmdir /run/ilex-test/lazyheld 2>/dev/null || true
{WAIT_FREE_SMALL}"#
        ),
        Some(("small", "in-use", "held open exclusively")),
        |made, probe| {
            ensure!(
                made.contains("mounted [0]"),
                "the file system is still mounted: {made}"
            );
            let small = probed(probe, "small")?;
            // No table lists it: only the kernel's own claim on the disk does.
            ensure!(
                !small.live_medium
                    && reason(small, "is held open exclusively by the running system")
                    && !reason(small, "is mounted at")
                    && !reason(small, "is attached to")
                    && !reason(small, "cannot tell whether"),
                "a file system held after a lazy unmount is not refused by the kernel's claim: {small:?}"
            );
            offered(probe, &["blank", "layout", "oldlinux"])?;
            println!(
                "     lazily unmounted file system held by a process: virtio-small in use {:?}",
                small.in_use
            );
            Ok(())
        },
    ));
    uses
}

/// The guest's view of the old installation's volume group (`vgold`, two
/// volumes) and array (`md0`), and the udev event a disk appearing at boot
/// (or hotplugged) causes: `add` for the disk and each partition.
const OLD_LINUX: &str = r#"d=/dev/disk/by-id/virtio-oldlinux
active() { printf 'lvm=%s md=%s\n' "$(dmsetup ls 2>/dev/null | grep -c '^vgold-')" "$(grep -c '^md' /proc/mdstat)"; }
trigger() { udevadm trigger --action=add "$(readlink -f $d)" $(readlink -f $d-part*); udevadm settle; }
release() { vgchange -an vgold >/dev/null 2>&1 || true; mdadm --stop /dev/md0 >/dev/null 2>&1 || true; udevadm settle; }
"#;

/// Put the guest back as the image ships it: the live image's LVM and MD
/// configuration in place, the old installation inactive.
const OLD_LINUX_RESTORE: &str = r#"[ ! -e /etc/lvm/lvmlocal.conf.off ] || mv /etc/lvm/lvmlocal.conf.off /etc/lvm/lvmlocal.conf
[ ! -e /etc/mdadm.conf.off ] || mv /etc/mdadm.conf.off /etc/mdadm.conf
release
trigger
sleep 5
udevadm settle
echo "restored $(active)"
"#;

/// A disk that carries an old Linux installation is offered like any other: the
/// live image does not activate its LVM volume group or MD array (the udev
/// event of a disk appearing starts neither), so nothing holds it. A control
/// with the image's configuration out of the way shows that the same event
/// does activate them otherwise, and a disk the system really does hold stays
/// refused; with the configuration back, it is offered again.
fn old_linux_activation(vm: &EphemeralVm) -> Result<()> {
    let result = old_linux_activation_body(vm);
    let restored = guest_script(
        vm,
        &format!("{OLD_LINUX}{OLD_LINUX_RESTORE}"),
        "restoring the old installation's state",
    );
    result?;
    ensure!(
        restored?.trim().ends_with("restored lvm=0 md=0"),
        "the old installation could not be put back inactive"
    );
    let old = probe_document(vm)?
        .disks
        .into_iter()
        .find(|d| d.id == "virtio-oldlinux")
        .context("no oldlinux disk")?;
    ensure!(
        old.in_use.is_empty() && !old.live_medium,
        "oldlinux after the control: {old:?}"
    );
    Ok(())
}

fn old_linux_activation_body(vm: &EphemeralVm) -> Result<()> {
    // As shipped: the event leaves everything inactive.
    let shipped = guest_script(
        vm,
        &format!(
            "{OLD_LINUX}release\ntrigger\nsleep 5\nudevadm settle\necho \"config $(lvmconfig --type full global/event_activation) $(grep -c '^AUTO -all' /etc/mdadm.conf)\"\necho \"shipped $(active)\"\n"
        ),
        "the event of the old disk appearing",
    )?;
    ensure!(
        shipped.contains("config event_activation=0 1")
            && shipped.trim().ends_with("shipped lvm=0 md=0"),
        "the live image activated the old installation: {shipped}"
    );
    let probe = probe_document(vm)?;
    let old = probe
        .disks
        .iter()
        .find(|d| d.id == "virtio-oldlinux")
        .context("no oldlinux disk")?;
    ensure!(
        old.in_use.is_empty() && !old.live_medium && old.notes.is_empty(),
        "the old installation's disk is not offered: {old:?}"
    );
    println!(
        "     virtio-oldlinux: LVM volume group and MD array not activated, in use {:?}",
        old.in_use
    );

    // Control: without the image's configuration the same event activates them,
    // and the disk, now held, is refused.
    let control = guest_script(
        vm,
        &format!(
            "{OLD_LINUX}mv /etc/lvm/lvmlocal.conf /etc/lvm/lvmlocal.conf.off\nmv /etc/mdadm.conf /etc/mdadm.conf.off\ntrigger\nfor i in $(seq 1 30); do [ \"$(active)\" = 'lvm=2 md=1' ] && break; sleep 1; done\necho \"control $(active)\"\n"
        ),
        "the control: the event without the image's configuration",
    )?;
    ensure!(
        control.trim().ends_with("control lvm=2 md=1"),
        "the control did not activate the old installation (the check proves nothing): {control}"
    );
    let probe = probe_document(vm)?;
    let held = probe
        .disks
        .iter()
        .find(|d| d.id == "virtio-oldlinux")
        .context("no oldlinux disk")?;
    ensure!(
        held.in_use.iter().any(|r| r.contains("is used by dm-"))
            && held.in_use.iter().any(|r| r.contains("is used by md0")),
        "a held old installation is not reported: {held:?}"
    );
    println!("     virtio-oldlinux, activated: in use {:?}", held.in_use);
    let refusal =
        plan_refusal_on_uefi(vm, &plan_request(|r| r["disk"] = json!("virtio-oldlinux")))?;
    ensure!(
        refusal["code"] == "in-use" && refusal["field"] == "disk",
        "a held disk was not refused: {refusal}"
    );
    Ok(())
}

// --- installer::plan --------------------------------------------------------

/// Every disk's partition table, first 4 MiB and signature probe, to prove a
/// plan wrote nothing.
fn disk_fingerprints(vm: &EphemeralVm) -> Result<String> {
    let out = vm
        .ssh(
            "for n in blank layout oldlinux small; do d=/dev/disk/by-id/virtio-$n; echo \"$n $(sfdisk --dump \"$d\" 2>&1 | sed 's/^device:.*//' | sha256sum) $(head -c 4194304 \"$d\" | sha256sum) $(blkid -p \"$d\" >/dev/null 2>&1; echo blkid=$?)\"; done",
        )?
        .ok("fingerprinting the disks")?;
    Ok(printed(&out))
}

/// `installer::finish`: every way `Finish` refuses, and the policy that lets
/// only the installer's user ask.
fn finish(vm: &EphemeralVm) -> Result<()> {
    // The disks a trial before this one touched settle (its volume groups and
    // arrays are put away asynchronously): the starting point is the first
    // fingerprint that stays the same.
    let mut before = disk_fingerprints(vm)?;
    for _ in 0..15 {
        std::thread::sleep(std::time::Duration::from_secs(2));
        let again = disk_fingerprints(vm)?;
        if again == before {
            break;
        }
        before = again;
    }
    let refused: [(&str, &str); 11] = [
        ("", "invalid-field"),
        ("bogus", "invalid-field"),
        ("Restart", "invalid-field"),
        ("restart ", "invalid-field"),
        ("save-log:", "invalid-field"),
        ("save-log:../x", "invalid-field"),
        ("save-log:a/b", "invalid-field"),
        ("save-log:virtio-blank", "not-removable"),
        ("save-log:virtio-live", "live-medium"),
        ("save-log:virtio-isoscan", "live-medium"),
        ("save-log:usb-nothing-0:0", "unknown-disk"),
    ];
    let scripts: Vec<String> = refused
        .iter()
        .map(|(action, _)| busctl("ilex-installer", "Finish", Some(action)))
        .chain([
            busctl("nobody", "Finish", Some("restart")),
            busctl("root", "State", None),
        ])
        .collect();
    let ran = batch(vm, &scripts)?;
    for ((action, code), ran) in refused.iter().zip(&ran) {
        let document = refusal_document(ran).with_context(|| format!("Finish({action:?})"))?;
        ensure!(
            document["code"] == *code && document["field"].as_str().is_some(),
            "Finish({action:?}) is refused as {document}, expected {code}"
        );
        // Not a word of what was asked comes back in the refusal.
        ensure!(
            action.len() <= 9
                || !document["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains(action),
            "the refusal of {action:?} quotes it: {document}"
        );
    }
    ensure!(
        denied(&ran[refused.len()]),
        "the bus let another user call Finish: {}",
        ran[refused.len()].text
    );
    let state = document(&ran[refused.len() + 1], "State")?;
    ensure!(
        state["stage"] == "idle" && state["active"] == false && state["summary"].is_null(),
        "Finish changed the helper's state: {state}"
    );
    let after = disk_fingerprints(vm)?;
    ensure!(
        after == before,
        "a refused Finish wrote to a disk:\nbefore: {before}\nafter:  {after}"
    );
    ensure!(
        no_scratch_left(vm)?,
        "a refused Finish left its scratch directory behind"
    );
    wait_for_helper(vm)?;
    Ok(())
}

/// `installer::plan`: the helper is run against a firmware fixture for the
/// test and put back as the image ships it, whatever happens.
fn plan(vm: &EphemeralVm) -> Result<()> {
    let result = plan_body(vm);
    let restored = vm
        .ssh(RESTORE_HELPER)
        .and_then(|out| out.ok("restoring the helper"));
    let waited = wait_for_helper(vm);
    result?;
    restored?;
    waited
}

fn plan_body(vm: &EphemeralVm) -> Result<()> {
    let before = disk_fingerprints(vm)?;

    // UEFI only. Whether this VM booted through UEFI is asserted by
    // installer::probe, against what the guest says; here the helper looks
    // for the firmware directory in a fixture, absent for the first request
    // (a BIOS boot) and present for all the others.
    let ran = batch(
        vm,
        &[
            format!(
                "rm -rf {FIXTURES}; mkdir -p {FIXTURES}\n{}",
                restart_helper(&format!("--efi-dir {FIXTURES}/efi"))
            ),
            busctl("ilex-installer", "Probe", None),
            busctl("ilex-installer", "Plan", Some(&plan_request(|_| {}))),
            format!("mkdir -p {FIXTURES}/efi"),
        ],
    )?;
    ensure!(ran[0].code == 0, "restarting the helper: {}", ran[0].text);
    let bios: Probe = serde_json::from_value(document(&ran[1], "Probe")?)?;
    ensure!(!bios.firmware.uefi, "firmware {:?}", bios.firmware);
    let refusal = refusal_document(&ran[2])?;
    ensure!(
        refusal["code"] == "unsupported-firmware"
            && refusal["message"]
                .as_str()
                .is_some_and(|m| m.contains("BIOS")),
        "a BIOS boot was not refused: {refusal}"
    );
    ensure!(
        ran[3].code == 0,
        "making the firmware directory: {}",
        ran[3].text
    );

    let probe = probe_document(vm)?;
    ensure!(probe.firmware.uefi, "firmware {:?}", probe.firmware);
    let blank_path = probe
        .disks
        .iter()
        .find(|d| d.id == "virtio-blank")
        .context("no blank disk")?
        .path
        .clone();

    // Whole disk, unencrypted, on the blank disk.
    let plain: Plan = serde_json::from_value(call(vm, "Plan", Some(&plan_request(|_| {})))?)?;
    ensure!(
        plain.schema == 1
            && plain.id.len() == 32
            && plain.id.bytes().all(|b| b.is_ascii_hexdigit()),
        "plan id {}",
        plain.id
    );
    ensure!(
        plain.valid_seconds == 900 && plain.edition == "gnome",
        "plan header"
    );
    ensure!(
        plain.request.keyboard == "us"
            && plain.request.language == "en_US.UTF-8"
            && plain.request.time_zone == "Etc/UTC"
            && plain.request.host_name == "ilex"
            && plain.request.tpm == Some(ilex_devkit::installer_model::plan::TpmMode::Off)
            && !plain.request.encrypt,
        "normalised request {:?}",
        plain.request
    );
    ensure!(
        plain.composition.tag == "generic.gnome"
            && plain.composition.machine == "generic"
            && plain.composition.shells == ["gnome"],
        "composition {:?}",
        plain.composition
    );
    let layout: Vec<(u32, &str, &str, &str, u64, &str, bool)> = plain
        .partitions
        .iter()
        .map(|p| {
            (
                p.number,
                p.kind.as_str(),
                p.label.as_str(),
                p.path.as_str(),
                p.size_bytes,
                p.filesystem.as_str(),
                p.encrypted,
            )
        })
        .collect();
    let p = |n: u32| format!("{blank_path}{n}");
    let root_size = 40 * GIB - 20480 - (MIB + 512 * MIB + GIB);
    ensure!(
        layout
            == [
                (
                    1,
                    "esp",
                    "ilex-esp",
                    p(1).as_str(),
                    512 * MIB,
                    "vfat",
                    false
                ),
                (
                    2,
                    "xbootldr",
                    "ilex-boot",
                    p(2).as_str(),
                    GIB,
                    "ext4",
                    false
                ),
                (
                    3,
                    "root-x86-64",
                    "ilex-root",
                    p(3).as_str(),
                    root_size,
                    "btrfs",
                    false
                ),
            ],
        "partitions {layout:?}"
    );
    ensure!(
        plain.partitions[0].start_bytes == MIB
            && plain.partitions[1].start_bytes == MIB + 512 * MIB
            && plain.partitions[2].start_bytes == MIB + 512 * MIB + GIB,
        "partition offsets"
    );
    ensure!(
        plain.partitions.iter().all(|p| p.uuid.len() == 36),
        "partition uuids"
    );
    ensure!(
        plain.disk.id == "virtio-blank"
            && plain.disk.size_bytes == 40 * GIB
            && plain.erases.partitions.is_empty()
            && plain.erases.systems.is_empty(),
        "disk {:?} erases {:?}",
        plain.disk,
        plain.erases
    );
    ensure!(
        !plain.encryption.enabled && !plain.encryption.tpm.will_enroll,
        "encryption {:?}",
        plain.encryption
    );

    // The image is what the registry answers for the tag: independently resolved here.
    let registry = vm
        .ssh("skopeo inspect --raw docker://ghcr.io/reidond/ilex:generic.gnome | sha256sum | cut -d' ' -f1")?
        .ok("resolving the tag in the guest")?;
    ensure!(
        plain.image.digest == format!("sha256:{}", registry.out())
            && plain.image.source == format!("ghcr.io/reidond/ilex@{}", plain.image.digest)
            && plain.image.target == "ghcr.io/reidond/ilex:generic.gnome"
            && plain.image.download_bytes > 100 * MIB
            && plain
                .image
                .version
                .as_deref()
                .is_some_and(|v| !v.is_empty())
            && plain
                .image
                .revision
                .as_deref()
                .is_some_and(|r| r.len() == 40),
        "image {:?} (registry {})",
        plain.image,
        registry.out()
    );
    println!(
        "     {} is {} ({} MiB to download)",
        plain.image.target,
        plain.image.digest,
        plain.image.download_bytes / MIB
    );

    // Encrypted: the same layout, the root inside LUKS; the unlock method is the probe's verdict.
    let encrypted: Plan = serde_json::from_value(call(
        vm,
        "Plan",
        Some(&plan_request(|r| r["encrypt"] = json!(true))),
    )?)?;
    ensure!(
        encrypted
            .partitions
            .iter()
            .map(|p| p.encrypted)
            .collect::<Vec<_>>()
            == [false, false, true]
            && encrypted
                .partitions
                .iter()
                .map(|p| p.size_bytes)
                .sum::<u64>()
                == plain.partitions.iter().map(|p| p.size_bytes).sum::<u64>()
            && encrypted.encryption.enabled
            && encrypted.request.tpm == Some(ilex_devkit::installer_model::plan::TpmMode::Auto)
            && !encrypted.encryption.tpm.will_enroll
            && encrypted.encryption.tpm.reason == "secure-boot-off"
            && encrypted.warnings.iter().any(|w| w.contains("passphrase")),
        "encrypted plan {:?}",
        encrypted.encryption
    );
    ensure!(
        encrypted.id != plain.id && encrypted.seed != plain.seed,
        "plans share an id or a seed"
    );
    let state = call(vm, "State", None)?;
    ensure!(
        state["plan"] == json!(encrypted.id) && state["stage"] == "idle",
        "state after planning: {state}"
    );

    // The layout disk: what would be erased, and the warning that says so.
    let layout_plan: Plan = serde_json::from_value(call(
        vm,
        "Plan",
        Some(&plan_request(|r| {
            r["disk"] = json!("virtio-layout");
            r["composition"] = json!("generic.gnome");
        })),
    )?)?;
    ensure!(
        layout_plan.erases.partitions.len() == 4 && layout_plan.erases.systems.len() == 5,
        "erases {:?}",
        layout_plan.erases
    );
    ensure!(
        layout_plan
            .warnings
            .iter()
            .any(|w| w.contains("other system(s) found on it")),
        "warnings {:?}",
        layout_plan.warnings
    );

    // The old Linux installation's disk: offered and plannable, its LVM and MD
    // volumes listed among what would be erased. Nothing activated them.
    let old_plan: Plan = serde_json::from_value(call(
        vm,
        "Plan",
        Some(&plan_request(|r| r["disk"] = json!("virtio-oldlinux"))),
    )?)?;
    ensure!(
        old_plan.disk.id == "virtio-oldlinux"
            && old_plan.erases.partitions.len() == 4
            && old_plan
                .erases
                .systems
                .iter()
                .any(|s| s.description.starts_with("LVM2_member"))
            && old_plan
                .erases
                .systems
                .iter()
                .any(|s| s.description.starts_with("linux_raid_member"))
            && old_plan
                .warnings
                .iter()
                .any(|w| w.contains("other system(s) found on it")),
        "the old installation's plan: {:?} {:?}",
        old_plan.erases,
        old_plan.warnings
    );
    let still_inactive = vm
        .ssh("test \"$(dmsetup ls 2>/dev/null | grep -c '^vgold-')\" = 0 && ! grep -q '^md' /proc/mdstat")?
        .success();
    ensure!(
        still_inactive,
        "planning activated the old installation's volumes"
    );

    // Refusals: each names its code and the field at fault.
    let refused: [(&str, Edit, &str, Option<&str>); 24] = [
        (
            "unknown tag",
            Box::new(|r| r["composition"] = json!("generic.kde")),
            "unknown-composition",
            Some("composition"),
        ),
        (
            "vm",
            Box::new(|r| r["composition"] = json!("vm.gnome")),
            "unknown-composition",
            Some("composition"),
        ),
        (
            "another edition",
            Box::new(|r| r["composition"] = json!("ms-7c35.ilex")),
            "unknown-composition",
            Some("composition"),
        ),
        (
            "no such disk",
            Box::new(|r| r["disk"] = json!("virtio-nonesuch")),
            "unknown-disk",
            Some("disk"),
        ),
        (
            "a kernel name",
            Box::new(|r| r["disk"] = json!("vda")),
            "unknown-disk",
            Some("disk"),
        ),
        (
            "too small",
            Box::new(|r| r["disk"] = json!("virtio-small")),
            "disk-too-small",
            Some("disk"),
        ),
        // The medium the system started from, however it is reached.
        (
            "live medium mounted by label",
            Box::new(|r| r["disk"] = json!("virtio-live")),
            "live-medium",
            Some("disk"),
        ),
        (
            "live medium behind a loop device (iso-scan)",
            Box::new(|r| r["disk"] = json!("virtio-isoscan")),
            "live-medium",
            Some("disk"),
        ),
        (
            "live medium behind device-mapper (Ventoy)",
            Box::new(|r| r["disk"] = json!("virtio-ventoy")),
            "live-medium",
            Some("disk"),
        ),
        (
            "a disk in use",
            Box::new(|r| r["disk"] = json!("virtio-busy")),
            "in-use",
            Some("disk"),
        ),
        (
            "free space",
            Box::new(|r| r["mode"] = json!("free-space")),
            "unsupported-mode",
            Some("mode"),
        ),
        (
            "tpm without encryption",
            Box::new(|r| r["tpm"] = json!("auto")),
            "tpm-needs-encryption",
            Some("tpm"),
        ),
        (
            "reserved name",
            Box::new(|r| r["administrator"] = json!("root")),
            "reserved-name",
            Some("administrator"),
        ),
        (
            "daily profile name",
            Box::new(|r| r["administrator"] = json!("andriishafar")),
            "reserved-name",
            Some("administrator"),
        ),
        // Names the live image defines (a user, a group) and the installed
        // system's marker groups; names the length of a Unix name allows.
        // (`rpcuser` is nfs-utils' sysusers.d account: not on the helper's own
        // list, so only the live image's own files can refuse it.)
        (
            "a user of the live image",
            Box::new(|r| r["administrator"] = json!("rpcuser")),
            "reserved-name",
            Some("administrator"),
        ),
        (
            "a group of the live image",
            Box::new(|r| r["administrator"] = json!("empower")),
            "reserved-name",
            Some("administrator"),
        ),
        (
            "the marker group of daily homes",
            Box::new(|r| r["administrator"] = json!("ilex-homed")),
            "reserved-name",
            Some("administrator"),
        ),
        (
            "a user name of 32 bytes",
            Box::new(|r| r["administrator"] = json!("a".repeat(32))),
            "invalid-field",
            Some("administrator"),
        ),
        (
            "host name",
            Box::new(|r| r["host_name"] = json!("-bad")),
            "invalid-field",
            Some("host_name"),
        ),
        (
            "a language without a codeset",
            Box::new(|r| r["language"] = json!("en_US")),
            "invalid-field",
            Some("language"),
        ),
        (
            "a zoneinfo file that is no zone",
            Box::new(|r| r["time_zone"] = json!("leapseconds")),
            "invalid-field",
            Some("time_zone"),
        ),
        (
            "a zone of the posix tree",
            Box::new(|r| r["time_zone"] = json!("posix/Europe/Berlin")),
            "invalid-field",
            Some("time_zone"),
        ),
        (
            "time zone",
            Box::new(|r| r["time_zone"] = json!("Mars/Olympus_Mons")),
            "invalid-field",
            Some("time_zone"),
        ),
        (
            "keymap",
            Box::new(|r| r["keyboard"] = json!("nonesuch")),
            "invalid-field",
            Some("keyboard"),
        ),
    ];
    let mut names: Vec<String> = Vec::new();
    let mut expected: Vec<(&str, Option<&str>)> = Vec::new();
    let mut scripts: Vec<String> = Vec::new();
    for (name, edit, code, field) in refused {
        names.push(name.to_string());
        expected.push((code, field));
        scripts.push(busctl("ilex-installer", "Plan", Some(&plan_request(edit))));
    }
    for (name, text) in [
        ("not JSON", "nonsense".to_string()),
        (
            "an unknown field",
            plan_request(|r| r["password"] = json!("hunter2")),
        ),
        // A mistyped or unknown value is never quoted back.
        (
            "an unknown mode",
            plan_request(|r| r["mode"] = json!("hunter2")),
        ),
        (
            "a mistyped field",
            plan_request(|r| r["encrypt"] = json!("hunter2")),
        ),
        (
            "a missing field",
            json!({"composition": "generic.gnome"}).to_string(),
        ),
    ] {
        names.push(name.to_string());
        expected.push(("invalid-request", None));
        scripts.push(busctl("ilex-installer", "Plan", Some(&text)));
    }
    scripts.push(busctl("nobody", "Plan", Some(&plan_request(|_| {}))));
    let ran = batch(vm, &scripts)?;
    for ((name, (code, field)), ran) in names.iter().zip(&expected).zip(&ran) {
        let document = refusal_document(ran)?;
        ensure!(
            document["code"] == *code
                && !document["message"].as_str().unwrap_or("").is_empty()
                && (field.is_none() || document["field"].as_str() == *field),
            "{name}: {document}"
        );
        ensure!(
            !document.to_string().contains("hunter2"),
            "{name}: the refusal repeats a field value"
        );
    }
    let unknown = refusal_document(&ran[0])?;
    ensure!(
        unknown["message"]
            .as_str()
            .is_some_and(|m| m.contains("offered: generic.gnome")),
        "the refusal does not list the offers: {unknown}"
    );
    ensure!(
        denied(ran.last().context("the last refusal")?),
        "nobody planning: {}",
        ran.last().map_or("", |r| &r.text)
    );

    // Planning wrote nothing: every disk is byte-for-byte what it was.
    let after = disk_fingerprints(vm)?;
    ensure!(
        before == after,
        "planning changed a disk:\n{before:?}\n{after:?}"
    );
    ensure!(
        no_scratch_left(vm)?,
        "a probe, plan or self-check left its scratch directory behind"
    );
    Ok(())
}

// --- the interface ----------------------------------------------------------------

/// The interface's checks, in the guest. `render` starts GNOME Kiosk in its
/// headless mode with one virtual monitor (its scale from a `monitors.xml`:
/// the virtual monitor's own identity is `MetaVendor` / `MetaVirtualMonitor`)
/// and draws every page and state to `/run/ilex-ui-shots`.
const UI_SCRIPT: &str = r#"set -u
export HOME=/run/ilex-ui-home
rm -rf /run/ilex-ui-shots; mkdir -p /run/ilex-ui-shots
echo "== self-check"
ilex-installer --self-check; echo "self-check status $?"
echo "== simulate on a real boot"
env -u ILEX_INSTALLER_SIMULATE_ALLOW ilex-installer --simulate generic 2>&1 | head -2; echo "simulate status ${PIPESTATUS[0]}"
render() {
    scheme=$1; scale=$2
    export XDG_RUNTIME_DIR=/run/ilex-ui
    rm -rf "$XDG_RUNTIME_DIR" "$HOME"; mkdir -m700 "$XDG_RUNTIME_DIR"; mkdir -p "$HOME/.config"
    cat > "$HOME/.config/monitors.xml" <<XML
<monitors version="2"><configuration><layoutmode>logical</layoutmode><logicalmonitor><x>0</x><y>0</y><scale>$scale</scale><primary>yes</primary><monitor><monitorspec><connector>Meta-0</connector><vendor>MetaVendor</vendor><product>MetaVirtualMonitor</product><serial>0x00</serial></monitorspec><mode><width>1920</width><height>1200</height><rate>60.000</rate></mode></monitor></logicalmonitor></configuration></monitors>
XML
    flag=; [ "$scheme" = dark ] && flag=--dark
    cat > /run/ilex-ui-inner.sh <<'INNER'
gnome-kiosk --headless --virtual-monitor 1920x1200 --wayland --no-x11 > /run/ilex-ui-kiosk.log 2>&1 &
kiosk=$!
for _ in $(seq 150); do [ -S "$XDG_RUNTIME_DIR/wayland-0" ] && break; sleep 0.2; done
sleep 2
WAYLAND_DISPLAY=wayland-0 GDK_BACKEND=wayland timeout 900 ilex-installer --self-check --render /run/ilex-ui-shots $RENDER_FLAG 2>&1 | grep -E '^ILEX-INSTALLER-RENDER|^ilex-installer:|rror|panicked|CRITICAL'
echo "render status ${PIPESTATUS[0]}"
kill "$kiosk" 2>/dev/null
INNER
    echo "== render $scheme $scale"
    RENDER_FLAG=$flag dbus-run-session -- bash /run/ilex-ui-inner.sh
}
render light 1
render dark 1
render light 1.5
echo "== files"
for f in /run/ilex-ui-shots/*.png; do echo "FILE $(basename "$f")"; base64 -w0 "$f"; echo; done
echo "== done"
"#;

/// Decode standard base64 (what `base64 -w0` prints).
fn unbase64(text: &str) -> Result<Vec<u8>> {
    let value = |c: u8| -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("not base64: {:?}", c as char),
        } as u32)
    };
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut bits = 0u32;
    let mut count = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        bits = (bits << 6) | value(c)?;
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Ok(out)
}

/// A PNG's width and height from its header.
fn png_size(bytes: &[u8]) -> Option<(u32, u32)> {
    (bytes.len() > 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]).then(
        || {
            let word = |at: usize| {
                u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
            };
            (word(16), word(20))
        },
    )
}

fn ui(vm: &EphemeralVm) -> Result<()> {
    let ws = common::workspace()?;
    let out = vm.ssh_timeout(
        &format!(
            "printf %s {} | base64 -d | bash -s",
            base64(UI_SCRIPT.as_bytes())
        ),
        std::time::Duration::from_secs(1800),
    )?;
    let text = printed(&out);
    let (log, files) = text
        .split_once("== files\n")
        .context("the script did not reach its files")?;
    let artifacts = ilex_devkit::qemu::artifact_dir(&ws, "installer-ui")?;
    fs::write(artifacts.join("ui-script.log"), log)?;
    println!("     {}", log.trim_end().replace('\n', "\n     "));

    ensure!(
        log.contains("self-check status 0") && log.contains("ILEX-INSTALLER-SELFCHECK ok"),
        "`ilex-installer --self-check` failed on the live image:\n{log}"
    );
    ensure!(
        log.contains("--simulate is refused") && log.contains("simulate status 1"),
        "--simulate was not refused on a boot without ilex.installer.simulate:\n{log}"
    );
    for (scheme, scale) in [("light", "1"), ("dark", "1"), ("light", "1.5")] {
        let want = "ILEX-INSTALLER-RENDER ok shots=";
        let line = log
            .lines()
            .find(|l| l.starts_with(want) && l.contains(&format!("scheme={scheme} scale={scale} ")))
            .with_context(|| format!("no render report for {scheme} at {scale}x:\n{log}"))?;
        ensure!(line.contains("shots=56 "), "{line}");
    }
    ensure!(
        !log.contains("render status 1") && log.matches("render status 0").count() == 3,
        "a render failed:\n{log}"
    );

    // The files: every page and state in each of the three configurations.
    let mut seen = std::collections::BTreeSet::new();
    let mut distinct = std::collections::BTreeSet::new();
    let mut name = None;
    for line in files.lines() {
        if let Some(file) = line.strip_prefix("FILE ") {
            name = Some(file.trim().to_owned());
        } else if let Some(file) = name.take() {
            let bytes = unbase64(line.trim()).with_context(|| format!("decoding {file}"))?;
            let (width, height) =
                png_size(&bytes).with_context(|| format!("{file} is not a PNG"))?;
            let expected = if file.ends_with("-1.5x.png") {
                (1920, 1200)
            } else {
                (1280, 800)
            };
            ensure!(
                (width, height) == expected && bytes.len() > 8 * 1024,
                "{file} is {width}x{height}, {} bytes: expected {expected:?} with something drawn",
                bytes.len()
            );
            distinct.insert(ilex_devkit::core::digest(&bytes));
            fs::write(artifacts.join(&file), &bytes)?;
            seen.insert(file);
        }
    }
    ensure!(
        seen.len() == 168,
        "{} images came back, expected 168 (56 states in three configurations)",
        seen.len()
    );
    ensure!(
        distinct.len() >= 140,
        "only {} of {} images differ: pages are not drawing their own states",
        distinct.len(),
        seen.len()
    );
    println!(
        "     {} images ({} distinct) in {}",
        seen.len(),
        distinct.len(),
        artifacts.display()
    );
    Ok(())
}
