//! Installed-disk homed and profile qualification, never the SELinux-disabled
//! virtiofs VM. Encrypted-root qualification belongs to the security tiers.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::qmp::Qmp;
use ilex_vmctl::{Vmctl, WaitCond};
use libtest_mimic::Trial;
use serde_json::{Value, json};

use crate::{common, disk::vmtest};

const USER: &str = "homedtest";
const WAIT: Duration = Duration::from_secs(240);
const GUEST_DRIVER: &str = include_str!("homed-guest.py");
const CRASH_DRIVER: &str = include_str!("homed-crash.py");

pub fn trials() -> Vec<Trial> {
    vec![
        common::trial("homed::lifecycle", lifecycle),
        common::trial("homed::allocation", allocation),
        common::trial("homed::crash_grow", || resize_crash("grow")),
        common::trial("homed::crash_shrink", || resize_crash("shrink")),
        common::trial("homed::profiles", profiles),
    ]
}

fn profiles() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "homed/profiles")?;
    let disk = vmtest::disk(&ws)?;
    let mut config = VmConfig::new("profiles", disk);
    config.memory = "2G".into();
    let mut vm = vmtest::boot(&ws, config)?;
    let result = (|| {
        vm.ssh_ok("test \"$(getenforce)\" = Enforcing && ! ilexctl profile apply andriishafar && ! ilexctl profile prune-backups && ilexctl profile list")?;
        vm.ssh_ok(&format!(
            "printf %s {} | base64 -d > /run/ilex-profiles-user.py",
            qemu::base64(include_str!("profiles-user-guest.py").as_bytes())
        ))?;
        vm.ssh_ok(&format!(
            "printf %s {} | base64 -d > /run/ilex-profiles-races.py",
            qemu::base64(include_str!("profiles-races-guest.py").as_bytes())
        ))?;
        // Read-only, root-owned profile variants model a later signed image.
        // Only the e2e harness redirects the production module's image root;
        // the shipped command accepts no alternate source/home option.
        vm.ssh_ok(r#"python3 - <<'PY'
import hashlib, json, pathlib, shutil
source = pathlib.Path('/usr/share/ilex')
for name, text in [('v2', 'profile revision two\n'), ('rewritten-v2', 'rewritten without revision bump\n')]:
    root = pathlib.Path('/run/ilex-profile-' + name)
    shutil.copytree(source / 'users', root / 'users')
    shutil.copy(source / 'image.json', root / 'image.json')
    leaf = root / 'users/andriishafar/home/.config/ilex/welcome.txt'
    leaf.write_text(text)
    added = leaf.with_name('added.txt')
    added.write_text('new public file\n')
    manifest = root / 'users/andriishafar/user.json'
    data = json.loads(manifest.read_text())
    data['revision'] = 2
    data['settings'] = {'gnome': {'color-scheme': 'default', 'clock-show-weekday': False}}
    data['files'] = {'.config/ilex/' + p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in [leaf, added]}
    data['apps'] = {'flatpaks': [{'id': 'org.example.IlexMissing', 'remote': 'flathub'}]}
    manifest.write_text(json.dumps(data))
    catalogue = {'schema': 1, 'profiles': [{'id': 'andriishafar', 'schema': 1, 'revision': 2, 'sha256': hashlib.sha256(manifest.read_bytes()).hexdigest()}]}
    (root / 'users.json').write_text(json.dumps(catalogue))
PY"#)?;
        let identity: Value =
            serde_json::from_str(&vm.ssh_ok("cat /usr/share/ilex/users/andriishafar/user.json")?)?;
        let owner = identity["username"].as_str().context("profile username")?;
        ensure!(owner == "andriishafar", "unexpected owner profile username");
        let secret = password()?;
        let maintenance = password()?;
        guest(&vm, json!({"action":"maintenance", "secrets":[maintenance]}))?;
        guest(&vm, json!({"action":"collision", "secrets":[secret]}))?;
        guest(
            &vm,
            json!({"action":"create", "name":owner, "secrets":[secret]}),
        )?;
        guest(
            &vm,
            json!({"action":"profiles", "name":owner, "secrets":[secret]}),
        )?;
        vm.wait_for(
            "profile user's home is released",
            &format!("! mountpoint -q /home/{owner}"),
            WAIT,
        )?;
        guest(
            &vm,
            json!({"action":"profile-relogin", "name":owner, "secrets":[secret]}),
        )?;
        guest(&vm, json!({"action":"maintenance-recovery", "secrets":[maintenance]}))?;
        guest(&vm, json!({"action":"secret-audit", "secrets":[secret, maintenance]}))?;
        Ok(())
    })();
    if let Ok(journal) = vm.ssh_ok("journalctl --no-pager -u systemd-homed -u 'user@*' -n 200") {
        qemu::write_artifact(&artifacts, "journal.txt", &journal)?;
    }
    finish(result, audit(&vm, &artifacts))
}

