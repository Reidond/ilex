use super::*;

pub(super) fn secret() -> Result<Secret> {
    let mut bytes = [0u8; 48];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    for byte in &mut bytes {
        *byte = b'a' + *byte % 26;
    }
    let secret = Secret::read(ilex_enroll::secret_fd(&bytes)?)?;
    bytes.fill(0);
    Ok(secret)
}

pub(super) fn run(
    fixture: &mut Fixture,
    artifacts: &Path,
    disk: &Secret,
    daily: &Secret,
    maintenance: &Secret,
    credentials: &Value,
    expected: &Value,
) -> Result<()> {
    fixture.disk_boot()?;
    fixture.private_console()?;
    let vm = fixture.vm.running_mut()?;
    let start = Instant::now();
    loop {
        let serial = fs::read_to_string(vm.info.dir.join("serial.log"))
            .unwrap_or_default()
            .to_ascii_lowercase();
        if serial.contains("please enter")
            && (serial.contains("passphrase") || serial.contains("password"))
        {
            break;
        }
        ensure!(
            vm.exited()?.is_none() && start.elapsed() < Duration::from_secs(900),
            "installed system did not request its disk passphrase"
        );
        thread::sleep(Duration::from_secs(1));
    }
    let ctl = Vmctl::from_info(vm.info.clone());
    ctl.save_screenshot(&artifacts.join("disk-passphrase.png"))?;
    fixture
        .console
        .as_mut()
        .context("private console missing")?
        .credential(disk)?;
    vm.wait_ssh(Duration::from_secs(900))?;
    let setup = guest_json(
        vm,
        artifacts,
        "installed-setup.json",
        "cat /etc/ilex/setup.json",
    )?;
    ensure!(
        setup["composition"] == expected["composition"]
            && setup["profile"]["id"] == "andriishafar"
            && setup["profile"]["schema"] == 1
            && setup["security"]["tpm"]["status"] == "passphrase-only",
        "selected profile and passphrase-only mode did not survive installation"
    );
    let bootc = guest_json(vm, artifacts, "profile-bootc.json", "bootc status --json")?;
    ensure!(
        bootc["status"]["booted"]["image"]["imageDigest"] == expected["digest"]
            && bootc["spec"]["image"]["signature"] == "containerPolicy",
        "incorrect booted candidate or signature policy"
    );
    vm.ssh_ok(r#"set -eu
test "$(getenforce)" = Enforcing
test ! -e /dev/tpmrm0
test -n "$(ilex-enroll layout --root / | python3 -c 'import json,sys; print(json.load(sys.stdin)["uuid"])')"
"#)?;
    daily_home(vm, artifacts, daily, maintenance)?;
    guest_privacy(vm, artifacts, "profile-setup", credentials)?;
    poweroff(fixture, artifacts, "profile")
}

fn daily_home(vm: &qemu::Vm, artifacts: &Path, daily: &Secret, maintenance: &Secret) -> Result<()> {
    let ctl = Vmctl::from_info(vm.info.clone());
    vm.ssh_ok(
        r#"set -eu
test -r /etc/issue.d/90-ilex-home-setup.issue
test "$(getent -s files passwd ilexmaint | cut -d: -f1)" = ilexmaint
id -nG ilexmaint | grep -w wheel
test ! -e /var/home/andriishafar.home
if getent passwd andriishafar; then exit 1; fi
# Keep the management forward on the connected subnet; remove Internet paths.
ip -4 route flush default
ip -6 route flush default
if ip route get 1.1.1.1; then exit 1; fi
systemctl stop systemd-homed
if result=$(ilexctl home setup 2>&1); then exit 1; fi
printf '%s\n' "$result" | grep -F 'systemd-homed is unavailable'
test ! -e /var/lib/ilex/home-setup/state.json
systemctl start systemd-homed
useradd --no-create-home andriishafar
if result=$(ilexctl home setup 2>&1); then exit 1; fi
printf '%s\n' "$result" | grep -F 'already a local account'
test ! -e /var/lib/ilex/home-setup/state.json
test ! -e /var/home/andriishafar.home
userdel andriishafar
"#,
    )?;
    let daily = std::str::from_utf8(daily.bytes())?;
    let maintenance = std::str::from_utf8(maintenance.bytes())?;
    crate::homed::guest(
        vm,
        json!({"action":"installer-profile-create", "secrets":[maintenance, daily]}),
    )?;
    vm.ssh_ok(r#"set -eu
test ! -e /etc/issue.d/90-ilex-home-setup.issue
test ! -e /var/lib/ilex/home-setup/identity.json
test -f /var/home/andriishafar.home
id -nG andriishafar | grep -w ilex-homed
if id -nG andriishafar | grep -w wheel; then exit 1; fi
if mountpoint -q /home/andriishafar; then exit 1; fi
# Lose the completion write and prove that setup reconciles the original UUID.
state=/var/lib/ilex/home-setup/state.json
cp "$state" /run/ilex-complete-state.json
python3 -c 'import json,sys; p=sys.argv[1]; s=json.load(open(p)); s["uid"]=None; open(p,"w").write(json.dumps(s))' "$state"
ilexctl home setup </dev/null | grep -F 'is ready'
cmp "$state" /run/ilex-complete-state.json
chvt 8
TERM=linux setterm --blank poke < /dev/tty8 > /dev/tty8
printf '\033[2J\033[3J\033[H\033[?25l' > /dev/tty8
(
    set -x
    ilexctl home setup </dev/null
) > /dev/tty8 2>&1
grep -aFq 'Daily home andriishafar is ready' /dev/vcs8
"#)?;
    let (setup_screen, _) = ctl
        .wait_screen(&ilex_vmctl::WaitCond::nonblank(), Duration::from_secs(30))
        .context("waiting for the daily home setup console to render")?;
    fs::write(artifacts.join("daily-home-setup.png"), setup_screen)?;
    crate::homed::guest(
        vm,
        json!({"action":"installer-profile-login", "name":"andriishafar", "secrets":[daily]}),
    )?;
    vm.ssh_ok(
        r#"timeout 180 bash -c '
while :; do
    for session in $(loginctl list-sessions --no-legend | awk "{print \$1}"); do
        test "$(loginctl show-session "$session" -p Class --value)" = greeter || continue
        vt=$(loginctl show-session "$session" -p VTNr --value)
        if [[ "$vt" =~ ^[1-9][0-9]*$ ]] && chvt "$vt"; then exit 0; fi
    done
    sleep 1
done
'"#,
    )
    .context("switching to the GDM greeter")?;
    thread::sleep(Duration::from_secs(3));
    ctl.save_screenshot(&artifacts.join("gdm.png"))?;
    let proof = json!({"offline":true, "homed_unavailable_retry":true, "local_username_refused":true,
        "reconciled_original_home":true, "authenticated_profile_application":true});
    fs::write(
        artifacts.join("profile-proof.json"),
        serde_json::to_vec_pretty(&proof)?,
    )?;
    Ok(())
}
