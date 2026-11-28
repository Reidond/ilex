//! Helpers the live installer's ISO trials share: SMBIOS fixtures for a
//! machine of the catalogue, and editing GRUB's kernel line with real key input.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ilex_devkit::cmd;
use ilex_devkit::installer;
use ilex_devkit::qemu::Vm;
use ilex_vmctl::Vmctl;

/// How long GRUB's editor may take to draw its last line.
const GRUB_TIMEOUT: Duration = Duration::from_secs(300);

/// The `-smbios` structures of a machine's DMI match rule (the system pair, or
/// with `board_only` the board pair), as QEMU takes them.
pub(crate) fn smbios(
    table: &installer::Table,
    machine: Option<&str>,
    board_only: bool,
) -> Result<Vec<String>> {
    let Some(machine) = machine else {
        return Ok(vec![]);
    };
    let rules = &table
        .machines
        .get(machine)
        .with_context(|| format!("machine {machine} is not offered"))?
        .matches;
    if rules.is_empty() {
        bail!("machine {machine} has no DMI fixture");
    }
    let mut structures = Vec::new();
    for (vendor, model, kind) in [
        ("sys_vendor", "product_name", 1),
        ("board_vendor", "board_name", 2),
    ] {
        if kind == 1 && board_only {
            continue;
        }
        if let Some(rule) = rules
            .iter()
            .find(|r| r.contains_key(vendor) && r.contains_key(model))
        {
            structures.push(format!(
                "type={kind},manufacturer={},product={}",
                rule[vendor].replace(',', ",,"),
                rule[model].replace(',', ",,")
            ));
        }
    }
    if structures.is_empty() {
        bail!("no complete SMBIOS fixture for {machine}");
    }
    Ok(structures)
}

fn wait_serial(vm: &mut Vm, serial: &Path, wanted: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        let console = fs::read_to_string(serial).unwrap_or_default();
        if console.contains(wanted) {
            return Ok(());
        }
        if started.elapsed() > GRUB_TIMEOUT || vm.exited()?.is_some() {
            bail!("missing {wanted}; serial: {console}");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Append `arguments` to the first entry's kernel line of the ISO's GRUB menu
/// with actual keyboard input, then boot it. The caller waited for the menu's
/// countdown. Read its generated config to find the linux line; never modify
/// or repack the ISO. The text typed is checked by the caller's markers.
#[allow(clippy::too_many_arguments)]
pub(crate) fn edit_kernel_line(
    iso: &Path,
    work: &Path,
    arguments: &str,
    ctl: &Vmctl,
    artifacts: &Path,
    screenshot: &str,
    vm: &mut Vm,
    serial: &Path,
) -> Result<()> {
    // The caller waited for the menu countdown. Stop it before reading the
    // ISO, so even slow storage cannot let the default entry boot mid-edit.
    grub_key(ctl, "up")?;
    let grub = work.join("grub.cfg");
    cmd::run(
        Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(iso)
            .args(["-extract", "/EFI/BOOT/grub.cfg"])
            .arg(&grub),
    )?;
    let original = fs::read_to_string(&grub)?;
    let entry = original
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("menuentry "))
        .skip(1)
        .take_while(|l| l.trim() != "}")
        .collect::<Vec<_>>();
    let line = entry
        .iter()
        .position(|l| {
            l.trim_start().starts_with("linux ") || l.trim_start().starts_with("linuxefi ")
        })
        .context("no linux line in ISO's first GRUB entry")?;
    let last_line = entry
        .iter()
        .rfind(|l| !l.trim().is_empty())
        .context("empty GRUB entry")?;
    grub_key(ctl, "e")?;
    // Firmware can spend seconds drawing the editor. Keys sent before its
    // final command is visible are lost, including the cursor movement.
    wait_serial(vm, serial, last_line.trim())?;
    grub_key(ctl, "ctrl+home")?;
    // GRUB inserts setparams and a blank line before the entry's commands.
    for _ in 0..line + 2 {
        grub_key(ctl, "down")?;
    }
    grub_key(ctl, "end")?;
    for character in arguments.chars() {
        let key = match character {
            ' ' => "space".into(),
            // A literal plus cannot be a key name (`+` separates the keys of a
            // combination): it is Shift and the equals key.
            '+' => "shift+=".into(),
            other => other.to_string(),
        };
        grub_key(ctl, &key)?;
    }
    std::thread::sleep(Duration::from_millis(500));
    ctl.save_screenshot(&artifacts.join(screenshot))?;
    grub_key(ctl, "f10")?; // GRUB's single-key equivalent of Ctrl+X.
    Ok(())
}

/// GRUB polls firmware input and drops the desktop typing path's 10 ms key
/// presses. Use vmctl's QMP transport with 100 ms physical key holds and a gap.
pub(crate) fn grub_key(ctl: &Vmctl, combo: &str) -> Result<()> {
    let keys = ilex_vmctl::keys::parse_combo(combo)?
        .into_iter()
        .map(|key| serde_json::json!({"type": "qcode", "data": key}))
        .collect::<Vec<_>>();
    ilex_vmctl::qmp::Qmp::connect(&ctl.info.qmp_sock)?.execute(
        "send-key",
        serde_json::json!({"keys": keys, "hold-time": 100}),
    )?;
    std::thread::sleep(Duration::from_millis(250));
    Ok(())
}
