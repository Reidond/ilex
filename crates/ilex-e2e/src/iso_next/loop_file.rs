//! The live ISO as a *file* on another disk: how GRUB's loopback with dracut's
//! `iso-scan/filename=` (and, with a mapper device in between, Ventoy) boots
//! it. No ISO is attached to the VM at all.
//!
//! * the VM's own first disk is the host disk: a GPT with one ext4 partition
//!   labelled `ISOHOST` that holds the ISO at [`ISO_FILE`];
//! * the boot loader is the ISO's own signed shim and GRUB, taken from its
//!   `/EFI` tree and served as a small FAT disk ([`VmConfig::esp_dir`]), with a
//!   `grub.cfg` that finds the host disk by label, loops the ISO file and
//!   boots the kernel inside it with the ISO's own arguments and
//!   `iso-scan/filename=` for the file.
//!   (Firmware connects only the disk it boots from, which here is the FAT
//!   disk, not the host disk, so the config asks for the others with
//!   `connectefi pciroot`: a boot from the host disk's own ESP, as on a real
//!   stick or with Ventoy, needs no such line.)
//!
//! Everything is built without privileges: `mke2fs -d` fills the file system
//! from a directory at an offset inside the disk file, `sfdisk` writes the
//! table, and the loader files come out of the ISO with `xorriso`.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, ensure};
use ilex_devkit::cmd;

/// The host partition's label, and where the ISO lies on it.
pub(super) const HOST_LABEL: &str = "ISOHOST";
pub(super) const ISO_FILE: &str = "/boot/isos/ilex.iso";

const MIB: u64 = 1 << 20;

/// What was built: the host disk and the directory of the FAT disk with the
/// boot loader.
pub(super) struct Fixture {
    pub disk: PathBuf,
    pub esp: PathBuf,
    /// The kernel arguments the ISO's own menu gives, and the ones this boot
    /// gives (saved with the evidence).
    pub original_args: String,
    pub args: String,
}

pub(super) fn build(root: &Path, iso: &Path) -> Result<Fixture> {
    for tool in ["xorriso", "mke2fs", "sfdisk"] {
        cmd::require(tool)?;
    }
    let esp = root.join("esp");
    let _ = fs::remove_dir_all(&esp);
    fs::create_dir_all(&esp)?;
    // The ISO's own boot loader: shim as the removable-media fallback, GRUB
    // next to it. `-osirrox` keeps the ISO's read-only modes: made writable
    // below, for the one file that is replaced.
    cmd::run(
        Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(iso)
            .arg("-extract")
            .arg("/EFI")
            .arg(esp.join("EFI"))
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )?;
    make_writable(&esp)?;
    let grub = esp.join("EFI/BOOT/grub.cfg");
    let menu =
        fs::read_to_string(&grub).with_context(|| format!("the ISO has no {}", grub.display()))?;
    let (kernel, initrd, original_args) = boot_entry(&menu)?;
    let args = loop_args(&original_args)?;
    fs::write(
        &grub,
        format!(
            "set timeout=0\nset default=0\n\
             connectefi pciroot\n\
             search --no-floppy --label {HOST_LABEL} --set=host\n\
             loopback loop ($host){ISO_FILE}\n\
             menuentry 'ilex from a file' {{\n\
             \tlinux (loop){kernel} {args}\n\
             \tinitrd (loop){initrd}\n\
             }}\n"
        ),
    )?;

    // The host disk: the ISO is copied into a tree that `mke2fs -d` copies
    // into the file system, 1 MiB into the disk, where the partition starts.
    let tree = root.join("host-tree");
    let _ = fs::remove_dir_all(&tree);
    let file = tree.join(ISO_FILE.trim_start_matches('/'));
    fs::create_dir_all(file.parent().unwrap())?;
    fs::copy(iso, &file).with_context(|| format!("copying {}", iso.display()))?;
    let iso_bytes = fs::metadata(iso)?.len();
    // Room for the ISO and an ext4's own overhead, and the disk's two tables.
    let partition = (iso_bytes + 768 * MIB).div_ceil(MIB) * MIB;
    let disk = root.join("host.raw");
    let _ = fs::remove_file(&disk);
    fs::File::create(&disk)?.set_len(MIB + partition + MIB)?;
    cmd::run(
        Command::new("mke2fs")
            // Only what GRUB's ext2 driver reads: Fedora's mke2fs profile adds
            // `metadata_csum_seed` and `orphan_file`, which it does not.
            .args(["-q", "-F", "-t", "ext4", "-L", HOST_LABEL])
            .args(["-O", "^metadata_csum_seed,^orphan_file", "-d"])
            .arg(&tree)
            .arg("-E")
            .arg(format!("offset={MIB}"))
            .arg(&disk)
            .arg(format!("{}k", partition / 1024)),
    )?;
    sfdisk(
        &disk,
        &format!(
            "label: gpt\nstart={}, size={}, type=L, name=\"ISO host\"\n",
            MIB / 512,
            partition / 512
        ),
    )?;
    let _ = fs::remove_dir_all(&tree);
    Ok(Fixture {
        disk,
        esp,
        original_args,
        args,
    })
}

