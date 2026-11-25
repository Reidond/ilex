//! `installer::` tier: the new installer's root helper, without an ISO.
//!
//! The next installer's live image (`ILEX_INSTALLER_IMAGE`, built by
//! `ilexctl iso --next`'s image stage) boots as an ephemeral VM with an SSH
//! layer added for the harness, a blank 40 GiB disk, a disk carrying a fake
//! other-OS layout and an 8 GiB disk. The tests call `org.ilex.Installer1`
//! with `busctl` as the unprivileged `ilex-installer` user, exactly as the
//! interface will, and assert the JSON; another user must be refused by the
//! bus policy. Nothing here installs: no test writes to the helper's disks.
//!
//! * `installer::probe`: the helper's self-check; what it reports about this
//!   VM and its three disks; detection and the boot override through the real
//!   bus path (the SMBIOS identity and kernel command line are fixtures the
//!   unit is pointed at: a VM cannot carry an MSI board); the bus policy.
//! * `installer::plan`: whole-disk plans (plain and encrypted) against
//!   repart's own dry run, the registry's manifest, every refusal, and the
//!   proof that planning wrote nothing.
//!
//! The ISO's own boot (the live medium, Secure Boot, the session) is `iso::next…`.

use std::fs;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use ilex_devkit::bcvk::{self, EphemeralOpts, EphemeralVm, Output};
use ilex_devkit::installer_model::{plan::Plan, probe::Probe};
use ilex_devkit::qemu::base64;
use ilex_devkit::{Workspace, cmd};
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
const DISKS: [(&str, u64); 3] = [
    ("blank", 40 * GIB),
    ("layout", 48 * GIB),
    ("small", 8 * GIB),
];
const BUS: &str = "org.ilex.Installer1";
const MEMORY: &str = "4G";

static GROUP: Group<EphemeralVm> = Group::new("installer", boot);

pub fn expect(tests: usize) {
    GROUP.expect(tests);
}

pub fn shutdown() {
    GROUP.finish();
}

pub fn trials() -> Vec<Trial> {
    vec![
        trial("installer::probe", || GROUP.run(probe)),
        trial("installer::plan", || GROUP.run(plan)),
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
/// one in a layer of its own, tagged by the image it extends, and keeps the
/// kiosk session from starting in a VM without a display.
fn with_ssh(ws: &Workspace, live: &str) -> Result<String> {
    let id =
        cmd::output(Command::new("podman").args(["image", "inspect", "--format", "{{.Id}}", live]))
            .with_context(|| format!("the installer image {live} is not in local storage"))?;
    let tag = format!(
        "localhost/ilex-installer-next-test:{}",
        &id[..12.min(id.len())]
    );
    if bcvk::image_exists(&tag) {
        return Ok(tag);
    }
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
            .args(["--tag", &tag])
            .arg(&dir),
    )?;
    Ok(tag)
}

fn boot() -> Result<EphemeralVm> {
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
    wait_for_helper(&vm)?;
    Ok(vm)
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

/// A script that points the helper at SMBIOS and kernel command line
/// fixtures (a unit drop-in), waits for it and probes it as `ilex-installer`.
fn fixture_probe(dmi: &[(&str, &str)], cmdline: &str) -> String {
    let mut script = String::from(
        "rm -rf /run/ilex-test; mkdir -p /run/ilex-test/dmi /run/systemd/system/ilex-installerd.service.d\n",
    );
    for (field, value) in dmi {
        script.push_str(&format!(
            "printf '%s\\n' {} > /run/ilex-test/dmi/{field}\n",
            shell_quote(value)
        ));
    }
    script.push_str(&format!(
        "printf '%s\\n' {} > /run/ilex-test/cmdline\n",
        shell_quote(cmdline)
    ));
    script.push_str(
        "printf '[Service]\\nExecStart=\\nExecStart=/usr/libexec/ilex-installer/ilex-installerd --dmi-dir /run/ilex-test/dmi --cmdline /run/ilex-test/cmdline\\n' > /run/systemd/system/ilex-installerd.service.d/fixture.conf\nsystemctl daemon-reload\nsystemctl restart ilex-installerd\n",
    );
    script.push_str(WAIT_FOR_HELPER);
    script.push('\n');
    script.push_str(&busctl("ilex-installer", "Probe", None));
    script
}

/// The script that puts the helper back as the image ships it.
const RESTORE_HELPER: &str = "rm -rf /run/ilex-test /run/systemd/system/ilex-installerd.service.d; systemctl daemon-reload; systemctl restart ilex-installerd";

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

// --- installer::probe -------------------------------------------------------

fn probe(vm: &EphemeralVm) -> Result<()> {
    let check = vm
        .ssh("/usr/libexec/ilex-installer/ilex-installerd --self-check")?
        .ok("ilex-installerd --self-check")?;
    ensure!(
        check.stdout.contains("ILEX-INSTALLERD-CHECK OK"),
        "self-check: {}",
        check.stdout
    );
    println!("     {}", check.out());

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
                .all(|c| c.shells.first().map(String::as_str) == Some("gnome")),
        "catalogue offers {offered:?}"
    );

    // The disks: three, by their stable ids, none the live medium.
    let ids: Vec<&str> = probe.disks.iter().map(|d| d.id.as_str()).collect();
    ensure!(
        ids == ["virtio-blank", "virtio-layout", "virtio-small"],
        "disk ids {ids:?}"
    );
    for (disk, (name, size)) in probe.disks.iter().zip(DISKS) {
        ensure!(
            disk.size_bytes == size
                && disk.bus == "virtio"
                && !disk.removable
                && !disk.live_medium
                && disk.sector_size == 512,
            "{name}: {disk:?}"
        );
    }
    let blank = &probe.disks[0];
    ensure!(
        blank.partition_table.is_none()
            && blank.partitions.is_empty()
            && blank.other_systems.is_empty()
            && blank.free_extents.len() == 1
            && blank.free_extents[0].start_bytes == MIB
            && blank.free_extents[0].size_bytes == 40 * GIB - 2 * MIB,
        "blank: {blank:?}"
    );
    let layout = &probe.disks[1];
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
        probe.disks[2].partitions.is_empty(),
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
        .map(|(_, dmi, cmdline)| fixture_probe(dmi, cmdline))
        .collect();
    scripts.push(RESTORE_HELPER.into());
    let ran = batch(vm, &scripts)?;
    ensure!(
        ran.last().is_some_and(|r| r.code == 0),
        "restoring the helper: {}",
        ran.last().map_or("", |r| &r.text)
    );
    wait_for_helper(vm)?;
    let probes: Vec<Probe> = ran
        .iter()
        .zip(&cases)
        .map(|(ran, (name, _, _))| {
            serde_json::from_value(document(ran, name)?).with_context(|| (*name).to_string())
        })
        .collect::<Result<_>>()?;
    let [full, board_only, system_only, lower, half, boot, unknown, _] = &probes[..] else {
        bail!("{} fixture probes", probes.len());
    };
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

fn plan(vm: &EphemeralVm) -> Result<()> {
    let before = disk_fingerprints(vm)?;
    let probe = probe_document(vm)?;
    let blank_path = probe.disks[0].path.clone();

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
    let refused: [(&str, Edit, &str, Option<&str>); 13] = [
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
        (
            "host name",
            Box::new(|r| r["host_name"] = json!("-bad")),
            "invalid-field",
            Some("host_name"),
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
        vm.ssh("test -z \"$(ls -A /run/ilex-installer/plans 2>/dev/null)\"")?
            .success(),
        "a plan left its scratch files behind"
    );
    Ok(())
}
