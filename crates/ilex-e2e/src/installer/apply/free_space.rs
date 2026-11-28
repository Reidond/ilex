//! `installer::free_space`, `::free_space_encrypted`, `::free_space_refusals`,
//! `::free_space_failure` and `::free_space_answers`: the helper installing
//! beside another system, for real.
//!
//! The target disk is made on the host the way another system leaves one: a
//! GPT with an EFI system partition (FAT32, a Windows Boot Manager directory
//! and the fallback copy of it, `EFI/Boot/bootx64.efi`), the reserved
//! partition, a data partition (ext4, standing in for NTFS, with known bytes
//! in it), a gap of 2 GiB, a recovery partition and the rest free. The gap is
//! the point of one assertion: `systemd-repart` takes the smallest free area
//! that holds each partition, so `/boot` goes to the 2 GiB gap and the root to
//! the tail. A boot entry "Windows Boot Manager" and another Fedora's entry on
//! a disk that is not there any more (`bootupd` replaces an entry that has its
//! label) are added to the firmware's variables before the installation, and
//! the VMs of the trial share them.
//!
//! * `installer::free_space`: the plan (two partitions, where they are, what is
//!   shared and kept, nothing erased), a wrong confirmation refused, the
//!   installation, and then the firmware's variables byte for byte (every one
//!   that was there is what it was, ilex has one more and is first in the
//!   order), and, from the host: every partition of the other system is byte
//!   for byte what it was (SHA-256 of each), every file of the shared EFI
//!   system partition is as it was (the fallback loader `bootc` replaced is
//!   back) beside ilex's own, and so is the MBR's boot code; no SELinux denial.
//!   The disk is then booted with the same devices, the firmware boots ilex
//!   through its own entry, and every assertion of `installer::install` holds.
//! * `installer::free_space_encrypted`: the same with the root in a LUKS2
//!   volume, unlocked at the boot prompt.
//! * `installer::free_space_refusals`: a disk for each way beside cannot be
//!   done (too small by a MiB, an ESP that is full, an ESP that is not FAT, no
//!   free space, an `EFI/fedora` directory, no ESP, a root partition of another
//!   system, a BIOS boot partition, partitions of an earlier ilex installation),
//!   the exact fit that is accepted, and an ESP that fills up between the plan
//!   and `Apply`.
//! * `installer::free_space_failure`: an installation made to fail (the test
//!   credential), before `bootc` runs and right after it (when `bootupd` has
//!   written ilex's loader and replaced the firmware's entries and nothing is
//!   put back yet): exactly the partitions it made are gone, and the firmware's
//!   variables, the files of the shared partition, the other partitions and
//!   the MBR are byte for byte what they were.
//! * `installer::free_space_answers`: the same installation with an answers
//!   file (`mode = "free-space"`), nothing typed.

use super::*;
use ilex_devkit::installer_model::free_space as model;
use std::collections::BTreeMap;

const MIB: u64 = 1 << 20;
/// The target disk: sparse.
const DISK: u64 = 80 * GIB;
const ESP_TYPE: &str = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B";
const MSR_TYPE: &str = "E3C9E316-0B5C-4DB8-817D-F92DF00215AE";
const DATA_TYPE: &str = "EBD0A0A2-B9E5-4433-87C0-68B6B72699C7";
const RECOVERY_TYPE: &str = "DE94BBA4-06D1-4D40-A16A-BFD50179D6AC";
const BIOS_TYPE: &str = "21686148-6449-6E6F-744E-656564454649";
const BOOT_TYPE: &str = "BC13C2FF-59E6-4262-A352-B275FD6F7172";
const ROOT_TYPE: &str = "4F68BCE3-E8CD-4DB1-96E7-FBCAF984B709";
/// The host name the installed system is given.
const HOST: &str = "ilexbeside";

/// One way of installing beside: plain, or with the root encrypted.
pub(crate) struct Case {
    name: &'static str,
    encrypt: bool,
}

pub(crate) const PLAIN: Case = Case {
    name: "free-space",
    encrypt: false,
};
pub(crate) const ENCRYPTED: Case = Case {
    name: "free-space-encrypted",
    encrypt: true,
};

// --- the other system's disk ---------------------------------------------------

/// A partition of the other system, in bytes.
#[derive(Clone, Debug)]
struct Part {
    number: u32,
    start: u64,
    len: u64,
}

/// Where everything is on the target disk (the same for every trial).
struct Layout {
    esp: Part,
    msr: Part,
    data: Part,
    recovery: Part,
    /// The 2 GiB gap and the free tail.
    gap: (u64, u64),
    tail: (u64, u64),
}

fn layout() -> Layout {
    let part = |number, start: u64, len: u64| Part {
        number,
        start: start * MIB,
        len: len * MIB,
    };
    let esp = part(1, 1, 100);
    let msr = part(2, 101, 16);
    let data = part(3, 117, 6 * 1024);
    let gap = ((117 + 6 * 1024) * MIB, 2048 * MIB);
    let recovery = part(4, 117 + 6 * 1024 + 2048, 500);
    let tail_start = recovery.start + recovery.len;
    Layout {
        esp,
        msr,
        data,
        recovery,
        gap,
        tail: (tail_start, DISK - MIB - tail_start),
    }
}

impl Layout {
    fn kept(&self) -> Vec<&Part> {
        vec![&self.esp, &self.msr, &self.data, &self.recovery]
    }
}

/// Bytes that look like nothing in particular and are the same every time.
fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

