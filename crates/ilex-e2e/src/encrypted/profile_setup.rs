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
cp "$state" /run/ilex-pending-state.json
printf '%s\n' 'Run sudo ilexctl home setup.' > /etc/issue.d/90-ilex-home-setup.issue
# Keep the homed record and a nonempty image at the original pathname.
image=/var/home/andriishafar.home
mv "$image" /var/home/.ilex-test-intact
cp --attributes-only --preserve=mode,ownership,xattr /var/home/.ilex-test-intact "$image"
head -c 4096 /var/home/.ilex-test-intact > "$image"
test "$(stat -c %s "$image")" = 4096
homectl inspect andriishafar --json=short | python3 -c 'import json,sys; assert json.load(sys.stdin)["userName"] == "andriishafar"'
"#)?;
    let damaged = crate::homed::guest(
        vm,
        json!({"action":"installer-profile-damaged", "secrets":[daily]}),
    );
    vm.ssh_ok("mv -f /var/home/.ilex-test-intact /var/home/andriishafar.home")?;
    damaged?;
    vm.ssh_ok(
        r#"set -eu
cmp /var/lib/ilex/home-setup/state.json /run/ilex-pending-state.json
test -e /etc/issue.d/90-ilex-home-setup.issue
chvt 8
TERM=linux setterm --blank poke < /dev/tty8 > /dev/tty8
printf '\033[2J\033[3J\033[H\033[?25l' > /dev/tty8
"#,
    )?;
    crate::homed::guest(
        vm,
        json!({"action":"installer-profile-resume", "secrets":[daily]}),
    )?;
    vm.ssh_ok(
        r#"set -eu
cmp /var/lib/ilex/home-setup/state.json /run/ilex-complete-state.json
test ! -e /etc/issue.d/90-ilex-home-setup.issue
grep -aFq 'Daily home andriishafar is ready' /dev/vcs8
"#,
    )?;
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
    custom_capacity_retry(vm, maintenance, daily)?;
    let proof = json!({"offline":true, "homed_unavailable_retry":true, "local_username_refused":true,
        "installer_maintenance_password":true, "reconciled_original_home":true,
        "authenticated_profile_application":true, "damaged_home_refused":true,
        "custom_capacity_retry":true});
    fs::write(
        artifacts.join("profile-proof.json"),
        serde_json::to_vec_pretty(&proof)?,
    )?;
    Ok(())
}

fn custom_capacity_retry(vm: &qemu::Vm, maintenance: &str, daily: &str) -> Result<()> {
    vm.ssh_ok(r#"set -eu
# The disposable guest has already proven and captured the named profile.
mv /var/lib/ilex/home-setup/state.json /run/ilex-named-state.json
python3 - <<'PY'
import json,pathlib
path=pathlib.Path('/etc/ilex/setup.json')
data=json.loads(path.read_bytes())
data['profile']={'id':None,'schema':None,'mode':'custom'}
path.write_text(json.dumps(data))
PY
if result=$(ilexctl home setup --name ilexcustom --disk-size 1048576 </dev/null 2>&1); then exit 1; fi
printf '%s\n' "$result" | grep -F 'insufficient space'
test ! -e /var/lib/ilex/home-setup/state.json
test ! -e /var/home/ilexcustom.home
# Model a saved intent from before creation began, including older intents
# that recorded an oversized request.
python3 - <<'PY'
import json,pathlib,uuid
state={'schema':1,'profile':{'id':None,'schema':None,'mode':'custom'},
       'name':'ilexcustom','size':1048576,'luks_uuid':uuid.uuid4().hex,'uid':None}
pathlib.Path('/var/lib/ilex/home-setup/state.json').write_text(json.dumps(state))
PY
"#)?;
    crate::homed::guest(
        vm,
        json!({"action":"installer-custom-retry", "secrets":[maintenance, daily]}),
    )?;
    vm.ssh_ok(
        r#"set -eu
python3 - <<'PY'
import json
s=json.load(open('/var/lib/ilex/home-setup/state.json'))
assert s['name']=='ilexcustom2' and s['size']==2 and s['uid'] is not None
PY
test -f /var/home/ilexcustom2.home
test ! -e /var/home/ilexcustom.home
if result=$(ilexctl home setup --name ilexcustom2 --disk-size 3 </dev/null 2>&1); then exit 1; fi
printf '%s\n' "$result" | grep -F 'already recorded'
"#,
    )?;
    Ok(())
}