fn password() -> Result<String> {
    // Strong disposable fixture credential, never a repo constant/artifact.
    let mut bytes = [0u8; 24];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!(
        "Ix9-{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

/// Unlike Vm::ssh_ok, neither the command nor its errors can contain the
/// credential: JSON travels only through an anonymous stdin pipe to Python.
pub(crate) fn guest(vm: &Vm, request: Value) -> Result<Value> {
    // /run is cleared by the lifecycle's reboots. Reinstall this nonsecret
    // driver on each call; credentials still travel only through stdin.
    for (path, driver) in [
        ("/run/ilex-homed-e2e.py", GUEST_DRIVER),
        ("/run/ilex_homed_crash.py", CRASH_DRIVER),
    ] {
        vm.ssh_ok(&format!(
            "umask 077; printf %s {} | base64 -d > {path}",
            qemu::base64(driver.as_bytes())
        ))?;
    }
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "StrictHostKeyChecking=no",
            "-o",
            "UserKnownHostsFile=/dev/null",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "IdentitiesOnly=yes",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=10",
        ])
        .arg("-i")
        .arg(&vm.info.ssh_key)
        .args([
            "-p",
            &vm.info.ssh_port.to_string(),
            "root@127.0.0.1",
            "--",
            "timeout 600 python3 /run/ilex-homed-e2e.py",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting homed guest driver")?;
    child
        .stdin
        .take()
        .context("guest stdin")?
        .write_all(&serde_json::to_vec(&request)?)?;
    let out = child.wait_with_output()?;
    ensure!(
        out.status.success(),
        "homed guest {} failed: {}",
        request["action"],
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).context("reading homed guest result")?;
    println!("     {}: {value}", request["action"]);
    Ok(value)
}

fn prepare(vm: &Vm) -> Result<()> {
    vm.ssh_ok(
        r#"set -eu
test "$(getenforce)" = Enforcing
test -e /run/ostree-booted
timeout 300 systemctl is-system-running --wait
test -f /usr/share/selinux/packages/ilex-homed.cil
semodule -l | grep -x ilex-homed
authselect check
authselect current | grep -F with-systemd-homed
systemctl is-enabled systemd-homed.service
systemctl is-active systemd-homed.service
for file in system-auth password-auth; do
    for kind in auth account password session; do
        grep -E "^-?$kind .*pam_systemd_home.so" /etc/pam.d/$file
    done
done
! grep -E 'pam_systemd_home.so.*suspend=(1|yes|on|true)' /etc/pam.d/*
grep -E '^passwd:.*systemd' /etc/nsswitch.conf
grep -E '^group:.*systemd' /etc/nsswitch.conf
getent group ilex-homed
rpm -q systemd systemd-udev systemd-pam btrfs-progs selinux-policy-targeted
if command -v gdm >/dev/null; then rpm -q gdm accountsservice; fi
test -L /home
test "$(readlink -f /home)" = /var/home
"#,
    )?;
    Ok(())
}

fn inactive(vm: &mut Vm, name: &str) -> Result<()> {
    vm.wait_for("home becomes inactive after its final reference", &format!(
        r#"homectl inspect {name} --json=short | python3 -c 'import json,sys; r=json.load(sys.stdin); assert any(v.get("state") == "inactive" for v in r.get("status", {{}}).values())'"#), WAIT)?;
    vm.ssh_ok(&format!(
        "! mountpoint -q /home/{name} && test ! -e /home/{name}/e2e-data && getent passwd {name}"
    ))?;
    Ok(())
}

fn audit(vm: &Vm, artifacts: &Path) -> Result<()> {
    audit_with_probe(vm, artifacts, None)
}

fn audit_with_probe(vm: &Vm, artifacts: &Path, probe: Option<(u64, u64)>) -> Result<()> {
    // Audit records are not necessarily in journald when auditd is present.
    let records = vm.ssh_ok("journalctl --no-pager -g 'avc:.*denied' 2>/dev/null; test ! -f /var/log/audit/audit.log || grep -E 'type=(AVC|USER_AVC).*denied' /var/log/audit/audit.log; true")?;
    qemu::write_artifact(artifacts, "avc.txt", &records)?;
    // v262 detect_container() probes /proc/TracerPid/comm to detect proot.
    // That optional probe cannot search the fixture tracer's unconfined proc
    // directory. Annotate only this exact worker/tracer pair; retain every
    // record and do not grant a runtime permission for the test harness.
    // https://github.com/systemd/systemd/blob/v262/src/basic/virt.c
    let tracer_probe = |line: &str| {
        probe.is_some_and(|(tracer, worker)| {
            line.contains(&format!("pid={worker} "))
                && line.contains(&format!("name=\"{tracer}\" "))
                && line.contains("dev=\"proc\"")
                && line.contains("scontext=system_u:system_r:systemd_homework_t:")
                && line.contains(":unconfined_r:unconfined_t:")
                && line.contains("tclass=dir")
                && line.contains("{ search }")
        })
    };
    if let Some((tracer, worker)) = probe {
        let annotated: Vec<_> = records.lines().filter(|line| tracer_probe(line)).collect();
        qemu::write_artifact(
            artifacts,
            "tracer-probe-avc.json",
            &serde_json::to_string_pretty(&json!({
                "tracer_pid":tracer, "worker_pid":worker,
                "cause":"v262 detect_container optional proot probe while ptraced",
                "records":annotated,
            }))?,
        )?;
        println!(
            "     annotated {} optional tracer-probe AVC(s); full audit retained",
            annotated.len()
        );
    }
    // Keep unrelated desktop denials visible, fail on any home/auth boundary.
    let relevant: Vec<_> = records
        .lines()
        .filter(|l| {
            if tracer_probe(l) {
                return false;
            }
            [
                "homed",
                "homework",
                "home_root_t",
                "user_home",
                "unlabeled_t",
                "login_exec_t",
                "local_login_t",
                "gdm",
                "sshd",
                "sudo",
                "comm=\"login\"",
                "comm=\"passwd\"",
            ]
            .iter()
            .any(|s| l.contains(s))
        })
        .collect();
    ensure!(
        relevant.is_empty(),
        "homed/authentication AVCs:\n{}",
        relevant.join("\n")
    );
    vm.ssh_ok("test \"$(getenforce)\" = Enforcing")?;
    Ok(())
}

fn evidence(vm: &Vm, artifacts: &Path) -> Result<()> {
    let text = vm.ssh_ok("rpm -q systemd systemd-udev systemd-pam selinux-policy-targeted gdm accountsservice; getenforce; findmnt /var/home; ls -Zd /var/home /var/home/*.home; lsattr /var/home/*.home; stat -c '%n apparent=%s blocks=%b block-unit=%B' /var/home/*.home; systemctl --failed --no-legend; bootc status --json")?;
    qemu::write_artifact(artifacts, "runtime.txt", &text)?;
    Ok(())
}

fn lifecycle() -> Result<()> {
    ensure!(
        common::selection()?.default_shell.as_deref() == Some("gnome"),
        "homed::lifecycle requires GNOME/GDM"
    );
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "homed/lifecycle")?;
    let disk = vmtest::disk(&ws)?;
    let mut config = VmConfig::graphical("homed", disk);
    // Keep software-rendered login qualification affordable under parallel load.
    config.width = 1280;
    config.height = 800;
    let mut vm = vmtest::boot(&ws, config)?;
    let ctl = Vmctl::from_info(vm.info.clone());
    let result = (|| {
        prepare(&vm)?;
        let (old, new, admin) = (password()?, password()?, password()?);
        guest(&vm, json!({"action":"maintenance", "secrets":[admin]}))?;
        guest(
            &vm,
            json!({"action":"create", "admin":true, "secrets":[old]}),
        )?;
        // Homed refuses the existing traditional maintenance user; no import.
        guest(&vm, json!({"action":"collision", "secrets":[old]}))?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"wrong-password", "secrets":[new]}))?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"activate", "secrets":[old]}))?;
        vm.ssh_ok("homectl deactivate homedtest")?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"tty", "secrets":[old]}))?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"password", "secrets":[old,new]}))?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"wrong-password", "secrets":[old]}))?;
        guest(&vm, json!({"action":"tty", "secrets":[new]}))?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"activate", "secrets":[new]}))?;
        ssh_boundaries(&vm)?;
        vm.ssh_ok("homectl deactivate homedtest")?;
        inactive(&mut vm, USER)?;

        // GDM starts with both a traditional administrator and an inactive
        // homed identity. Assert AccountsService discovery without activation.
        vm.reboot(vmtest::BOOT_TIMEOUT)?;
        wait_gdm(&mut vm)?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"tty", "secrets":[new]}))?;
        inactive(&mut vm, USER)?;
        discover(&mut vm)?;
        shot(&ctl, &artifacts, "01-gdm-inactive")?;
        // This fresh fixture has two visible identities. From the selected
        // first tile, pass the second tile to reach GDM's Not listed route.
        ctl.key("Escape")?;
        ctl.key("Tab")?;
        ctl.key("Tab")?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(2));
        ctl.type_text(USER)?;
        ctl.key("Return")?;
        std::thread::sleep(Duration::from_secs(3));
        ctl.type_text(&new)?;
        ctl.key("Return")?;
        vm.wait_for(
            "homed user's graphical session",
            "pgrep -u homedtest -x gnome-shell",
            WAIT,
        )?;
        vm.ssh_ok("test \"$(findmnt -nro FSTYPE --target /home/homedtest)\" = btrfs; id homedtest | grep wheel; test \"$(loginctl show-user homedtest -p Linger --value)\" = no")?;
        shot(&ctl, &artifacts, "02-gdm-homed-session")?;
        ssh_boundaries(&vm)?;
        guest(&vm, json!({"action":"tty", "secrets":[new]}))?;
        vm.ssh_ok("mountpoint -q /home/homedtest; pgrep -u homedtest -x gnome-shell")?;
        // End the desktop through its own session bus. No forced home
        // deactivation: PAM/logind must release the last reference itself.
        vm.ssh_ok("uid=$(id -u homedtest); runuser -u homedtest -- env XDG_RUNTIME_DIR=/run/user/$uid DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus gnome-session-quit --logout --no-prompt")?;
        inactive(&mut vm, USER)?;
        ssh_inactive(&mut vm, &artifacts)?;
        guest(
            &vm,
            json!({"action":"maintenance-recovery", "secrets":[admin]}),
        )?;
        guest(
            &vm,
            json!({"action":"secret-audit", "secrets":[old,new,admin]}),
        )?;
        vm.ssh_ok("getent -s files passwd ilexmaint; test ! -e /var/home/ilexmaint.home; test ! -e /var/lib/systemd/linger/homedtest")?;
        evidence(&vm, &artifacts)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = ctl.save_screenshot(&artifacts.join("failure.png"));
        let _ = evidence(&vm, &artifacts);
        let journal = vm.ssh_ok("journalctl --no-pager -u systemd-homed -u 'ilex-homed-label@*' -u 'user@*' -u gdm -u sshd -n 500");
        if let Ok(journal) = journal {
            let _ = qemu::write_artifact(&artifacts, "failure-journal.txt", &journal);
        }
    }
    let avc = audit(&vm, &artifacts);
    finish(result, avc)
}

fn finish(result: Result<()>, avc: Result<()>) -> Result<()> {
    match (result, avc) {
        (Err(error), Err(audit)) => Err(error.context(format!("AVC audit also failed: {audit:#}"))),
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(error),
    }
}

fn wait_gdm(vm: &mut Vm) -> Result<()> {
    vm.wait_for(
        "GDM greeter",
        "systemctl is-active gdm && loginctl list-sessions --no-legend | grep -w greeter",
        WAIT,
    )?;
    Ok(())
}

fn discover(vm: &mut Vm) -> Result<()> {
    let uid = vm.ssh_ok("id -u homedtest")?;
    let path = format!("/org/freedesktop/Accounts/User{uid}");
    vm.wait_for("AccountsService enumerates the inactive homed identity", &format!(
        "busctl call org.freedesktop.Accounts /org/freedesktop/Accounts org.freedesktop.Accounts ListCachedUsers | grep -F {}", common::quote(&path)), WAIT)?;
    let listed = vm.ssh_ok("busctl call org.freedesktop.Accounts /org/freedesktop/Accounts org.freedesktop.Accounts ListCachedUsers")?;
    ensure!(
        listed.contains(&path),
        "AccountsService did not enumerate inactive {USER}: {listed}"
    );
    let name = vm.ssh_ok(&format!(
        "busctl get-property org.freedesktop.Accounts {path} org.freedesktop.Accounts.User UserName"
    ))?;
    ensure!(
        name.contains(USER),
        "AccountsService returned wrong identity: {name}"
    );
    Ok(())
}

fn shot(ctl: &Vmctl, artifacts: &Path, name: &str) -> Result<()> {
    ctl.wait_screen(
        &WaitCond {
            nonblank: true,
            stable: Some(Duration::from_secs(3)),
            ..WaitCond::default()
        },
        WAIT,
    )?;
    ctl.save_screenshot(&artifacts.join(format!("{name}.png")))?;
    Ok(())
}

fn ssh_boundaries(vm: &Vm) -> Result<()> {
    let key = std::fs::read_to_string(vm.info.ssh_key.with_extension("pub"))?;
    // Valid keys in both homes prove an active home's key is still refused.
    vm.ssh_ok(&format!(
        r#"set -eu
for user in ilexmaint homedtest; do
    install -d -m 700 -o "$user" -g "$user" /home/$user/.ssh
    printf '%s\n' {} > /home/$user/.ssh/authorized_keys
    chown "$user:$user" /home/$user/.ssh/authorized_keys
    chmod 600 /home/$user/.ssh/authorized_keys
    restorecon -RF /home/$user/.ssh
done
"#,
        common::quote(key.trim())
    ))?;
    ssh_attempts(vm)
}

fn ssh_attempts(vm: &Vm) -> Result<()> {
    for (user, allowed) in [("ilexmaint", true), (USER, false)] {
        let output = Command::new("ssh")
            .env("LC_ALL", "C")
            .args([
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "ConnectTimeout=10",
            ])
            .arg("-i")
            .arg(&vm.info.ssh_key)
            .args([
                "-p",
                &vm.info.ssh_port.to_string(),
                &format!("{user}@127.0.0.1"),
                "id -un",
            ])
            .output()?;
        ensure!(
            output.status.success() == allowed,
            "SSH boundary for {user}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if allowed {
            ensure!(
                String::from_utf8_lossy(&output.stdout).trim() == user,
                "SSH returned the wrong identity"
            );
        } else {
            ensure!(
                String::from_utf8_lossy(&output.stderr).contains("Permission denied"),
                "SSH failed before authentication: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

fn ssh_inactive(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    inactive(vm, USER)?;
    let key = std::fs::read_to_string(vm.info.ssh_key.with_extension("pub"))?;
    vm.ssh_ok("umask 077; cp /etc/ssh/sshd_config /run/ilex-e2e-sshd_config")?;
    let result = (|| {
        // A valid, world-readable public key outside either home. Match is
        // appended after all global configuration; root's SSH is unchanged.
        vm.ssh_ok(&format!(r#"set -eu
install -d -m 755 /etc/ssh/ilex-homed-e2e
for user in ilexmaint homedtest; do
    printf '%s\n' {} > /etc/ssh/ilex-homed-e2e/$user
    chmod 644 /etc/ssh/ilex-homed-e2e/$user
done
restorecon -RF /etc/ssh/ilex-homed-e2e
printf '\nMatch User ilexmaint,homedtest\n    AuthorizedKeysFile /etc/ssh/ilex-homed-e2e/%%u\n' >> /etc/ssh/sshd_config
sshd -t
for user in ilexmaint homedtest; do
    sshd -T -C user=$user,host=localhost,addr=127.0.0.1 | grep -iFx 'authorizedkeysfile /etc/ssh/ilex-homed-e2e/%u'
done
systemctl reload sshd
"#, common::quote(key.trim())))?;
        let cursor = vm.ssh_ok("journalctl -n 1 -o json --no-pager | python3 -c 'import json,sys; print(json.load(sys.stdin)[\"__CURSOR\"])'")?;
        ssh_attempts(vm)?;
        inactive(vm, USER)?;
        let logs = vm.ssh_ok(&format!(
            "journalctl --no-pager -o cat -u sshd -u systemd-homed --after-cursor={}",
            common::quote(&cursor)
        ))?;
        qemu::write_artifact(artifacts, "ssh-inactive.txt", &logs)?;
        ensure!(
            logs.lines()
                .any(|l| l.contains(USER) && l.contains("DenyGroups")),
            "locked-home SSH was not refused by DenyGroups: {logs}"
        );
        ensure!(
            !logs
                .lines()
                .any(|l| l.contains(USER) && l.contains("inactive → activating")),
            "SSH attempted to activate the locked home: {logs}"
        );
        println!(
            "     locked-home SSH: external valid key refused by DenyGroups; maintenance accepted; home stayed inactive"
        );
        Ok(())
    })();
    let cleanup = vm.ssh_ok("set -eu; cat /run/ilex-e2e-sshd_config > /etc/ssh/sshd_config; sshd -t; systemctl reload sshd; rm /run/ilex-e2e-sshd_config").map(|_| ());
    cleanup.context("restoring normal sshd configuration after the locked-home check")?;
    result
}

fn allocation() -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, "homed/allocation")?;
    let disk = vmtest::disk(&ws)?;
    let mut vm = vmtest::boot(&ws, VmConfig::new("homed-allocation", disk))?;
    let result = (|| {
        prepare(&vm)?;
        // Dedicated loop-backed outer btrfs: ENOSPC cannot consume root or
        // the host. Preserve the same Fedora /var/home SELinux path/labels.
        vm.ssh_ok("set -eu; systemctl stop gdm 2>/dev/null || test ! -e /usr/lib/systemd/system/gdm.service; truncate -s 7G /var/tmp/homed-outer.raw; mkfs.btrfs -f /var/tmp/homed-outer.raw; mount -o loop /var/tmp/homed-outer.raw /var/home; restorecon /var/home")?;
        let (secret, admin) = (password()?, password()?);
        guest(&vm, json!({"action":"maintenance", "secrets":[admin]}))?;
        guest(
            &vm,
            json!({"action":"create", "admin":true, "secrets":[secret]}),
        )?;
        guest(&vm, json!({"action":"activate", "secrets":[secret]}))?;
        let resize = guest(&vm, json!({"action":"resize", "secrets":[secret]}))?;
        qemu::write_artifact(
            &artifacts,
            "resize.json",
            &serde_json::to_string_pretty(&resize)?,
        )?;
        let multi = guest(&vm, json!({"action":"multi-home", "secrets":[secret]}))?;
        qemu::write_artifact(
            &artifacts,
            "multi-home.json",
            &serde_json::to_string_pretty(&multi)?,
        )?;
        let full = guest(&vm, json!({"action":"enospc", "secrets":[secret,admin]}))?;
        qemu::write_artifact(
            &artifacts,
            "enospc.json",
            &serde_json::to_string_pretty(&full)?,
        )?;
        vm.ssh_ok("homectl deactivate homedother homedtest")?;
        inactive(&mut vm, USER)?;
        let reopened = guest(&vm, json!({"action":"activate", "secrets":[secret]}))?;
        ensure!(
            reopened["sha256"] == resize["sha256"],
            "home data changed after logout/relogin"
        );
        vm.ssh_ok("sha256sum /home/homedtest/e2e-data; homectl deactivate homedtest")?;
        inactive(&mut vm, USER)?;
        evidence(&vm, &artifacts)
    })();
    let avc = audit(&vm, &artifacts);
    finish(result, avc)
}

fn dimensions(value: &Value) -> Result<[u64; 3]> {
    let read = |i| {
        value["sizes"][i]
            .as_u64()
            .context("missing filesystem/mapper/backing size")
    };
    Ok([read(0)?, read(1)?, read(2)?])
}

fn resize_crash(direction: &str) -> Result<()> {
    let ws = common::workspace()?;
    let artifacts = qemu::artifact_dir(&ws, &format!("homed/crash-{direction}"))?;
    for name in ["failure.txt", "normalized.json", "tracer-probe-avc.json"] {
        match std::fs::remove_file(artifacts.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("clearing previous crash-trial evidence"),
        }
    }
    let disk = vmtest::disk(&ws)?;
    let mut config = VmConfig::new(format!("homed-crash-{direction}"), disk);
    config.memory = "2G".into();
    config.vcpus = 2;
    let mut vm = vmtest::boot(&ws, config)?;
    let (secret, admin) = (password()?, password()?);
    let mut maintenance_created = false;
    let mut trace_probe = None;
    let result = (|| {
        // Only this disposable overlay: keep the reset boot headless too.
        vm.ssh_ok("systemctl set-default multi-user.target; systemctl stop gdm")?;
        prepare(&vm)?;
        guest(&vm, json!({"action":"maintenance", "secrets":[admin]}))?;
        maintenance_created = true;
        guest(
            &vm,
            json!({"action":"create", "admin":true, "secrets":[secret]}),
        )?;
        let original = guest(&vm, json!({"action":"activate", "secrets":[secret]}))?;
        vm.ssh_ok("homectl deactivate homedtest")?;
        inactive(&mut vm, USER)?;
        guest(&vm, json!({"action":"tty", "secrets":[secret]}))?;
        inactive(&mut vm, USER)?;
        let mut before = guest(&vm, json!({"action":"activate", "secrets":[secret]}))?;
        if direction == "shrink" {
            before = guest(
                &vm,
                json!({"action":"resize-to", "size":"3G", "secrets":[secret]}),
            )?;
        }
        ensure!(
            before["sha256"] == original["sha256"],
            "fixture checksum changed before interruption"
        );
        let before_sizes = dimensions(&before)?;
        let boot_id = vm.ssh_ok("sync && cat /proc/sys/kernel/random/boot_id")?;
        qemu::write_artifact(
            &artifacts,
            "before.json",
            &serde_json::to_string_pretty(&before)?,
        )?;
        let tracer = guest(
            &vm,
            json!({"action":"crash-start", "direction":direction, "secrets":[secret]}),
        )?;
        let gate = guest(&vm, json!({"action":"crash-wait"}))?;
        trace_probe = Some((
            tracer["tracer_pid"]
                .as_u64()
                .context("missing tracer PID")?,
            gate["worker_pid"].as_u64().context("missing worker PID")?,
        ));
        qemu::write_artifact(
            &artifacts,
            "gate.json",
            &serde_json::to_string_pretty(&gate)?,
        )?;
        let gated = dimensions(&gate)?;
        ensure!(
            gate["worker_state"]
                .as_str()
                .is_some_and(|s| s.contains("tracing stop")),
            "resize worker was not stopped"
        );
        ensure!(
            gate["resize_client_completed"] == false,
            "resize completed before the crash"
        );
        if direction == "grow" {
            ensure!(
                gated[0] == before_sizes[0]
                    && gated[1] > before_sizes[1]
                    && gated[2] > before_sizes[2],
                "growth gate was not between lower-layer growth and filesystem growth: {before_sizes:?} -> {gated:?}"
            );
        } else {
            ensure!(
                gated[0] < before_sizes[0]
                    && gated[1] == before_sizes[1]
                    && gated[2] == before_sizes[2],
                "shrink gate was not between filesystem shrink and lower-layer shrink: {before_sizes:?} -> {gated:?}"
            );
        }
        // A hardware reset, without guest shutdown, sync or a new overlay.
        {
            // Close the QMP connection before waiting for the new boot.
            let mut qmp = Qmp::connect(&vm.info.qmp_sock)?;
            qmp.execute("stop", json!({}))?;
            ensure!(
                qmp.execute("query-status", json!({}))?["running"] == false,
                "QEMU did not pause"
            );
            qmp.execute("system_reset", json!({}))?;
            qmp.execute("cont", json!({}))?;
        }
        vm.wait_for(
            "new boot after interrupted homed resize",
            &format!(
                "test \"$(cat /proc/sys/kernel/random/boot_id)\" != {}",
                common::quote(&boot_id)
            ),
            vmtest::BOOT_TIMEOUT,
        )?;
        let after_boot = vm.ssh_ok("cat /proc/sys/kernel/random/boot_id")?;
        qemu::write_artifact(
            &artifacts,
            "reset.json",
            &serde_json::to_string_pretty(
                &json!({"method":"QMP stop/system_reset/cont", "before_boot_id":boot_id, "after_boot_id":after_boot}),
            )?,
        )?;
        prepare(&vm)?;
        guest(
            &vm,
            json!({"action":"maintenance-recovery", "secrets":[admin]}),
        )?;
        let reopened = guest(&vm, json!({"action":"activate", "secrets":[secret]}))?;
        qemu::write_artifact(
            &artifacts,
            "reopened.json",
            &serde_json::to_string_pretty(&reopened)?,
        )?;
        ensure!(
            reopened["sha256"] == original["sha256"],
            "interrupted {direction} changed pre-existing home data"
        );
        let target = if direction == "grow" { "3G" } else { "2G" };
        let target_bytes = if direction == "grow" { 3u64 } else { 2u64 } * 1024 * 1024 * 1024;
        let reopened_sizes = dimensions(&reopened)?;
        ensure!(
            (before_sizes[2].min(target_bytes)..=before_sizes[2].max(target_bytes))
                .contains(&reopened_sizes[2]),
            "recovered allocation outside the old/requested bounds"
        );
        // A same-backing-size request can be a no-op after interrupted growth,
        // leaving a safe smaller filesystem. Normalize through the smaller
        // supported size first, then retry the requested growth; never truncate
        // the backing file or resize dm-crypt/partitions by hand.
        if direction == "grow" && reopened_sizes[0] < reopened_sizes[1] {
            let normalized = guest(
                &vm,
                json!({"action":"resize-to", "size":"2G", "secrets":[secret]}),
            )?;
            ensure!(
                normalized["sha256"] == original["sha256"],
                "normalizing partial growth changed data"
            );
            qemu::write_artifact(
                &artifacts,
                "normalized.json",
                &serde_json::to_string_pretty(&normalized)?,
            )?;
        }
        let retried = guest(
            &vm,
            json!({"action":"resize-to", "size":target, "secrets":[secret]}),
        )?;
        qemu::write_artifact(
            &artifacts,
            "retried.json",
            &serde_json::to_string_pretty(&retried)?,
        )?;
        let final_sizes = dimensions(&retried)?;
        ensure!(
            retried["sha256"] == original["sha256"]
                && final_sizes[0] == final_sizes[1]
                && final_sizes[2] == target_bytes,
            "resize retry did not restore consistent allocation/data"
        );
        vm.ssh_ok("homectl deactivate homedtest")?;
        inactive(&mut vm, USER)?;
        let relogged = guest(&vm, json!({"action":"activate", "secrets":[secret]}))?;
        ensure!(
            relogged == retried,
            "repaired allocation/data did not survive deactivate/reactivate"
        );
        vm.ssh_ok("homectl deactivate homedtest")?;
        inactive(&mut vm, USER)?;
        guest(
            &vm,
            json!({"action":"maintenance-recovery", "secrets":[admin]}),
        )?;
        guest(
            &vm,
            json!({"action":"secret-audit", "secrets":[secret,admin]}),
        )?;
        evidence(&vm, &artifacts)
    })();
    if result.is_err() {
        let _ = evidence(&vm, &artifacts);
        let maintenance = if maintenance_created {
            guest(
                &vm,
                json!({"action":"maintenance-recovery", "secrets":[admin]}),
            )
            .map(|_| ())
        } else {
            Ok(())
        };
        let _ = qemu::write_artifact(
            &artifacts,
            "failure.txt",
            &format!("{result:?}\nmaintenance recovery: {maintenance:?}"),
        );
    }
    let avc = audit_with_probe(&vm, &artifacts, trace_probe);
    finish(result, avc)
}
