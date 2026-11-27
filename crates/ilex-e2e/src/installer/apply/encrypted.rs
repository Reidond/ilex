//! `installer::encrypted` and `installer::encrypted_german`: the helper writing
//! an encrypted disk, for real, and the disk it wrote unlocked at its own boot
//! prompt.
//!
//! The trial installs `generic.gnome` onto a whole disk with LUKS2 (the
//! passphrase through `SetSecret`, as the interface gives it), then checks from
//! the live system what is on the disk (LUKS2, one keyslot derived with
//! Argon2id, `aes-xts-plain64`, a 512-bit payload key; the passphrase opens it
//! and a wrong one does not; no disk swap, `/var` inside the volume; the
//! `/etc/crypttab` line and the kernel arguments the installed system unlocks
//! with) and that the passphrase is nowhere it must not be (the helper's state
//! and logs, the journal, the log saved to a stick, the installed system's own
//! logs). The disk is then booted, and the passphrase is typed at the real
//! prompt through QMP: the keys a person presses, in the layout the interface
//! was in, so the German run's passphrase has characters whose keys differ
//! from a US keyboard's. The booted system must reach `running` with the root
//! on the mapped volume, and every assertion of `installer::install` holds.

use super::*;

/// One passphrase and the keyboard it is typed on.
pub(crate) struct Variant {
    pub name: &'static str,
    /// The console keymap the installation is asked for.
    pub keymap: &'static str,
    /// The XKB layout that keymap is in the installed system's `localectl`.
    pub layout: &'static str,
    /// The passphrase, as the characters the person means.
    pub passphrase: &'static str,
    /// The keys that make it on this keyboard, named as a US keyboard names the
    /// keys at the same places (what QMP can press).
    pub keys: &'static str,
}

/// A passphrase with what a person types: spaces, quotes, a colon, an
/// ampersand. It must survive the memfd, the bus, repart's key file,
/// cryptsetup and the console prompt.
pub(crate) const US: Variant = Variant {
    name: "encrypted",
    keymap: "us",
    layout: "us",
    passphrase: "Plant-the-Holly: 7 'berries' & \"green\"",
    keys: "Plant-the-Holly: 7 'berries' & \"green\"",
};

/// A German keyboard: `z` and `y` swapped, `ü` and `ß` and the hyphen on the
/// keys a US keyboard has `[`, `-` and `/` on. Neither run's characters are the
/// same keys on the other layout, so a prompt that is not in the layout the
/// passphrase was typed in cannot open the volume.
pub(crate) const GERMAN: Variant = Variant {
    name: "encrypted-german",
    keymap: "de-nodeadkeys",
    layout: "de",
    passphrase: "Süß-Zebra-yoga-2026",
    keys: "S[-/Yebra/zoga/2026",
};

/// The log stick: a 64 MiB image with a table and one FAT32 partition, as a
/// stick from a shop is.
fn fat_stick(path: &Path) -> Result<()> {
    fs::File::create(path)?.set_len(64 << 20)?;
    let mut child = Command::new("sfdisk")
        .arg("--quiet")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("starting sfdisk")?;
    std::io::Write::write_all(
        child.stdin.as_mut().context("sfdisk's input")?,
        b"label: dos\nstart=2048, type=c\n",
    )?;
    ensure!(child.wait()?.success(), "sfdisk could not write the table");
    ilex_devkit::cmd::run(
        Command::new("mkfs.fat")
            .args(["-F", "32", "-n", "ILEXLOG", "--offset", "2048"])
            .arg(path),
    )?;
    Ok(())
}

/// A script that has the passphrase in `$PASS` (base64 on the command line, so
/// no quoting depends on its text).
fn with_passphrase(passphrase: &str, body: &str) -> String {
    format!(
        "PASS=$(printf %s {} | base64 -d)\n{body}",
        base64(passphrase.as_bytes())
    )
}