fn write_at(path: &Path, offset: u64, data: &[u8]) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .env("MTOOLS_SKIP_CHECK", "1")
        .output()
        .with_context(|| {
            format!("starting {program} (it is needed to make the other system's disk)")
        })?;
    ensure!(
        out.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A GPT on `path` from (start, length in MiB, type, name) lines.
fn gpt(path: &Path, lines: &[(u64, u64, &str, &str)]) -> Result<()> {
    let mut script = String::from("label: gpt\nunit: sectors\nfirst-lba: 2048\n");
    for (start, len, kind, name) in lines {
        script.push_str(&format!(
            "start={}, size={}, type={kind}, name=\"{name}\"\n",
            start * MIB / 512,
            len * MIB / 512
        ));
    }
    let mut child = Command::new("sfdisk")
        .arg("--quiet")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("starting sfdisk")?;
    std::io::Write::write_all(
        child.stdin.as_mut().context("sfdisk's input")?,
        script.as_bytes(),
    )?;
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "sfdisk: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(())
}

/// FAT32 on a partition of the image, and the files of the other system's EFI
/// directory in it (and `extra`, which a refusal's disk adds).
fn fat_with_files(path: &Path, esp: &Part, dir: &Path, extra: &[(&str, u64)]) -> Result<()> {
    run(
        "mkfs.fat",
        &[
            "-F",
            "32",
            // As Windows formats its 100 MiB partition: one sector a cluster,
            // so that it has the 65,525 clusters FAT32 needs (firmware reads
            // fewer as FAT16, and mkfs.fat's own choice of 32 KiB clusters
            // leaves 3,196).
            "-s",
            "1",
            "-n",
            "SYSTEM",
            "--offset",
            &(esp.start / 512).to_string(),
            &path.to_string_lossy(),
            &(esp.len / 1024).to_string(),
        ],
    )?;
    let boot = bytes(11, 131_072);
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("EFI/Microsoft/Boot/bootmgfw.efi", boot.clone()),
        ("EFI/Microsoft/Boot/bootmgr.efi", bytes(12, 98_304)),
        ("EFI/Microsoft/Boot/BCD", bytes(13, 65_536)),
        ("EFI/Microsoft/Recovery/BCD", bytes(14, 32_768)),
        // Windows' setup leaves a copy of its boot manager as the fallback
        // loader: `bootupd` replaces a file of that name.
        ("EFI/Boot/bootx64.efi", boot),
    ];
    let _ = fs::remove_dir_all(dir);
    for (name, content) in &files {
        let file = dir.join(name);
        fs::create_dir_all(file.parent().context("no parent")?)?;
        fs::write(&file, content)?;
    }
    for (name, len) in extra {
        let file = dir.join(name);
        fs::create_dir_all(file.parent().context("no parent")?)?;
        fs::write(&file, bytes(99, *len as usize))?;
    }
    let image = format!("{}@@{}", path.display(), esp.start);
    run(
        "mcopy",
        &["-s", "-i", &image, &dir.join("EFI").to_string_lossy(), "::"],
    )?;
    Ok(())
}

/// ext4 on a partition of the image, with known bytes well inside it.
fn ext4_with_bytes(path: &Path, part: &Part, label: &str, seed: u64) -> Result<()> {
    run(
        "mkfs.ext4",
        &[
            "-F",
            "-q",
            "-L",
            label,
            "-E",
            &format!("offset={}", part.start),
            &path.to_string_lossy(),
            &format!("{}k", part.len / 1024),
        ],
    )?;
    write_at(
        path,
        part.start + part.len / 2,
        &bytes(seed, 4 * MIB as usize),
    )?;
    Ok(())
}

/// The Windows-like disk: the layout above, made on the host.
fn prepare(path: &Path) -> Result<Layout> {
    let l = layout();
    raw_disk(path, DISK)?;
    gpt(
        path,
        &[
            (1, 100, ESP_TYPE, "EFI system partition"),
            (101, 16, MSR_TYPE, "Microsoft reserved partition"),
            (117, 6 * 1024, DATA_TYPE, "Basic data partition"),
            (
                117 + 6 * 1024 + 2048,
                500,
                RECOVERY_TYPE,
                "Windows RE tools",
            ),
        ],
    )?;
    fat_with_files(path, &l.esp, &path.with_extension("esp"), &[])?;
    write_at(path, l.msr.start, &bytes(21, MIB as usize))?;
    ext4_with_bytes(path, &l.data, "Windows", 22)?;
    ext4_with_bytes(path, &l.recovery, "Recovery", 23)?;
    Ok(l)
}

/// SHA-256 of a region of the image (read on the host, with the VM stopped).
fn region_hash(path: &Path, start: u64, len: u64) -> Result<String> {
    let script = format!(
        "dd if={} bs=1M skip={start} count={len} iflag=skip_bytes,count_bytes status=none | sha256sum | cut -d' ' -f1",
        path.display()
    );
    Ok(run("sh", &["-c", &script])?.trim().to_owned())
}

/// What the other system's partitions hold, by hash.
fn hashes(path: &Path, l: &Layout) -> Result<Vec<(u32, String)>> {
    l.kept()
        .into_iter()
        // The ESP is read file by file: ilex's boot files are added to it.
        .filter(|p| p.number != 1)
        .map(|p| Ok((p.number, region_hash(path, p.start, p.len)?)))
        .collect()
}

/// Every file of the ESP's `EFI` directory, by path and digest, read on the host.
fn esp_files(path: &Path, l: &Layout, dir: &Path) -> Result<BTreeMap<String, String>> {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir)?;
    let image = format!("{}@@{}", path.display(), l.esp.start);
    run(
        "mcopy",
        &["-s", "-n", "-i", &image, "::EFI", &dir.to_string_lossy()],
    )?;
    let listing = run(
        "sh",
        &[
            "-c",
            &format!(
                "cd {} && find . -type f | sort | xargs sha256sum",
                dir.display()
            ),
        ],
    )?;
    Ok(listing
        .lines()
        .filter_map(|l| {
            let (hash, name) = l.split_once("  ")?;
            Some((name.trim_start_matches("./").to_owned(), hash.to_owned()))
        })
        .collect())
}

fn find_disk(probe: &Probe, bytes: u64) -> Result<Disk> {
    probe
        .disks
        .iter()
        .find(|d| d.size_bytes == bytes)
        .cloned()
        .with_context(|| {
            format!(
                "no disk of {bytes} bytes in the probe: {:?}",
                probe
                    .disks
                    .iter()
                    .map(|d| (&d.id, d.size_bytes))
                    .collect::<Vec<_>>()
            )
        })
}

fn free_request(disk: &str, encrypt: bool, edit: impl FnOnce(&mut Value)) -> String {
    request(disk, |r| {
        r["mode"] = json!("free-space");
        r["encrypt"] = json!(encrypt);
        r["host_name"] = json!(HOST);
        if encrypt {
            r["tpm"] = json!("off");
        }
        edit(r);
    })
}

/// The firmware's boot entries, as `efibootmgr -v` prints them in the guest.
fn boot_entries(vm: &Vm) -> Result<String> {
    ok(vm, "efibootmgr -v")
}

/// The number of the entry whose line has `needle`.
fn entry_number(entries: &str, needle: &str) -> Option<String> {
    entries.lines().find_map(|l| {
        let rest = l.strip_prefix("Boot")?;
        (l.contains(needle) && rest.get(..4)?.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| rest[..4].to_owned())
    })
}

/// The firmware's boot variables of the guest, byte for byte: name (`Boot0001`,
/// `BootOrder`) to the SHA-256 of the variable's file (attributes and data).
fn nvram(vm: &Vm) -> Result<BTreeMap<String, String>> {
    let listing = ok(
        vm,
        "cd /sys/firmware/efi/efivars && sha256sum Boot[0-9A-F][0-9A-F][0-9A-F][0-9A-F]-* BootOrder-* | sort -k2",
    )?;
    Ok(listing
        .lines()
        .filter_map(|l| {
            let (hash, name) = l.split_once("  ")?;
            Some((name.split('-').next()?.to_owned(), hash.to_owned()))
        })
        .collect())
}

