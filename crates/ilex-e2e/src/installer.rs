//! `installer::` tier: the new installer's root helper, without an ISO.
//!
//! The next installer's live image (`ILEX_INSTALLER_IMAGE`, built by
//! `ilexctl iso --next`'s image stage) boots as an ephemeral VM with an SSH
//! layer added for the harness and seven disks: a blank 40 GiB one, one
//! carrying a fake other-OS layout, an 8 GiB one, and four the running system
//! is using (an ISO mounted by its label, an ISO file attached to a loop device
//! from a mounted partition as dracut's iso-scan does, Ventoy's device-mapper
//! device, and a disk with a mounted partition, swap and a stacked device). The
//! tests call `org.ilex.Installer1` with `busctl` as the unprivileged
//! `ilex-installer` user, exactly as the interface will, and assert the JSON;
//! another user must be refused by the bus policy. The tests share one VM and
//! one helper, which some of them restart: the group runs them one at a time.
//! Nothing here installs: no test writes to the helper's disks.
//!
//! * `installer::probe`: the helper's self-check; what it reports about this
//!   VM and its disks, the live medium and the disks in use among them;
//!   detection and the boot override through the real bus path (the SMBIOS
//!   identity and kernel command line are fixtures the unit is pointed at: a
//!   VM cannot carry an MSI board); the bus policy and bus activation.
//! * `installer::plan`: whole-disk plans (plain and encrypted) against
//!   repart's own dry run, the registry's manifest, every refusal a request
//!   can cause, and the proof that planning wrote nothing.
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

const GIB: u64 = 1 << 30;
const MIB: u64 = 1 << 20;
/// A change to a JSON document, for a variant of a fixture or a request.
type Edit = Box<dyn Fn(&mut Value)>;
/// A detection case: its name, SMBIOS fields and kernel command line.
type Case<'a> = (&'a str, Vec<(&'a str, &'a str)>, &'a str);

