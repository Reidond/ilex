//! Installed-disk homed qualification, never the SELinux-disabled virtiofs VM.
//! Profiles and encrypted-root qualification belong to subsequent tiers.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::{Vmctl, WaitCond};
use libtest_mimic::Trial;
use serde_json::{Value, json};

use crate::{common, disk::vmtest};

const USER: &str = "homedtest";
const WAIT: Duration = Duration::from_secs(240);
const GUEST_DRIVER: &str = include_str!("homed-guest.py");

pub fn trials() -> Vec<Trial> {
    vec![
        common::trial("homed::lifecycle", lifecycle),
        common::trial("homed::allocation", allocation),
    ]
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
fn guest(vm: &Vm, request: Value) -> Result<Value> {
    // /run is cleared by the lifecycle's reboots. Reinstall this nonsecret
    // driver on each call; credentials still travel only through stdin.
    vm.ssh_ok(&format!(
        "umask 077; printf %s {} | base64 -d > /run/ilex-homed-e2e.py",
        qemu::base64(GUEST_DRIVER.as_bytes())
    ))?;
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
    // Audit records are not necessarily in journald when auditd is present.
    let records = vm.ssh_ok("journalctl --no-pager -g 'avc:.*denied' 2>/dev/null; test ! -f /var/log/audit/audit.log || grep -E 'type=(AVC|USER_AVC).*denied' /var/log/audit/audit.log; true")?;
    qemu::write_artifact(artifacts, "avc.txt", &records)?;
    // Keep unrelated desktop denials visible, fail on any home/auth boundary.
    let relevant: Vec<_> = records
        .lines()
        .filter(|l| {
            [
                "homed",
                "homework",
                "home_root_t",
                "user_home",
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
        // GDM's not-listed route avoids depending on tile ordering.
        // Escape then Tab navigates from the selected user list to Not listed.
        ctl.key("Escape")?;
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
    for (user, allowed) in [("ilexmaint", true), (USER, false)] {
        let output = Command::new("ssh")
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
    }
    Ok(())
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