/// What a firmware boot entry is made of: attributes, the file path list's
/// length, the label, a hard-disk node naming `guid`, the loader's path.
fn boot_entry_bytes(label: &str, guid: &str, loader: &str) -> Vec<u8> {
    let mut path = vec![4, 1, 42, 0];
    path.extend_from_slice(&1u32.to_le_bytes());
    path.extend_from_slice(&0x800u64.to_le_bytes());
    path.extend_from_slice(&0x32000u64.to_le_bytes());
    let digits: String = guid.chars().filter(|c| *c != '-').collect();
    let mut raw: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&digits[i * 2..i * 2 + 2], 16).unwrap_or(0))
        .collect();
    raw[0..4].reverse();
    raw[4..6].reverse();
    raw[6..8].reverse();
    path.extend_from_slice(&raw);
    path.extend_from_slice(&[2, 2]);
    let text: Vec<u8> = loader
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    path.extend_from_slice(&[4, 4]);
    path.extend_from_slice(&((text.len() + 4) as u16).to_le_bytes());
    path.extend_from_slice(&text);
    path.extend_from_slice(&[0x7f, 0xff, 4, 0]);
    // The variable's attributes (non-volatile, boot service, runtime), then the
    // load option's: active.
    let mut entry = vec![7, 0, 0, 0, 1, 0, 0, 0];
    entry.extend_from_slice(&(path.len() as u16).to_le_bytes());
    entry.extend(label.encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    entry.extend_from_slice(&path);
    entry
}

/// Another Fedora's entry, on a disk that is not there any more: `bootupd`
/// replaces an entry with its own label, and a restoration that needs the disk
/// to exist cannot put this one back. Written raw, as a firmware would have it.
fn add_stale_fedora(vm: &Vm) -> Result<()> {
    let hex: String = boot_entry_bytes(
        "Fedora",
        "deadbeef-0000-4000-8000-0000000000aa",
        "\\EFI\\fedora\\shimx64.efi",
    )
    .iter()
    .map(|b| format!("{b:02x}"))
    .collect();
    ok(
        vm,
        &format!(
            "python3 - <<'EOF'\nwith open('/sys/firmware/efi/efivars/Boot0042-8be4df61-93ca-11d2-aa0d-00e098032b8c','wb') as f:\n    f.write(bytes.fromhex('{hex}'))\nEOF"
        ),
    )?;
    Ok(())
}

/// The files of a disk's EFI system partition as the guest sees them (mounted
/// read-only for a moment): path to SHA-256.
fn esp_in_guest(vm: &Vm, node: &str) -> Result<BTreeMap<String, String>> {
    let listing = ok(
        vm,
        &format!(
            "set -e; mkdir -p /run/espx; mount -t vfat -o ro {node} /run/espx; (cd /run/espx && find . -type f | sort | xargs sha256sum); umount /run/espx"
        ),
    )?;
    Ok(listing
        .lines()
        .filter_map(|l| {
            let (hash, name) = l.split_once("  ")?;
            Some((name.trim_start_matches("./").to_owned(), hash.to_owned()))
        })
        .collect())
}

// --- installer::free_space and ::free_space_encrypted ----------------------------