/// Disks attached to the VM: stable ids `virtio-<name>` (bcvk names the
/// virtio serial), and their sizes.
const DISKS: [(&str, u64); 7] = [
    ("blank", 40 * GIB),
    ("layout", 48 * GIB),
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

pub fn expect(tests: usize) {
    GROUP.expect(tests);
}

pub fn shutdown() {
    GROUP.finish();
}

pub fn trials() -> Vec<Trial> {
    vec![
        trial("installer::probe", || {
            GROUP.run(|vm| probe(&vm.lock().unwrap_or_else(PoisonError::into_inner)))
        }),
        trial("installer::plan", || {
            GROUP.run(|vm| plan(&vm.lock().unwrap_or_else(PoisonError::into_inner)))
        }),
    ]
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

/// The live image has no SSH server (a released ISO must not). The harness adds
/// one in a layer of its own and keeps the kiosk session from starting in a VM
/// without a display. The layer is this checkout's: its tag carries the
/// checkout's slug, the image it extends is part of the tag, and the image is
/// labelled with the checkout's owner like every other image the tooling builds.
fn with_ssh(ws: &Workspace, live: &str) -> Result<String> {
    let id =
        cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Id}}", live]))
            .with_context(|| format!("the installer image {live} is not in local storage"))?;
    let tag = format!(
        "localhost/ilex-installer-next-test:{}.{}",
        ws.slug,
        &id[..12.min(id.len())]
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
    fs::write(
        &containerfile,
        format!(
            "FROM {live}\n\
             RUN dnf -y install --setopt=install_weak_deps=0 openssh-server && \\\n\
                 systemctl enable sshd.service && \\\n\
                 systemctl mask ilex-installer-session.service ilex-installer-fallback.service\n\
             LABEL ilex.installer-test=\"ssh\"\n"
        ),
    )?;
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

/// Four disks the running system is using, set up the way the situations they
/// stand for look to the kernel:
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
            && mounts.contains("/run/initramfs/isoscan"),
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
        probe.edition.id == "gnome" && probe.edition.name == "ilex GNOME",
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
    // Nothing uses the three the tests plan on.
    for name in ["blank", "layout", "small"] {
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
            && full.edition.name == "ilex GNOME",
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

    lost_medium(vm)?;
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

/// A loop device whose file's mount is gone: the medium cannot be located, and
/// then no disk can be shown not to hold it. (The kernel reports a lazily
/// unmounted file as its name below the detached mount, `/lost.iso`, which the
/// root file system, `/`, contains by name: a mount table's longest prefix would
/// have called that "on no disk" and left every disk unmarked.)
fn lost_medium(vm: &EphemeralVm) -> Result<()> {
    let result = lost_medium_body(vm);
    let restored = guest_script(
        vm,
        r#"loop=$(cat /run/ilex-test/lost.loop 2>/dev/null || true)
umount /run/ilex-test/lost 2>/dev/null || true
[ -z "$loop" ] || losetup -d "$loop" || true
rm -f /run/ilex-test/lost.loop
rmdir /run/ilex-test/lost 2>/dev/null || true
for i in $(seq 1 30); do mountpoint -q /run/ilex-test/busy && break; mount /dev/disk/by-id/virtio-busy-part1 /run/ilex-test/busy 2>/dev/null || sleep 1; done
rm -f /run/ilex-test/busy/lost.iso
mountpoint -q /run/ilex-test/busy && echo restored
"#,
        "restoring the busy disk's mount",
    );
    result?;
    ensure!(
        restored?.trim().ends_with("restored"),
        "the busy disk's mount could not be put back"
    );
    let probe = probe_document(vm)?;
    let busy = probe
        .disks
        .iter()
        .find(|d| d.id == "virtio-busy")
        .context("no busy disk")?;
    let blank = probe
        .disks
        .iter()
        .find(|d| d.id == "virtio-blank")
        .context("no blank disk")?;
    ensure!(
        busy.in_use.len() == 3 && blank.in_use.is_empty(),
        "the disks are not as before the lost medium: {:?} / {:?}",
        busy.in_use,
        blank.in_use
    );
    Ok(())
}

fn lost_medium_body(vm: &EphemeralVm) -> Result<()> {
    let made = guest_script(
        vm,
        r#"set -eu
mkdir -p /run/ilex-test/lost
xorrisofs -quiet -V ILEXLOST -o /run/ilex-test/busy/lost.iso /run/ilex-test/iso
loop=$(losetup --find --show --read-only /run/ilex-test/busy/lost.iso)
echo "$loop" > /run/ilex-test/lost.loop
mount -t iso9660 -o ro "$loop" /run/ilex-test/lost
umount -l /run/ilex-test/busy
test ! -e /lost.iso
echo "backing [$(cat /sys/block/$(basename "$loop")/loop/backing_file)]"
"#,
        "attaching an ISO file and detaching its mount",
    )?;
    ensure!(
        made.contains("backing [/lost.iso]"),
        "the kernel names the detached file differently: {made}"
    );
    let probe = probe_document(vm)?;
    let reason = "cannot tell whether this disk holds the medium it started from";
    for disk in &probe.disks {
        ensure!(
            disk.in_use.iter().any(|r| r.contains(reason)),
            "{} is not marked though the live medium cannot be located: {:?}",
            disk.id,
            disk.in_use
        );
    }
    let refusal = plan_refusal_on_uefi(vm, &plan_request(|_| {}))?;
    ensure!(
        refusal["code"] == "in-use" && refusal["field"] == "disk",
        "a plan on the blank disk was not refused: {refusal}"
    );
    println!(
        "     a loop device whose file's mount is gone: every disk marked ({})",
        probe.disks[0].in_use.first().map_or("", String::as_str)
    );
    Ok(())
}

// --- installer::plan --------------------------------------------------------

/// Every disk's partition table, first 4 MiB and signature probe, to prove a
/// plan wrote nothing.
fn disk_fingerprints(vm: &EphemeralVm) -> Result<String> {
    let out = vm
        .ssh(
            "for n in blank layout small; do d=/dev/disk/by-id/virtio-$n; echo \"$n $(sfdisk --dump \"$d\" 2>&1 | sed 's/^device:.*//' | sha256sum) $(head -c 4194304 \"$d\" | sha256sum) $(blkid -p \"$d\" >/dev/null 2>&1; echo blkid=$?)\"; done",
        )?
        .ok("fingerprinting the disks")?;
    Ok(printed(&out))
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
        (
            "a user of the live image",
            Box::new(|r| r["administrator"] = json!("passim")),
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