/// Everything below `dir` owner-writable (an ISO tree is read-only).
fn make_writable(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_symlink() {
            continue;
        }
        let mut mode = meta.permissions();
        mode.set_mode(mode.mode() | 0o700);
        fs::set_permissions(&path, mode)?;
        if meta.is_dir() {
            make_writable(&path)?;
        }
    }
    Ok(())
}

fn sfdisk(disk: &Path, script: &str) -> Result<()> {
    let mut child = Command::new("sfdisk")
        .arg("--quiet")
        .arg(disk)
        .stdin(Stdio::piped())
        .spawn()
        .context("spawning sfdisk")?;
    child
        .stdin
        .take()
        .context("sfdisk's stdin")?
        .write_all(script.as_bytes())?;
    let status = child.wait()?;
    ensure!(status.success(), "sfdisk failed with {status}");
    Ok(())
}

/// The kernel, the initramfs and the kernel arguments of the ISO's first menu
/// entry (`linux /images/pxeboot/vmlinuz root=live:CDLABEL=… quiet …`).
fn boot_entry(menu: &str) -> Result<(String, String, String)> {
    let word = |line: &str, key: &str| -> Option<String> {
        let rest = line.trim().strip_prefix(key)?;
        rest.starts_with([' ', '\t'])
            .then(|| rest.trim().to_owned())
    };
    let mut linux = None;
    let mut initrd = None;
    for line in menu.lines() {
        if linux.is_none() {
            linux = word(line, "linux").or_else(|| word(line, "linuxefi"));
        } else if initrd.is_none() {
            initrd = word(line, "initrd").or_else(|| word(line, "initrdefi"));
        }
    }
    let linux = linux.with_context(|| format!("no linux line in the ISO's menu:\n{menu}"))?;
    let initrd = initrd.with_context(|| format!("no initrd line in the ISO's menu:\n{menu}"))?;
    let (kernel, args) = linux.split_once(' ').unwrap_or((&linux, ""));
    Ok((kernel.to_owned(), initrd, args.trim().to_owned()))
}

/// The arguments for a boot from a file: the ISO's own, and
/// `iso-scan/filename=`. dracut's `iso-scan` looks for the file on the disks
/// the kernel has, mounts the one that holds it and attaches the ISO to a loop
/// device, whose label is the ISO's own: `root=live:CDLABEL=…` is therefore
/// left as the ISO has it, and finds that loop device.
fn loop_args(original: &str) -> Result<String> {
    ensure!(
        original
            .split_whitespace()
            .any(|arg| arg.starts_with("root=live:")),
        "the ISO's arguments have no root=live: ({original})"
    );
    let mut args: Vec<String> = original.split_whitespace().map(str::to_owned).collect();
    args.push(format!("iso-scan/filename={ISO_FILE}"));
    // `ILEX_ISO_NEXT_LOOP_ARGS` adds arguments (`console=ttyS0 rd.debug` to
    // read dracut's own account of a boot that stops early) and drops the
    // two that silence the console.
    if let Some(extra) = std::env::var("ILEX_ISO_NEXT_LOOP_ARGS")
        .ok()
        .filter(|extra| !extra.trim().is_empty())
    {
        args.retain(|arg| arg != "quiet" && arg != "rhgb");
        args.push(extra.trim().to_owned());
    }
    Ok(args.join(" "))
}