pub(super) fn beside(case: &'static Case) -> Result<()> {
    let mut bench = Bench::new(case.name)?;
    bench.efi_vars = Some(bench.dir.join("OVMF_VARS.fd"));
    let _ = fs::remove_file(bench.efi_vars.as_ref().context("vars")?);
    let l = prepare(&bench.target)?;
    let before = hashes(&bench.target, &l)?;
    let mbr_before = region_hash(&bench.target, 0, 440)?;
    let files_dir = bench.dir.join("esp-files");
    let esp_before = esp_files(&bench.target, &l, &files_dir)?;
    ensure!(
        esp_before.len() == 5,
        "the fake ESP's files: {esp_before:?}"
    );
    let vm = bench.boot_live()?;
    let probe = probe(&vm)?;
    let target = find_disk(&probe, DISK)?;
    let sentinel = find_disk(&probe, SENTINEL_BYTES)?;
    let sentinel_before = fingerprint(&vm, &[&sentinel])?;
    // The other system's own entry in the firmware's boot menu.
    ok(
        &vm,
        &format!(
            "efibootmgr --create --disk /dev/disk/by-id/{} --part 1 --label 'Windows Boot Manager' --loader '\\EFI\\Microsoft\\Boot\\bootmgfw.efi' >/dev/null",
            target.id
        ),
    )?;
    // Another Fedora's entry on a disk that is gone: `bootupd` replaces an
    // entry with its own label, and putting this one back must not need the disk.
    add_stale_fedora(&vm)?;
    let entries_before = boot_entries(&vm)?;
    let windows = entry_number(&entries_before, "Windows Boot Manager")
        .with_context(|| format!("no Windows entry was made: {entries_before}"))?;
    bench.save("firmware-before.txt", &entries_before);
    let nvram_before = nvram(&vm)?;
    ensure!(
        nvram_before.contains_key("Boot0042") && nvram_before.contains_key("BootOrder"),
        "the firmware's variables before: {nvram_before:?}"
    );

    // --- what the probe sees, and what the model says of it ----------------------
    ensure!(
        target.partition_table.as_deref() == Some("gpt")
            && target.partitions.len() == 4
            && target.esps.len() == 1
            && target.esps[0].readable
            && target.esps[0].loaders == ["Microsoft"]
            && !target.esps[0].fedora_directory
            && target.esps[0]
                .free_bytes
                .is_some_and(|f| f > 50 * MIB && f < 100 * MIB)
            && target.partitions.iter().all(|p| !p.uuid.is_empty())
            && target
                .other_systems
                .iter()
                .any(|s| s.description.contains("Windows Boot Manager"))
            && target.free_extents.len() == 2
            && target.free_extents[0].start_bytes == l.gap.0
            && target.free_extents[0].size_bytes == l.gap.1
            && target.free_extents[1].start_bytes == l.tail.0,
        "the probe of the other system's disk: {target:?}"
    );
    let offer = model::assess(&target)
        .map_err(|why| anyhow::anyhow!("the model refuses the disk: {why}"))?;
    ensure!(
        offer.placement.boot.start_bytes == l.gap.0 && offer.placement.root.start_bytes == l.tail.0,
        "the model's placement: {:?}",
        offer.placement
    );

    // --- the plan -------------------------------------------------------------
    let plan = plan(&vm, &free_request(&target.id, case.encrypt, |_| {}))?;
    bench.save("plan.json", &serde_json::to_string_pretty(&plan)?);
    let boot = &plan.partitions[0];
    let root = &plan.partitions[1];
    ensure!(
        plan.request.mode == ilex_devkit::installer_model::plan::Mode::FreeSpace
            && plan.partitions.len() == 2
            && boot.kind == "xbootldr"
            && boot.number == 5
            && boot.start_bytes == l.gap.0
            && boot.size_bytes == GIB
            && root.kind == "root-x86-64"
            && root.number == 6
            && root.start_bytes == l.tail.0
            && root.size_bytes.abs_diff(l.tail.1) < MIB
            && root.encrypted == case.encrypt
            && plan.erases.partitions.is_empty()
            && plan.erases.systems.is_empty()
            && plan.kept.len() == 4
            && plan.kept[0].path.ends_with('1')
            && plan.kept[2].filesystem == "ext4"
            && plan.esp.as_ref().is_some_and(|e| e.number == 1
                && e.loaders == ["Microsoft"]
                && e.free_bytes > 50 * MIB)
            && plan
                .warnings
                .iter()
                .any(|w| w.starts_with("back up what matters")),
        "the plan beside the other system: {plan:#?}"
    );
    println!(
        "     plan {}: /boot in the 2 GiB gap at {} MiB, root in the tail at {} MiB ({} MiB), nothing erased",
        plan.id,
        boot.start_bytes / MIB,
        root.start_bytes / MIB,
        root.size_bytes / MIB
    );
    // The confirmation is the disk's id: nothing is written without it.
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    if case.encrypt {
        let ran = set_secret(
            &vm,
            "ilex-installer",
            &plan.id,
            "disk-passphrase",
            encrypted::US.passphrase,
            true,
        )?;
        ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    }
    let image_before = fingerprint(&vm, &[&target, &sentinel])?;
    refused(
        &apply(&vm, "ilex-installer", &plan.id, "")?,
        ErrorCode::InvalidField,
        Some("erase_disk"),
    )?;
    nothing_happened(&vm, &image_before, &[&target, &sentinel], &plan.id)?;

    // --- the installation -------------------------------------------------------
    let started: State = serde_json::from_value(document(
        &apply(&vm, "ilex-installer", &plan.id, &target.id)?,
        "Apply",
    )?)?;
    ensure!(started.active, "Apply's state: {started:?}");
    let mut trace = Trace::new();
    let started_at = Instant::now();
    let finished = wait_for_end(&vm, &mut trace, INSTALL_TIMEOUT)?;
    println!(
        "     installed beside the other system in {:?}",
        started_at.elapsed()
    );
    bench.save("state.json", &serde_json::to_string_pretty(&finished)?);
    ensure!(
        finished.outcome == Some(Outcome::Success) && finished.stage == Stage::Done,
        "the installation ended: {:?}\n{}",
        finished.outcome,
        finished.log.join("\n")
    );
    // The stage log says what was done to the shared partition.
    let logs = ok(
        &vm,
        &format!(
            "cd /run/ilex-installer/log/{} && grep -h -e 'shared' -e 'replaced' -e 'as they were' -e 'firmware boot entry' -e 'free-space mode' *.log",
            plan.id
        ),
    )?;
    bench.save("stage-log-esp.txt", &logs);
    ensure!(
        logs.contains("is shared: it is mounted, never formatted")
            && logs.contains("the other system's file was put back byte for byte")
            && logs.contains("files that were on the shared EFI system partition are as they were")
            && logs.contains("the partitions on the disk are not wiped"),
        "the log of the installation beside another system: {logs}"
    );
    // The firmware: the other loader is still listed, and ilex has an entry of
    // its own, in front.
    let entries_after = boot_entries(&vm)?;
    bench.save("firmware-after.txt", &entries_after);
    // ilex's entry names the shared ESP's partition and shim; bootupd replaced
    // the entry that had its label (the live disk's own `Fedora`, which stands
    // here for another Fedora's), and the installation put it back.
    let esp_guid = plan
        .esp
        .as_ref()
        .context("the plan names no ESP")?
        .uuid
        .to_ascii_lowercase();
    let ilex_entry = entries_after
        .lines()
        .find_map(|l| {
            let lower = l.to_ascii_lowercase();
            let rest = l.strip_prefix("Boot")?;
            (lower.contains(&esp_guid) && lower.contains("\\efi\\fedora\\shimx64.efi"))
                .then(|| rest.get(..4).map(str::to_owned))
                .flatten()
        })
        .with_context(|| format!("no firmware entry for ilex: {entries_after}"))?;
    let order = entries_after
        .lines()
        .find_map(|l| l.strip_prefix("BootOrder: "))
        .with_context(|| format!("no BootOrder: {entries_after}"))?;
    ensure!(
        entry_number(&entries_after, "Windows Boot Manager").as_deref() == Some(windows.as_str())
            && order.split(',').any(|n| n == windows)
            && order.split(',').next() == Some(ilex_entry.as_str()),
        "the firmware's entries and order after: {entries_after}"
    );
    // Every entry that was there is still there, as it was.
    let texts = |entries: &str| -> std::collections::BTreeSet<String> {
        entries
            .lines()
            .filter(|l| {
                l.starts_with("Boot")
                    && l.get(4..8)
                        .is_some_and(|n| n.bytes().all(|b| b.is_ascii_hexdigit()))
            })
            .map(|l| l.get(9..).unwrap_or_default().to_owned())
            .collect()
    };
    let missing: Vec<String> = texts(&entries_before)
        .difference(&texts(&entries_after))
        .cloned()
        .collect();
    ensure!(
        missing.is_empty(),
        "firmware entries that were there are gone or changed: {missing:?}\n{entries_after}"
    );
    // The firmware's variables, byte for byte: every entry that was there
    // (the other Fedora's on a disk that is gone included) is exactly what it
    // was, ilex has one entry more, and only the order differs.
    let nvram_after = nvram(&vm)?;
    bench.save(
        "nvram-after.txt",
        &format!("{nvram_before:#?}\n{nvram_after:#?}"),
    );
    for (name, hash) in nvram_before.iter().filter(|(n, _)| *n != "BootOrder") {
        ensure!(
            nvram_after.get(name) == Some(hash),
            "the firmware variable {name} is not byte for byte what it was"
        );
    }
    let added: Vec<&String> = nvram_after
        .keys()
        .filter(|n| !nvram_before.contains_key(*n))
        .collect();
    ensure!(
        added.len() == 1 && added[0] == &format!("Boot{}", ilex_entry.to_ascii_uppercase()),
        "the variables the installation added: {added:?}"
    );
    // What is on the disk, from the live system: the other partitions and the
    // two new ones.
    let lsblk = ok(
        &vm,
        &format!("lsblk -n -o NAME,PARTLABEL,FSTYPE,SIZE -r {}", target.path),
    )?;
    bench.save("lsblk.txt", &lsblk);
    ensure!(
        lsblk.lines().count() >= 7 && lsblk.contains("ilex-boot") && lsblk.contains("ilex-root"),
        "the disk after the installation: {lsblk}"
    );
    ensure!(
        !lsblk.contains("ilex-esp"),
        "an ESP of ilex's own was made: {lsblk}"
    );
    ensure!(
        fingerprint(&vm, &[&sentinel])? == sentinel_before,
        "the sentinel disk changed"
    );
    ok(&vm, "sync")?;

    // --- what was just installed is not offered beside again --------------------
    // The disk page, `Plan` and the helper refuse a disk that holds partitions of
    // ilex's own: installing beside never removes a partition.
    let probe_done = self::probe(&vm)?;
    let done = find_disk(&probe_done, DISK)?;
    ensure!(
        done.partitions
            .iter()
            .filter(|p| p.label.starts_with("ilex-"))
            .count()
            == 2
            && model::assess(&done)
                .err()
                .is_some_and(|why| why.contains("remove them with a partition tool")
                    && why.contains("ilex-boot")),
        "the installation that was just made, in the model's rule: {done:#?}"
    );
    let error = refused(
        &ssh(
            &vm,
            &busctl(
                "ilex-installer",
                "Plan",
                &[&free_request(&target.id, case.encrypt, |_| {})],
            ),
        )?,
        ErrorCode::FreeSpaceUnavailable,
        Some("mode"),
    )?;
    ensure!(
        error.message.contains("remove them with a partition tool"),
        "Plan beside a disk that holds ilex's partitions: {}",
        error.message
    );
    println!("     beside ilex's own partitions: {}", error.message);
    selinux::check(&vm, &bench.artifacts, &format!("installer-{}", case.name))?;
    drop(vm);

    // --- the other system, from the host ------------------------------------------
    ensure!(
        hashes(&bench.target, &l)? == before,
        "a partition of the other system changed"
    );
    let esp_after = esp_files(&bench.target, &l, &files_dir)?;
    for (name, hash) in &esp_before {
        ensure!(
            esp_after.get(name) == Some(hash),
            "{name} on the shared EFI system partition is not what it was (the fallback loader is the file `bootc` replaces)"
        );
    }
    ensure!(
        esp_after
            .keys()
            .any(|n| n.to_ascii_lowercase().starts_with("efi/fedora/")),
        "ilex's boot files are not on the shared partition: {:?}",
        esp_after.keys().collect::<Vec<_>>()
    );
    ensure!(
        region_hash(&bench.target, 0, 440)? == mbr_before,
        "the first 440 bytes of the disk (the MBR's boot code) changed"
    );
    println!(
        "     the other system: {} partitions and {} EFI files byte for byte as they were; ilex added {} files",
        before.len(),
        esp_before.len(),
        esp_after.len() - esp_before.len()
    );

    // --- the disk, booted: the firmware starts ilex through its own entry --------
    let mut installed = Vm::boot(&bench.ws, &bench.live_config())?;
    if case.encrypt {
        encrypted::unlock(
            &mut installed,
            encrypted::US.keys,
            &bench.artifacts,
            &format!("installer-{}", case.name),
        )?;
    }
    if case.encrypt && installed.wait_ssh(Duration::from_secs(240)).is_err() {
        // The passphrase was typed before the prompt was there (a boot from a
        // cold disk is slow): once more.
        let ctl = Vmctl::from_info(installed.info.clone());
        ctl.type_text(encrypted::US.keys)?;
        ctl.key("Return")?;
    }
    installed
        .wait_ssh(Duration::from_secs(900))
        .context("the installed system did not come up beside the other system")?;
    let current = ok(&installed, "efibootmgr | sed -n 's/^BootCurrent: //p'")?;
    bench.save("boot-current.txt", &current);
    ensure!(
        current.trim() == ilex_entry,
        "the firmware booted entry {current}, not ilex's {ilex_entry}"
    );
    check_installed(
        &|name, text| bench.save(name, text),
        &installed,
        &Expected {
            digest: &plan.image.digest,
            target: &plan.image.target,
            host: HOST,
            zone: "Etc/UTC",
            locale: "en_US.UTF-8",
            keymap: "us",
            layout: "us",
            admin: ADMIN,
            password: PASSWORD,
            stamp: &[],
        },
    )?;
    if case.encrypt {
        encrypted::booted_encrypted(&installed, None, &encrypted::US, &|name, text| {
            bench.save(name, text)
        })?;
    }
    // The other system's loader is still in the firmware's list, from the
    // installed system too.
    ensure!(
        entry_number(&boot_entries(&installed)?, "Windows Boot Manager").is_some(),
        "the firmware lost the other system's entry"
    );
    Ok(())
}