pub(super) fn run(variant: &'static Variant) -> Result<()> {
    let mut bench = Bench::new(variant.name)?;
    let stick = bench.dir.join("stick.img");
    fat_stick(&stick)?;
    bench.sticks = vec![stick.clone()];
    let vm = bench.boot_live()?;
    let probe = probe(&vm)?;
    let (target, sentinel) = disks_of(&probe)?;
    let stick_disk = probe
        .disks
        .iter()
        .find(|d| d.removable && d.id.contains("ILEXSTICK0"))
        .map(|d| d.id.clone())
        .with_context(|| {
            format!(
                "no stick in the probe: {:?}",
                probe.disks.iter().map(|d| &d.id).collect::<Vec<_>>()
            )
        })?;
    let before = fingerprint(&vm, &[&target, &sentinel])?;
    let sentinel_before = fingerprint(&vm, &[&sentinel])?;

    // --- the plans -----------------------------------------------------------
    let encrypted = |edit: &dyn Fn(&mut Value)| {
        request(&target.id, |r| {
            r["encrypt"] = json!(true);
            r["keyboard"] = json!(variant.keymap);
            r["host_name"] = json!("ilexcrypt");
            edit(r);
        })
    };
    // The security chip is left at its default (`auto`): the installation does
    // not enrol it yet, and the plan says so.
    let defaulted = plan(&vm, &encrypted(&|_| {}))?;
    ensure!(
        defaulted.encryption.enabled
            && !defaulted.encryption.tpm.will_enroll
            && defaulted
                .warnings
                .iter()
                .any(|w| w.contains("security chip") && w.contains("passphrase")),
        "the plan of an encrypted installation with the chip left to auto: {:?} {:?}",
        defaulted.encryption,
        defaulted.warnings
    );
    let encrypted_partitions: Vec<(&str, bool)> = defaulted
        .partitions
        .iter()
        .map(|p| (p.kind.as_str(), p.encrypted))
        .collect();
    ensure!(
        encrypted_partitions
            == [
                ("esp", false),
                ("xbootldr", false),
                ("root-x86-64", true)
            ],
        "which partitions are encrypted: {encrypted_partitions:?}"
    );
    // What the interface asks: the chip off, so the plan has nothing to warn of.
    let plan = plan(&vm, &encrypted(&|r| r["tpm"] = json!("off")))?;
    bench.save("plan.json", &serde_json::to_string_pretty(&plan)?);
    ensure!(
        plan.encryption.enabled
            && !plan.warnings.iter().any(|w| w.contains("security chip"))
            && plan.partitions.len() == 3,
        "the plan: {:?}",
        plan.warnings
    );

    // --- what Apply and SetSecret refuse --------------------------------------
    // No passphrase: nothing begins and the disks are as they were.
    let ran = set_secret(&vm, "ilex-installer", &plan.id, "admin-password", PASSWORD, true)?;
    ensure!(ran.code == Some(0), "SetSecret: {}", ran.text);
    refused(
        &apply(&vm, "ilex-installer", &plan.id, &target.id)?,
        ErrorCode::MissingSecret,
        Some("disk-passphrase"),
    )?;
    nothing_happened(&vm, &before, &[&target, &sentinel], &plan.id)?;
    // A passphrase the prompt cannot take, and one that is too long.
    for (what, bad) in [
        ("a line break", "line\nbreak".to_owned()),
        ("a tab", "tab\there".to_owned()),
        ("513 bytes", "x".repeat(513)),
    ] {
        refused(
            &set_secret(&vm, "ilex-installer", &plan.id, "disk-passphrase", &bad, true)?,
            ErrorCode::InvalidSecret,
            Some("fd"),
        )
        .with_context(|| format!("a passphrase with {what}"))?;
    }
    // Not sealed: refused like the password's.
    refused(
        &set_secret(&vm, "ilex-installer", &plan.id, "disk-passphrase", variant.passphrase, false)?,
        ErrorCode::InvalidSecret,
        Some("fd"),
    )?;
    // A passphrase for a plan that does not encrypt.
    let plain = self::plan(&vm, &request(&target.id, |_| {}))?;
    refused(
        &set_secret(&vm, "ilex-installer", &plain.id, "disk-passphrase", variant.passphrase, true)?,
        ErrorCode::InvalidSecret,
        Some("fd"),
    )?;
    // The plan that is applied is the encrypted one again (a plan is the
    // latest of its request; the earlier ones are still valid).
    nothing_happened(&vm, &before, &[&target, &sentinel], &plan.id)?;
    println!("     refused: no passphrase, a line break, a tab, 513 bytes, unsealed, an unencrypted plan's");

    // --- the installation ----------------------------------------------------
    ok(
        &vm,
        "nohup busctl --system --json=short monitor org.ilex.Installer1 > /run/signals.json 2>/dev/null &\necho $! > /run/monitor.pid; sleep 1; echo monitoring",
    )?;
    for (kind, content) in [
        ("disk-passphrase", variant.passphrase),
        ("admin-password", PASSWORD),
    ] {
        let ran = set_secret(&vm, "ilex-installer", &plan.id, kind, content, true)?;
        ensure!(ran.code == Some(0), "SetSecret {kind}: {}", ran.text);
    }
    let started: State = serde_json::from_value(document(
        &apply(&vm, "ilex-installer", &plan.id, &target.id)?,
        "Apply",
    )?)?;
    ensure!(
        started.active
            && started.stages.iter().map(|r| r.stage).collect::<Vec<_>>()
                == [
                    Stage::Partition,
                    Stage::Encrypt,
                    Stage::Mount,
                    Stage::Image,
                    Stage::Configure,
                    Stage::Account,
                    Stage::Finalise
                ]
            && started.summary.as_ref().is_some_and(|s| s.encrypted),
        "Apply's state: {started:?}"
    );
    let mut trace = Trace::new();
    let begun = Instant::now();
    let finished = wait_for_end(&vm, &mut trace, INSTALL_TIMEOUT)?;
    println!("     installed in {:?}", begun.elapsed());
    bench.save("state.json", &serde_json::to_string_pretty(&finished)?);
    ensure!(
        finished.outcome == Some(Outcome::Success)
            && finished
                .stages
                .iter()
                .all(|r| r.status == StageStatus::Done && r.started.is_some() && r.finished.is_some()),
        "the installation ended: {:?}\n{}",
        finished.outcome,
        finished.log.join("\n")
    );
    // (A poll every few seconds misses the short stages; the signals below
    // say that every one of them ran.)
    let order: Vec<Stage> = {
        let mut stages: Vec<Stage> = trace.seen.iter().map(|(s, _)| *s).collect();
        stages.dedup();
        stages
    };
    ensure!(
        order.windows(2).all(|w| w[0] <= w[1]),
        "the stages went backwards: {order:?}"
    );
    let signals = ok(&vm, "kill $(cat /run/monitor.pid) || true; sleep 1; cat /run/signals.json")?;
    ensure!(
        ["partition", "encrypt", "mount", "image", "configure", "account", "finalise"]
            .iter()
            .all(|stage| signals.contains(&format!("\"{stage}\""))),
        "a stage was not announced on the bus"
    );

    // --- what is on the disk -------------------------------------------------
    let inspect = ok(
        &vm,
        &with_passphrase(variant.passphrase, ENCRYPTED_INSPECT),
    )?;
    bench.save("inspect.txt", &redact(&inspect, variant));
    println!(
        "{}",
        redact(&inspect, variant)
            .lines()
            .map(|l| format!("     | {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let found = encrypted_inspected(&inspect, &plan, variant)?;
    ensure!(
        fingerprint(&vm, &[&sentinel])? == sentinel_before,
        "the sentinel disk changed"
    );

    // --- the passphrase is nowhere it must not be ------------------------------
    // The helper's own files and the journal, then the log as it is saved to a
    // stick: the same files and the plan, `lsblk`, the state.
    let saved = document(
        &ssh(
            &vm,
            &busctl(
                "ilex-installer",
                "Finish",
                &[&format!("save-log:{stick_disk}")],
            ),
        )?,
        "Finish(save-log)",
    )?;
    ensure!(
        saved["file"].as_str().is_some_and(|f| f.ends_with(".tar")),
        "the saved log: {saved}"
    );
    ok(&vm, "sync")?;
    let scan = ok(
        &vm,
        &with_passphrase(
            variant.passphrase,
            &format!(
                r#"set -u
needle() {{ printf %s "$PASS"; }}
count() {{ grep -rlaF -e "$PASS" "$@" 2>/dev/null | wc -l; }}
echo "run_dir=$(count /run/ilex-installer)"
echo "state_json=$(grep -cF -e "$PASS" /run/ilex-installer/state.json || true)"
echo "journal=$(journalctl --no-pager 2>/dev/null | grep -cF -e "$PASS" || true)"
echo "dmesg=$(dmesg 2>/dev/null | grep -cF -e "$PASS" || true)"
echo "processes=$(ps -eo args | grep -v grep | grep -cF -e "$PASS" || true)"
mkdir -p /run/stickscan /run/stick
for p in /dev/disk/by-id/*ILEXSTICK0*-part1; do mount -o ro "$p" /run/stick; done
ls /run/stick
tar -C /run/stickscan -xf /run/stick/{file}
echo "tarball_files=$(ls /run/stickscan | wc -l)"
echo "tarball=$(count /run/stickscan)"
echo "tarball_raw=$(grep -caF -e "$PASS" /run/stick/{file} || true)"
echo "tarball_has_state=$(ls /run/stickscan | grep -c state || true)"
umount /run/stick
"#,
                file = saved["file"].as_str().unwrap_or_default()
            ),
        ),
    )?;
    bench.save("secret-scan.txt", &scan);
    for key in [
        "run_dir",
        "state_json",
        "journal",
        "dmesg",
        "processes",
        "tarball",
        "tarball_raw",
    ] {
        ensure!(
            field(&scan, key)? == "0",
            "the passphrase is in {key}: {scan}"
        );
    }
    ensure!(
        field(&scan, "tarball_files")? != "0" && field(&scan, "tarball_has_state")? != "0",
        "the saved log has nothing in it: {scan}"
    );
    ok(&vm, "sync")?;
    drop(vm);

    // --- the disk, booted ----------------------------------------------------
    let mut installed = bench.boot_target_encrypted(variant)?;
    check_installed(
        &|name, text| bench.save(name, text),
        &installed,
        &Expected {
            digest: &plan.image.digest,
            target: &plan.image.target,
            host: "ilexcrypt",
            zone: "Etc/UTC",
            locale: "en_US.UTF-8",
            keymap: variant.keymap,
            layout: variant.layout,
            admin: ADMIN,
            password: PASSWORD,
            stamp: &[],
        },
    )?;
    booted_encrypted(&installed, Some(&found.uuid), variant, &|name, text| {
        bench.save(name, text)
    })?;
    gdm_screenshot(
        &mut installed,
        &bench.artifacts.join(format!("installer-{}-gdm.png", variant.name)),
    )?;
    Ok(())
}

/// What the live system reads off the encrypted disk. One `key=value` per line;
/// the passphrase is `$PASS`.
const ENCRYPTED_INSPECT: &str = r#"set -u
root=/dev/disk/by-partlabel/ilex-root
boot=/dev/disk/by-partlabel/ilex-boot
echo "root_type=$(blkid -p -o value -s TYPE $root)"
echo "uuid=$(cryptsetup luksUUID $root)"
echo "version=$(cryptsetup luksDump $root | sed -n 's/^Version:[[:space:]]*//p')"
cryptsetup luksDump --dump-json-metadata $root | python3 -c '
import json, sys
h = json.load(sys.stdin)
k = h["keyslots"]
s = h["segments"]
print("keyslots=%d" % len(k))
print("kdf=%s" % ",".join(sorted(v["kdf"]["type"] for v in k.values())))
print("slot_key_bits=%s" % ",".join(str(v["key_size"] * 8) for v in k.values()))
print("segments=%d" % len(s))
print("cipher=%s" % ",".join(v["encryption"] for v in s.values()))
print("tokens=%d" % len(h.get("tokens", {})))
'
# The passphrase opens the volume, another does not (the test opens nothing).
printf %s "$PASS" | cryptsetup open --test-passphrase --key-file=- $root; echo "right_rc=$?"
printf %s "${PASS}x" | cryptsetup open --test-passphrase --key-file=- $root 2>/dev/null; echo "wrong_rc=$?"
printf %s "" | cryptsetup open --test-passphrase --key-file=- $root 2>/dev/null; echo "empty_rc=$?"
echo "mapper_before=$(ls /dev/mapper | grep -c '^luks-' || true)"
# Look inside: open it read-only and mount it.
t=/run/inspect
mkdir -p $t $t-boot
printf %s "$PASS" | cryptsetup open --key-file=- --readonly $root inspect-root
echo "inner_type=$(blkid -p -o value -s TYPE /dev/mapper/inspect-root)"
mount -o ro /dev/mapper/inspect-root $t
mount -o ro,noload $boot $t-boot
D=$(ls -d $t/ostree/deploy/default/deploy/*/ | head -1); D=${D%/}
echo "crypttab=$(cat $D/etc/crypttab)"
echo "crypttab_mode=$(stat -c %a $D/etc/crypttab)"
echo "vconsole=$(grep -E '^(KEYMAP|XKBLAYOUT)=' $D/etc/vconsole.conf | tr '\n' ' ')"
echo "setup=$(python3 -c 'import json,sys; print(json.dumps(json.load(open(sys.argv[1])), sort_keys=True))' $D/etc/ilex/setup.json)"
echo "fstab_swap=$(grep -cE '[[:space:]]swap[[:space:]]' $D/etc/fstab 2>/dev/null)"
echo "var_inside=$(test -d $t/ostree/deploy/default/var/home/ilexmaint && echo yes || echo no)"
echo "var_on_boot=$(test -e $t-boot/ostree/deploy && echo yes || echo no)"
echo "kargs=$(cat $t-boot/loader/entries/*.conf | grep -E '^options' | head -1)"
echo "entries=$(ls $t-boot/loader/entries | wc -l)"
echo "logs=$(ls $t/ostree/deploy/default/var/log/ilex-installer | tr '\n' ' ')"
echo "log_leak=$(grep -rlaF -e "$PASS" $t/ostree/deploy/default/var/log/ilex-installer | wc -l)"
echo "etc_leak=$(grep -rlaF -e "$PASS" $D/etc 2>/dev/null | wc -l)"
umount $t-boot
umount $t
cryptsetup close inspect-root
echo "mapper_after=$(ls /dev/mapper | grep -c -e '^luks-' -e '^inspect-' || true)"
echo "partitions=$(lsblk -n -o PARTLABEL,FSTYPE -r $root $boot /dev/disk/by-partlabel/ilex-esp | tr '\n' ';')"
swaps=$(cat /proc/swaps | tail -n +2 | grep -vc zram || true)
echo "disk_swaps=$swaps"
helper=$(systemctl show -p MainPID --value ilex-installerd)
echo "mounted_after=$(cat /proc/1/mountinfo /proc/$helper/mountinfo | grep -c ilex-installer/target || true)"
"#;

/// What the inspection found out, for the boot's checks.
struct Found {
    uuid: String,
}

fn encrypted_inspected(text: &str, plan: &Plan, variant: &Variant) -> Result<Found> {
    let want = |key: &str, wanted: &str| -> Result<()> {
        ensure!(
            field(text, key)? == wanted,
            "{key}: {:?}, wanted {wanted:?}\n{text}",
            field(text, key)?
        );
        Ok(())
    };
    want("root_type", "crypto_LUKS")?;
    want("version", "2")?;
    want("keyslots", "1")?;
    want("kdf", "argon2id")?;
    want("slot_key_bits", "512")?;
    want("segments", "1")?;
    want("cipher", "aes-xts-plain64")?;
    want("tokens", "0")?;
    want("right_rc", "0")?;
    ensure!(
        field(text, "wrong_rc")? != "0" && field(text, "empty_rc")? != "0",
        "a wrong or empty passphrase opened the volume"
    );
    want("inner_type", "btrfs")?;
    let uuid = field(text, "uuid")?.to_owned();
    ensure!(
        uuid.len() == 36,
        "the LUKS UUID: {uuid:?}"
    );
    // What the installed system finds the volume by.
    want("crypttab", &format!("luks-{uuid} UUID={uuid} none discard"))?;
    want("crypttab_mode", "600")?;
    let vconsole = field(text, "vconsole")?;
    ensure!(
        vconsole.contains(&format!("KEYMAP={}", variant.keymap))
            && vconsole.contains(&format!("XKBLAYOUT={}", variant.layout)),
        "vconsole.conf: {vconsole}"
    );
    let kargs = field(text, "kargs")?;
    for karg in [
        format!("rd.luks.uuid={uuid}"),
        format!("vconsole.keymap={}", variant.keymap),
        "rd.luks.options=tpm2-device=auto".to_owned(),
    ] {
        ensure!(
            kargs.split(' ').any(|k| k == karg),
            "the boot entry's kernel arguments lack {karg}: {kargs}"
        );
    }
    ensure!(
        kargs.split(' ').any(|k| k.starts_with("root=UUID=")),
        "the boot entry names no root: {kargs}"
    );
    // No disk swap, /var inside the volume.
    want("fstab_swap", "0")?;
    want("disk_swaps", "0")?;
    want("var_inside", "yes")?;
    want("var_on_boot", "no")?;
    // Three partitions and no more (no swap): the ESP, /boot and the volume.
    let partitions: std::collections::BTreeSet<&str> = field(text, "partitions")?
        .split(';')
        .filter(|p| !p.is_empty())
        .collect();
    ensure!(
        partitions
            == [
                "ilex-esp vfat",
                "ilex-boot ext4",
                "ilex-root crypto_LUKS"
            ]
            .into_iter()
            .collect(),
        "the partitions: {partitions:?}"
    );
    // The record the first boot reads: passphrase only, nothing enrolled.
    let setup: Value = serde_json::from_str(field(text, "setup")?)?;
    ensure!(
        setup
            == json!({
                "schema": 1,
                "composition": "generic.gnome",
                "image": plan.image.target,
                "profile": {"id": null, "schema": null},
                "security": {"capabilities": {}, "tpm": {"requested": "off", "capability": "unknown", "reason": "not-attempted", "status": "passphrase-only"}},
            }),
        "setup.json: {setup}"
    );
    // Nothing is left open or mounted, and nothing holds the passphrase.
    want("mapper_before", "0")?;
    want("mapper_after", "0")?;
    want("mounted_after", "0")?;
    want("log_leak", "0")?;
    want("etc_leak", "0")?;
    ensure!(
        field(text, "logs")?.contains("02-encrypt.log") && field(text, "logs")?.contains("plan.json"),
        "the logs on the target: {}",
        field(text, "logs")?
    );
    Ok(Found { uuid })
}

/// The inspection with the passphrase taken out (it is in no line of it, but
/// what is kept as evidence is checked all the same).
fn redact(text: &str, variant: &Variant) -> String {
    text.replace(variant.passphrase, "<passphrase>")
}

impl Bench {
    /// Boot the installed, encrypted disk and unlock it at its prompt.
    pub(super) fn boot_target_encrypted(&self, variant: &Variant) -> Result<Vm> {
        let mut config = VmConfig::new(format!("inst-{}-t", self.name), &self.target);
        config.memory = "4G".into();
        let mut vm = Vm::boot(&self.ws, &config)?;
        unlock(
            &mut vm,
            variant.keys,
            &self.artifacts,
            &format!("installer-{}", self.name),
        )
        .context("unlocking the installed disk at its prompt")?;
        if let Err(error) = vm.wait_ssh(Duration::from_secs(600)) {
            if let Ok(serial) = fs::read(vm.info.dir.join("serial.log")) {
                self.save("target-serial.txt", &String::from_utf8_lossy(&serial));
            }
            let _ = Vmctl::from_info(vm.info.clone()).save_screenshot(
                &self
                    .artifacts
                    .join(format!("installer-{}-target-failure.png", self.name)),
            );
            return Err(error).context("the unlocked disk did not come up");
        }
        Ok(vm)
    }
}

/// Wait for the prompt of the volume's passphrase and type `keys` into it, as a
/// person at the keyboard does. The prompt is found on the screen (the
/// installed system has no serial console of its own): the picture has stopped
/// changing and is not blank; its first such state is kept as evidence. A
/// passphrase typed before the prompt exists would be typed into the boot
/// loader or lost.
pub(crate) fn unlock(vm: &mut Vm, keys: &str, artifacts: &Path, name: &str) -> Result<()> {
    // (The VM may not be ours to wait on for ever: a prompt that never comes is
    // an error with a screenshot, never a hang.)
    let ctl = Vmctl::from_info(vm.info.clone());
    let serial = vm.info.dir.join("serial.log");
    let started = Instant::now();
    let discover = std::env::var_os("ILEX_ENCRYPTED_DISCOVER").is_some();
    let mut previous = None;
    let mut still = 0u32;
    let mut taken = 0u32;
    loop {
        if let Some(status) = vm.exited()? {
            bail!("the VM exited with {status} before its prompt");
        }
        if let Ok(frame) = ctl.screenshot() {
            let fingerprint = frame.fingerprint();
            if discover {
                taken += 1;
                let _ = ctl.save_screenshot(&artifacts.join(format!("{name}-boot-{taken:03}.png")));
            }
            if previous == Some(fingerprint) {
                still += 1;
            } else {
                still = 0;
            }
            previous = Some(fingerprint);
            let console = fs::read_to_string(&serial)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let asked = console.contains("passphrase") || console.contains("please enter");
            if !discover && (asked || (still >= 3 && started.elapsed() > Duration::from_secs(20))) {
                ctl.save_screenshot(&artifacts.join(format!("{name}-prompt.png")))?;
                break;
            }
        }
        if started.elapsed() >= Duration::from_secs(if discover { 150 } else { 360 }) {
            let _ = ctl.save_screenshot(&artifacts.join(format!("{name}-no-prompt.png")));
            if let Ok(serial) = fs::read(&serial) {
                let _ = fs::write(
                    artifacts.join(format!("{name}-no-prompt-serial.txt")),
                    serial,
                );
            }
            bail!(
                "no passphrase prompt within {:?}{}",
                started.elapsed(),
                if discover { " (discovery run)" } else { "" }
            );
        }
        std::thread::sleep(Duration::from_secs(3));
    }
    ctl.type_text(keys)?;
    ctl.key("Return")?;
    Ok(())
}

/// What the unlocked, booted system says of its encryption. `uuid` is the
/// volume's, when the caller knows it (it has read the disk); otherwise the
/// booted system says it.
pub(crate) fn booted_encrypted(
    vm: &Vm,
    uuid: Option<&str>,
    variant: &Variant,
    save: &dyn Fn(&str, &str),
) -> Result<()> {
    let own = ok(vm, "cryptsetup luksUUID /dev/disk/by-partlabel/ilex-root")?;
    ensure!(
        uuid.is_none_or(|known| known == own),
        "the volume's UUID is {own} when booted, {uuid:?} on the disk"
    );
    let uuid = &own;
    let out = ok(
        vm,
        &format!(
            r#"set -u
echo "cmdline=$(cat /proc/cmdline)"
echo "crypttab=$(cat /etc/crypttab)"
echo "sysroot_source=$(findmnt -no SOURCE /sysroot)"
echo "var_source=$(findmnt -no SOURCE /var | sed 's/\[.*//')"
echo "boot_source=$(findmnt -no SOURCE /boot)"
echo "root_type=$(lsblk -no TYPE /dev/mapper/luks-{uuid} | head -1)"
echo "parent_type=$(lsblk -sno TYPE /dev/mapper/luks-{uuid} | tr '\n' ' ')"
echo "parent_fstype=$(lsblk -sno FSTYPE /dev/mapper/luks-{uuid} | tr '\n' ' ')"
echo "swaps=$(tail -n +2 /proc/swaps | grep -vc zram || true)"
echo "dump=$(cryptsetup luksDump --dump-json-metadata /dev/disk/by-uuid/{uuid} | python3 -c 'import json,sys; h=json.load(sys.stdin); print(len(h["keyslots"]), ",".join(v["kdf"]["type"] for v in h["keyslots"].values()), ",".join(v["encryption"] for v in h["segments"].values()))')"
echo "discard=$(cryptsetup status luks-{uuid} | grep -c 'flags:.*discards' || true)"
echo "vconsole=$(grep -E '^KEYMAP=' /etc/vconsole.conf)"
echo "keymap_loaded=$(localectl status | sed -n 's/^ *VC Keymap: //p')"
echo "failed=$(systemctl --failed --no-legend --plain | wc -l)"
echo "cryptsetup_unit=$(systemctl is-active systemd-cryptsetup@luks-{uuid}.service 2>&1 | head -1)"
echo "unlock_record=$(cat /run/ilex/root-unlock.json 2>/dev/null | tr -d '\n ')"
echo "initrd_vconsole=$(journalctl -b --no-pager -g 'vconsole' 2>/dev/null | grep -ci -e 'failed' || true)"
echo "leak_var_log=$(grep -rlaF -e "$(printf %s {pass} | base64 -d)" /var/log/ilex-installer /etc /var/lib/ilex 2>/dev/null | wc -l)"
echo "leak_journal=$(journalctl -b --no-pager | grep -cF -e "$(printf %s {pass} | base64 -d)" || true)"
"#,
            pass = base64(variant.passphrase.as_bytes())
        ),
    )?;
    save("booted-encryption.txt", &redact(&out, variant));
    println!(
        "{}",
        redact(&out, variant)
            .lines()
            .map(|l| format!("     | {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let want = |key: &str, wanted: &str| -> Result<()> {
        ensure!(
            field(&out, key)? == wanted,
            "booted {key}: {:?}, wanted {wanted:?}\n{out}",
            field(&out, key)?
        );
        Ok(())
    };
    let cmdline = field(&out, "cmdline")?;
    for karg in [
        format!("rd.luks.uuid={uuid}"),
        format!("vconsole.keymap={}", variant.keymap),
        "rd.luks.options=tpm2-device=auto".to_owned(),
    ] {
        ensure!(
            cmdline.split(' ').any(|k| k == karg),
            "the booted kernel command line lacks {karg}: {cmdline}"
        );
    }
    want("crypttab", &format!("luks-{uuid} UUID={uuid} none discard"))?;
    want("sysroot_source", &format!("/dev/mapper/luks-{uuid}"))?;
    want("var_source", &format!("/dev/mapper/luks-{uuid}"))?;
    want("root_type", "crypt")?;
    ensure!(
        field(&out, "parent_type")?.starts_with("crypt part")
            && field(&out, "parent_fstype")?.contains("crypto_LUKS"),
        "what the root is made of: {out}"
    );
    want("swaps", "0")?;
    want("dump", "1 argon2id aes-xts-plain64")?;
    want("vconsole", &format!("KEYMAP={}", variant.keymap))?;
    want("keymap_loaded", variant.keymap)?;
    ensure!(
        field(&out, "boot_source")?.contains("/dev/") && !field(&out, "boot_source")?.contains("mapper"),
        "/boot is on the encrypted volume: {}",
        field(&out, "boot_source")?
    );
    want("leak_var_log", "0")?;
    // The journal of this boot has the passphrase only if something logged it.
    ensure!(
        field(&out, "leak_journal")? == "0",
        "the passphrase is in this boot's journal: {out}"
    );
    // The unlock stack's own evidence: the image's initramfs wrapper met a
    // volume it could classify (a password-only one: fallback, not a TPM
    // unlock) and recorded how this boot opened it.
    let record = field(&out, "unlock_record")?;
    println!("     unlock record: {record:?}");
    Ok(())
}