// --- installer::free_space_refusals ---------------------------------------------

/// A disk for one refusal: its size (distinct, so the probe's disk can be found
/// by it), a name, and how it is made.
struct Refusal {
    name: &'static str,
    bytes: u64,
    /// What `Plan` must say (a phrase of the message), or `None` where beside
    /// must be accepted.
    says: Option<&'static str>,
    make: fn(&Path) -> Result<()>,
}

/// An ESP with the other system's files, a data partition, and `gap` MiB free
/// after them (the rest of the disk is the data partition when `gap` is 0).
fn small_windows(path: &Path, total: u64, gap: u64, esp_extra: &[(&str, u64)]) -> Result<()> {
    raw_disk(path, total)?;
    let data_len = total / MIB - 117 - gap - 1;
    gpt(
        path,
        &[
            (1, 100, ESP_TYPE, "EFI system partition"),
            (101, 16, MSR_TYPE, "Microsoft reserved partition"),
            (117, data_len, DATA_TYPE, "Basic data partition"),
        ],
    )?;
    let esp = Part {
        number: 1,
        start: MIB,
        len: 100 * MIB,
    };
    fat_with_files(path, &esp, &path.with_extension("esp"), esp_extra)?;
    Ok(())
}

/// A Windows-like disk of `total` bytes with ilex's two partitions in its free
/// space, made on the host: what an installation (finished or not) leaves.
fn windows_with_ilex(path: &Path, total: u64) -> Result<()> {
    small_windows(path, total, 33 * 1024, &[])?;
    let (boot_start, root_start) = (total - 3 * GIB, total - 2 * GIB);
    let script = format!(
        "start={}, size={}, type={BOOT_TYPE}, name=\"ilex-boot\"\nstart={}, size={}, type={ROOT_TYPE}, name=\"ilex-root\"\n",
        boot_start / 512,
        GIB / 512,
        root_start / 512,
        GIB / 512
    );
    let mut child = Command::new("sfdisk")
        .args(["--quiet", "--append"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    std::io::Write::write_all(child.stdin.as_mut().context("input")?, script.as_bytes())?;
    ensure!(child.wait()?.success(), "sfdisk --append");
    Ok(())
}

fn refusal_cases() -> Vec<Refusal> {
    const G: u64 = GIB;
    vec![
        Refusal {
            name: "a MiB short of enough",
            bytes: 34 * G + MIB,
            says: Some("in one piece"),
            make: |p| {
                // 31 GiB less a MiB free, after a data partition.
                small_windows(p, 34 * G + MIB, 31 * 1024 - 1, &[])
            },
        },
        Refusal {
            name: "exactly enough",
            bytes: 35 * G + MIB,
            says: None,
            make: |p| small_windows(p, 35 * G + MIB, 31 * 1024, &[]),
        },
        Refusal {
            name: "an ESP with no room",
            bytes: 36 * G + MIB,
            says: Some("free and ilex's boot files need"),
            make: |p| {
                small_windows(
                    p,
                    36 * G + MIB,
                    33 * 1024,
                    &[("EFI/Vendor/big.bin", 90 * MIB)],
                )
            },
        },
        Refusal {
            name: "an ESP that is not FAT",
            bytes: 37 * G + MIB,
            says: Some("not FAT"),
            make: |p| {
                raw_disk(p, 37 * G + MIB)?;
                gpt(
                    p,
                    &[
                        (1, 100, ESP_TYPE, "EFI system partition"),
                        (101, 4096, DATA_TYPE, "Basic data partition"),
                    ],
                )?;
                let esp = Part {
                    number: 1,
                    start: MIB,
                    len: 100 * MIB,
                };
                ext4_with_bytes(p, &esp, "ESP", 31)
            },
        },
        Refusal {
            name: "no unallocated space",
            bytes: 38 * G + MIB,
            says: Some("no unallocated space"),
            make: |p| small_windows(p, 38 * G + MIB, 0, &[]),
        },
        Refusal {
            name: "EFI/fedora already there",
            bytes: 39 * G + MIB,
            says: Some("already holds EFI/fedora"),
            make: |p| {
                small_windows(
                    p,
                    39 * G + MIB,
                    33 * 1024,
                    &[("EFI/fedora/grubx64.efi", 4096)],
                )
            },
        },
        Refusal {
            name: "no EFI system partition",
            bytes: 40 * G + MIB,
            says: Some("no EFI system partition"),
            make: |p| {
                raw_disk(p, 40 * G + MIB)?;
                gpt(p, &[(1, 4096, DATA_TYPE, "Basic data partition")])
            },
        },
        Refusal {
            name: "a root partition of another system",
            bytes: 41 * G + MIB,
            says: Some("type ilex uses"),
            make: |p| {
                raw_disk(p, 41 * G + MIB)?;
                gpt(
                    p,
                    &[
                        (1, 100, ESP_TYPE, "EFI system partition"),
                        (101, 4096, ROOT_TYPE, "arch"),
                    ],
                )?;
                let esp = Part {
                    number: 1,
                    start: MIB,
                    len: 100 * MIB,
                };
                fat_with_files(p, &esp, &p.with_extension("esp"), &[])
            },
        },
        Refusal {
            name: "an ilex installation already there",
            bytes: 42 * G + MIB,
            says: Some("remove them with a partition tool"),
            make: |p| {
                raw_disk(p, 42 * G + MIB)?;
                gpt(
                    p,
                    &[
                        (1, 512, ESP_TYPE, "ilex-esp"),
                        (513, 1024, BOOT_TYPE, "ilex-boot"),
                        (1537, 4096, ROOT_TYPE, "ilex-root"),
                    ],
                )?;
                let esp = Part {
                    number: 1,
                    start: MIB,
                    len: 512 * MIB,
                };
                fat_with_files(p, &esp, &p.with_extension("esp"), &[])
            },
        },
        Refusal {
            name: "ilex's two partitions (an installation, finished or not)",
            bytes: 43 * G + MIB,
            says: Some("remove them with a partition tool"),
            make: |p| windows_with_ilex(p, 43 * G + MIB),
        },
        Refusal {
            name: "a BIOS boot partition",
            bytes: 45 * G + MIB,
            says: Some("BIOS boot partition"),
            make: |p| {
                raw_disk(p, 45 * G + MIB)?;
                gpt(
                    p,
                    &[
                        (1, 100, ESP_TYPE, "EFI system partition"),
                        (101, 1, BIOS_TYPE, "BIOS boot partition"),
                        (102, 4096, DATA_TYPE, "Basic data partition"),
                    ],
                )?;
                let esp = Part {
                    number: 1,
                    start: MIB,
                    len: 100 * MIB,
                };
                fat_with_files(p, &esp, &p.with_extension("esp"), &[])
            },
        },
    ]
}

pub(super) fn refusals() -> Result<()> {
    let bench = Bench::new("free-space-refusals")?;
    let cases = refusal_cases();
    let mut disks = Vec::new();
    for case in &cases {
        let file: String = case
            .name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let path = bench.dir.join(format!("{file}.img"));
        (case.make)(&path).with_context(|| case.name.to_owned())?;
        disks.push(path);
    }
    // The ESP of the disk that fills up between the plan and `Apply`.
    let late = bench.dir.join("late.img");
    small_windows(&late, 44 * GIB + MIB, 33 * 1024, &[])?;
    let mut config = bench.live_config();
    config.extra_disks = disks
        .iter()
        .cloned()
        .chain([late.clone(), bench.sentinel.clone()])
        .collect();
    config.bare_disks = 0;
    let mut vm = Vm::boot(&bench.ws, &config)?;
    vm.wait_ssh(Duration::from_secs(420))?;
    ok(&vm, "mount -t tmpfs tmpfs /sysroot/ostree")?;
    let probe = probe(&vm)?;
    let sentinel = find_disk(&probe, SENTINEL_BYTES)?;
    let sentinel_before = fingerprint(&vm, &[&sentinel])?;

    for case in &cases {
        let disk = find_disk(&probe, case.bytes)?;
        let ran = ssh(
            &vm,
            &busctl(
                "ilex-installer",
                "Plan",
                &[&free_request(&disk.id, false, |_| {})],
            ),
        )?;
        match case.says {
            Some(says) => {
                let error = refused(&ran, ErrorCode::FreeSpaceUnavailable, Some("mode"))
                    .with_context(|| case.name.to_owned())?;
                ensure!(
                    error.message.contains(says),
                    "{}: the refusal says '{}', not '{says}'",
                    case.name,
                    error.message
                );
                println!("     {}: {}", case.name, error.message);
            }
            None => {
                let plan: Plan = serde_json::from_value(document(&ran, "Plan")?)
                    .with_context(|| case.name.to_owned())?;
                ensure!(
                    plan.partitions.len() == 2
                        && plan.partitions[1].size_bytes >= 30 * GIB
                        && plan.kept.len() == 3,
                    "{}: {plan:?}",
                    case.name
                );
                println!(
                    "     {}: planned (/boot {} MiB at {}, root {} MiB at {})",
                    case.name,
                    plan.partitions[0].size_bytes / MIB,
                    plan.partitions[0].start_bytes / MIB,
                    plan.partitions[1].size_bytes / MIB,
                    plan.partitions[1].start_bytes / MIB
                );
            }
        }
        // Whole-disk mode on the same disk is not affected by any of this.
        let whole = ssh(
            &vm,
            &busctl("ilex-installer", "Plan", &[&request(&disk.id, |_| {})]),
        )?;
        ensure!(
            whole.code == Some(0) || refusal(&whole)?.code != ErrorCode::FreeSpaceUnavailable,
            "{}: whole-disk mode was refused for beside's reasons",
            case.name
        );
    }
    // The model and the helper agree on every disk: what the model says of a
    // probe is what Plan says.
    for case in &cases {
        let disk = find_disk(&probe, case.bytes)?;
        ensure!(
            model::assess(&disk).is_ok() == case.says.is_none(),
            "{}: the model says {:?}",
            case.name,
            model::assess(&disk).err()
        );
    }

    // An ESP that fills up after the plan: `Apply` looks at it again, before it
    // claims the disk, and refuses, with nothing written.
    let late_disk = find_disk(&probe, 44 * GIB + MIB)?;
    let plan = plan(&vm, &free_request(&late_disk.id, false, |_| {}))?;
    ok(
        &vm,
        &format!(
            "set -e; mkdir -p /run/late; mount {}1 /run/late; fallocate -l 80M /run/late/filler.bin; umount /run/late",
            late_disk.path
        ),
    )?;
    let table = ok(&vm, &format!("sfdisk -d {}", late_disk.path))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &plan.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    let error = refused(
        &apply(&vm, "ilex-installer", &plan.id, &late_disk.id)?,
        ErrorCode::FreeSpaceUnavailable,
        Some("mode"),
    )?;
    println!("     the ESP filled up after the plan: {}", error.message);
    let s = state(&vm)?;
    ensure!(
        !s.active && s.outcome.is_none() && !s.disk_changed,
        "the refused Apply left the helper in {s:?}"
    );
    ensure!(
        ok(&vm, &format!("sfdisk -d {}", late_disk.path))? == table,
        "the partition table changed during a refused Apply"
    );
    ensure!(
        fingerprint(&vm, &[&sentinel])? == sentinel_before,
        "the sentinel changed"
    );
    selinux::check(&vm, &bench.artifacts, "installer-free-space-refusals")?;
    drop(vm);
    Ok(())
}

// --- installer::free_space_failure ---------------------------------------------------

/// What a trial checks, with its VM stopped, after an installation that failed:
/// the other system's partitions, every file of its EFI directory and the MBR's
/// boot code are byte for byte what they were.
struct Start {
    partitions: Vec<(u32, String)>,
    esp: BTreeMap<String, String>,
    mbr: String,
}

impl Start {
    fn take(bench: &Bench, l: &Layout, files_dir: &Path) -> Result<Self> {
        Ok(Self {
            partitions: hashes(&bench.target, l)?,
            esp: esp_files(&bench.target, l, files_dir)?,
            mbr: region_hash(&bench.target, 0, 440)?,
        })
    }

    fn unchanged(&self, bench: &Bench, l: &Layout, files_dir: &Path, what: &str) -> Result<()> {
        ensure!(
            hashes(&bench.target, l)? == self.partitions,
            "{what}: a partition of the other system changed"
        );
        ensure!(
            esp_files(&bench.target, l, files_dir)? == self.esp,
            "{what}: the shared EFI system partition is not byte for byte what it was"
        );
        ensure!(
            region_hash(&bench.target, 0, 440)? == self.mbr,
            "{what}: the first 440 bytes of the disk changed"
        );
        Ok(())
    }
}

/// An installation made to fail by a test credential (`ilex.installer.fault`):
/// `image` fails before `bootc` runs; `esp-check` fails right after it, when
/// `bootupd` has written ilex's loader to the shared partition and replaced the
/// firmware's entry that has its label (the live disk's own `Fedora` entry and a
/// stale one whose disk is gone), and nothing is put back yet, so the undo has
/// all of it to do. Either way the disk, the shared partition, the firmware's
/// variables (byte for byte, from `efivarfs`) and the MBR are what they were,
/// and exactly the partitions the run made are gone.
fn failed(
    bench: &mut Bench,
    l: &Layout,
    start: &Start,
    files_dir: &Path,
    fault: &str,
    stage: Stage,
) -> Result<()> {
    bench
        .smbios
        .retain(|s| !s.contains("ilex.installer.fault="));
    bench.smbios.push(format!(
        "type=11,value=io.systemd.credential:ilex.installer.fault={fault}"
    ));
    let vm = bench.boot_live()?;
    let probe0 = probe(&vm)?;
    let target = find_disk(&probe0, DISK)?;
    // The other system's own entry and another Fedora's on a disk that is gone
    // are made once, in the first boot (the firmware's variables are shared).
    let entries = boot_entries(&vm)?;
    if entry_number(&entries, "Windows Boot Manager").is_none() {
        ok(
            &vm,
            &format!(
                "efibootmgr --create --disk /dev/disk/by-id/{} --part 1 --label 'Windows Boot Manager' --loader '\\EFI\\Microsoft\\Boot\\bootmgfw.efi' >/dev/null",
                target.id
            ),
        )?;
        add_stale_fedora(&vm)?;
    }
    let nvram_before = nvram(&vm)?;
    ensure!(
        nvram_before.contains_key("Boot0042") && nvram_before.contains_key("BootOrder"),
        "the firmware's variables before: {nvram_before:?}"
    );
    let esp_node = target.partitions[0].path.clone();
    let esp_guest_before = esp_in_guest(&vm, &esp_node)?;
    let failing = plan(&vm, &free_request(&target.id, false, |_| {}))?;
    let ran = set_secret(
        &vm,
        "ilex-installer",
        &failing.id,
        "admin-password",
        PASSWORD,
        true,
    )?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    document(
        &apply(&vm, "ilex-installer", &failing.id, &target.id)?,
        "Apply",
    )?;
    let mut trace = Trace::new();
    let finished = wait_for_end(&vm, &mut trace, INSTALL_TIMEOUT)?;
    let reason = match &finished.outcome {
        Some(Outcome::Failed { stage: at, reason }) if *at == stage => reason.clone(),
        other => bail!("the installation was to fail in the {stage:?} stage ({fault}): {other:?}"),
    };
    println!("     fault={fault}: {reason}");
    ensure!(
        reason.contains("The partitions it made were removed again")
            && reason.contains("nothing of the other systems on the disk was changed"),
        "the outcome does not say the disk was restored: {reason}"
    );
    ensure!(
        finished.disk_changed,
        "the state does not say the disk was written to"
    );
    if fault == "esp-check" {
        let logs = ok(
            &vm,
            &format!(
                "cd /run/ilex-installer/log/{} && grep -h -e 'firmware' -e 'shared EFI' -e 'fault=' *.log",
                failing.id
            ),
        )?;
        bench.save(&format!("failed-{fault}-log.txt"), &logs);
        ensure!(
            logs.contains("the firmware's boot variables are as they were found, byte for byte")
                && logs.contains(
                    "what the installation added to the shared EFI system partition was removed"
                ),
            "the log of the undo: {logs}"
        );
    }
    ok(&vm, "sync")?;
    let after = find_disk(&self::probe(&vm)?, DISK)?;
    ensure!(
        after.partitions == target.partitions
            && after.free_extents == target.free_extents
            && after.esps == target.esps,
        "the disk after the installation that failed in {fault} is not the disk before:\n{target:#?}\n{after:#?}"
    );
    let nvram_after = nvram(&vm)?;
    bench.save(
        &format!("failed-{fault}-nvram.txt"),
        &format!("{nvram_before:#?}\n{nvram_after:#?}"),
    );
    ensure!(
        nvram_after == nvram_before,
        "the firmware's variables are not byte for byte what they were after a failure ({fault}):\n{nvram_before:#?}\n{nvram_after:#?}"
    );
    ensure!(
        esp_in_guest(&vm, &esp_node)? == esp_guest_before,
        "the shared EFI system partition is not what it was after a failure ({fault})"
    );
    selinux::check(
        &vm,
        &bench.artifacts,
        &format!("installer-free-space-failed-{fault}"),
    )?;
    drop(vm);
    start.unchanged(bench, l, files_dir, fault)
}

pub(super) fn failure() -> Result<()> {
    let mut bench = Bench::new("free-space-failure")?;
    bench.efi_vars = Some(bench.dir.join("OVMF_VARS.fd"));
    let _ = fs::remove_file(bench.efi_vars.as_ref().context("vars")?);
    let l = prepare(&bench.target)?;
    let files_dir = bench.dir.join("esp-files");
    let start = Start::take(&bench, &l, &files_dir)?;
    failed(&mut bench, &l, &start, &files_dir, "image", Stage::Image)?;
    failed(
        &mut bench,
        &l,
        &start,
        &files_dir,
        "esp-check",
        Stage::Image,
    )
}

// --- installer::free_space_answers --------------------------------------------------

/// Boot the live image with the answers on their stick and follow the serial
/// console until the answers program says how it ended. The machine is returned
/// (powered off by the program when the installation succeeded) with the
/// console's text.
fn answers_boot(bench: &Bench, what: &str) -> Result<(Vm, String, String)> {
    let mut live = Vm::boot(&bench.ws, &bench.live_config())?;
    let serial = live.info.dir.join("serial.log");
    let started = Instant::now();
    let outcome = loop {
        let text = fs::read_to_string(&serial).unwrap_or_default();
        if let Some(line) = text
            .lines()
            .find(|l| l.contains("ILEX-INSTALLER-ANSWERS") && l.contains("result="))
        {
            break line.to_owned();
        }
        if let Some(status) = live.exited()? {
            bail!(
                "{what}: the VM exited ({status}) before the answers ended:\n{}",
                tail(&text)
            );
        }
        ensure!(
            started.elapsed() < INSTALL_TIMEOUT + Duration::from_secs(300),
            "{what}: the unattended installation did not end:\n{}",
            tail(&text)
        );
        std::thread::sleep(Duration::from_secs(5));
    };
    // The report of SELinux, which the program prints as it ends.
    std::thread::sleep(Duration::from_secs(8));
    let text = fs::read_to_string(&serial).unwrap_or_default();
    Ok((live, outcome, text))
}

pub(super) fn answers() -> Result<()> {
    let mut bench = Bench::new("free-space-answers")?;
    bench.efi_vars = Some(bench.dir.join("OVMF_VARS.fd"));
    let _ = fs::remove_file(bench.efi_vars.as_ref().context("vars")?);
    let l = prepare(&bench.target)?;
    let before = hashes(&bench.target, &l)?;
    let files_dir = bench.dir.join("esp-files");
    let esp_before = esp_files(&bench.target, &l, &files_dir)?;
    // The stick: a FAT volume labelled ILEXANSWERS with `answers.toml`. The
    // target is the first extra disk, `virtio-ilex-disk-1`; the volume is the
    // first stick.
    let stick = bench.dir.join("answers.img");
    fs::File::create(&stick)?.set_len(64 << 20)?;
    run(
        "mkfs.fat",
        &["-F", "32", "-n", "ILEXANSWERS", &stick.to_string_lossy()],
    )?;
    let text = format!(
        r#"schema = 1

[install]
composition = "generic.gnome"
disk = "virtio-ilex-disk-1"
mode = "free-space"

[encryption]
enabled = false

[system]
host-name = "{HOST}"

[administrator]
name = "{ADMIN}"
password = "{PASSWORD_PLAIN}"
"#,
        PASSWORD_PLAIN = "Correct horse: battery staple"
    );
    let file = bench.dir.join("answers.toml");
    fs::write(&file, &text)?;
    run(
        "mcopy",
        &[
            "-i",
            &stick.to_string_lossy(),
            &file.to_string_lossy(),
            "::answers.toml",
        ],
    )?;
    bench.sticks = vec![stick.clone()];
    // The live disk is an ostree host, which `bootc install` refuses to run
    // from ("Multiple commit objects found"; the other trials hide
    // `/sysroot/ostree` once the VM is up, and answers mode starts at boot): the
    // helper's own mount namespace gets an empty one before it starts.
    bench.smbios.push(format!(
        "type=11,value=io.systemd.credential.binary:systemd.unit-dropin.ilex-installerd.service={}",
        base64(b"[Service]\nTemporaryFileSystem=/sysroot/ostree\n")
    ));
    // The live disk starts the answers unit, not the interface; the harness
    // has no SSH on a machine that powers itself off, so it follows the serial
    // console.
    let (mut live, outcome, console) = answers_boot(&bench, "free-space answers")?;
    bench.save("answers-serial.txt", &console);
    ensure!(
        outcome.contains("result=success"),
        "the unattended installation: {outcome}\n{}",
        tail(&console)
    );
    ensure!(
        console.contains("plan=ready disk=virtio-ilex-disk-1")
            && console.contains("apply=started")
            && !console.contains("reinstall=true")
            && !console.contains("Correct horse"),
        "the answers' lines:\n{}",
        tail(&console)
    );
    selinux::assert_console(&console, &bench.artifacts, "installer-free-space-answers")?;
    println!("     unattended, beside the other system: {outcome}");
    // It powers the machine off at the end.
    let deadline = Instant::now() + Duration::from_secs(180);
    while live.exited()?.is_none() {
        ensure!(Instant::now() < deadline, "the machine did not power off");
        std::thread::sleep(Duration::from_secs(2));
    }
    drop(live);
    ensure!(
        hashes(&bench.target, &l)? == before,
        "a partition of the other system changed"
    );
    let esp_after = esp_files(&bench.target, &l, &files_dir)?;
    for (name, hash) in &esp_before {
        ensure!(
            esp_after.get(name) == Some(hash),
            "{name} changed on the shared EFI system partition"
        );
    }
    // Booted: the firmware starts ilex through the entry the installation made.
    let mut installed = Vm::boot(&bench.ws, &bench.live_config())?;
    installed.wait_ssh(Duration::from_secs(900))?;
    let digest = ok(
        &installed,
        "bootc status --json | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"status\"][\"booted\"][\"image\"][\"imageDigest\"])'",
    )?;
    ensure!(digest.starts_with("sha256:"), "the booted digest: {digest}");
    ensure!(
        ok(&installed, "hostnamectl --static")? == HOST,
        "the installed system is not the one the answers made"
    );
    Ok(())
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(25)..].join("\n")
}
