//! Opt-in installed-system qualification; never registered by default CI.
use crate::common::{self, Group, trial};
use crate::disk::vmtest;
use anyhow::{Result, ensure};
use ilex_devkit::qemu::{self, Vm, VmConfig};
use ilex_vmctl::qmp::Qmp;
use libtest_mimic::Trial;
use serde_json::{Value, json};
use std::sync::Mutex;
use std::time::Duration;

static GROUP: Group<Mutex<Vm>> = Group::new("apps", boot);
const INSTALL: Duration = Duration::from_secs(2400);
pub fn expect(count: usize) {
    GROUP.expect(count);
}
pub fn shutdown() {
    GROUP.finish();
}
pub fn trials() -> Vec<Trial> {
    vec![
        trial("apps::offline", || GROUP.run(|_| Ok(()))),
        trial("apps::install", || {
            GROUP.run(|vm| {
                let mut vm = vm.lock().unwrap();
                install(&mut vm)?;
                list_changes(&mut vm)?;
                policy(&vm)?;
                fast_path(&mut vm)
            })
        }),
        trial("apps::update", || {
            GROUP.run(|vm| {
                let mut vm = vm.lock().unwrap();
                install(&mut vm)?;
                updates(&mut vm)
            })
        }),
    ]
}

fn link(vm: &Vm, up: bool) -> Result<()> {
    Qmp::connect(&vm.info.qmp_sock)?.execute("set_link", json!({"name":"internet", "up":up}))?;
    Ok(())
}

fn boot() -> Result<Mutex<Vm>> {
    let ws = common::workspace()?;
    ensure!(
        !common::selection()?.shells.is_empty(),
        "apps:: requires a desktop image with app lists"
    );
    let mut config = VmConfig::new("apps", vmtest::disk(&ws)?);
    config.apps = true;
    config.isolated_ssh = true;
    config.smbios.push(format!(
        "type=11,value=io.systemd.credential.binary:systemd.unit-dropin.ilex-apps.timer={}",
        qemu::base64(b"[Timer]\nOnBootSec=15s\nRandomizedDelaySec=0\n")
    ));
    let mut vm = Vm::boot(&ws, &config)?;
    link(&vm, false)?;
    vm.wait_ssh(vmtest::BOOT_TIMEOUT)?;
    vm.ssh_ok(r#"set -eu
device=$(ip -o -4 addr show | awk '$4 ~ /^10\.0\.3\./ { print $2; exit }')
connection=$(nmcli -g GENERAL.CONNECTION device show "$device")
nmcli connection modify "$connection" ipv4.never-default yes ipv6.never-default yes ipv4.ignore-auto-dns yes
nmcli device reapply "$device"
"#)?;
    vm.wait_for("offline apps retry", "test \"$(systemctl show ilex-apps.service -p SubState --value)\" = auto-restart && test \"$(systemctl show ilex-apps.service -p ExecMainStatus --value)\" = 75", Duration::from_secs(180))?;
    vm.ssh_ok("test \"$(systemctl is-system-running --wait)\" = running && test -z \"$(systemctl --failed --no-legend)\" && systemctl is-active gdm && test \"$(getenforce)\" = Enforcing")?;
    let state: Value = serde_json::from_str(&vm.ssh_ok("cat /var/lib/ilex/apps/state.json")?)?;
    ensure!(state["result"] == "offline", "offline state: {state}");
    ensure!(
        vm.ssh_ok("flatpak list --system --app --columns=application")?
            .is_empty(),
        "offline boot installed apps"
    );
    vm.ssh_ok("systemctl stop ilex-apps.service")?;
    link(&vm, true)?;
    println!("ok   offline timer exits 75; boot and GDM stay healthy; link restored");
    Ok(Mutex::new(vm))
}

pub fn install(vm: &mut Vm) -> Result<()> {
    vm.ssh_ok("systemctl stop ilex-apps.timer ilex-apps.service")?;
    // Simulate Fedora's remote retained in /var from an older deployment.
    // Reuse the trusted public key file; the source name, not its URL, is the
    // compatibility policy. No unsigned test repository or second payload.
    vm.ssh_ok("flatpak remote-add --system --if-not-exists fedora /usr/share/flatpak/remotes.d/flathub.flatpakrepo")?;
    // Simulate a pre-policy system catalogue retained across an upgrade.
    // Only this opt-in tier enumerates/downloads it; sync must remove it even
    // on the unchanged fast path, and Software must offer user results only.
    vm.ssh_ok(r#"set -eu
flatpak remote-modify --system --enumerate flathub
flatpak update --system --appstream -y --noninteractive flathub
test -f "/var/lib/flatpak/appstream/flathub/$(uname -m)/active/appstream.xml.gz"
flatpak remote-modify --system --no-enumerate flathub
"#)?;
    vm.ssh_ok("systemctl start --no-block ilex-apps.service")?;
    vm.wait_for("complete system app installation", "test ! -e /var/lib/flatpak/appstream/flathub && python3 -c 'import json; assert json.load(open(\"/var/lib/ilex/apps/state.json\"))[\"result\"] == \"complete\"'", INSTALL)?;
    let evidence = vm.ssh_ok(r#"python3 - <<'PY'
import configparser,json,pathlib,subprocess
state=json.load(open('/var/lib/ilex/apps/state.json'))
image=json.load(open('/usr/share/ilex/image.json'))
machine=json.load(open('/usr/share/ilex/machine.json'))
wanted={app for entry in [*image['shells'],machine] for app in entry.get('apps',{}).get('flatpak',[])}
assert {a['id'] for a in state['apps']}==wanted and wanted,state
marker=pathlib.Path('/var/tmp/ilex-e2e-apps-installed')
assert all(a['status'] in ('installed','declined') for a in state['apps']),state
if not marker.exists():assert all(a['status']=='installed' for a in state['apps']),state
config=configparser.ConfigParser()
config.read('/var/lib/flatpak/repo/config')
assert config['core']['xa.preinstalled']
assert config['remote "flathub"']['xa.noenumerate']=='true'
assert config['remote "flathub"']['gpg-verify']=='true'
assert config['remote "flathub"']['gpg-verify-summary']=='true'
assert config['remote "fedora"']['xa.disable']=='true'
assert not pathlib.Path('/var/lib/flatpak/appstream/flathub').exists()
pins=subprocess.check_output(['flatpak','pin','--system'],text=True)
assert 'runtime/org.gnome.Platform/' not in pins and 'runtime/org.freedesktop.Platform/' not in pins,pins
for app in state['apps']:
    if app['status']=='installed':
        assert app['origin']=='flathub',app
        assert subprocess.check_output(['flatpak','info','--system','--show-origin',app['id']],text=True).strip()=='flathub',app
        assert subprocess.check_output(['flatpak','info','--system','--show-ref',app['id']],text=True).strip()==app['reference'],app
        assert pathlib.Path('/var/lib/flatpak/exports/share/applications',app['id']+'.desktop').exists(),app
marker.touch()
print(json.dumps(state,indent=2))
PY"#)?;
    qemu::write_artifact(
        &qemu::artifact_dir(&common::workspace()?, "apps")?,
        "installed.json",
        &evidence,
    )?;
    println!("ok   every requested app is installed, signed and exported from Flathub");
    Ok(())
}

fn list_changes(vm: &mut Vm) -> Result<()> {
    vm.ssh_ok(r#"set -eu
flatpak install --system -y --noninteractive flathub org.gnome.Extensions
mkdir -p /var/home/apps-data/.var/app/org.gnome.Decibels
echo retained > /var/home/apps-data/.var/app/org.gnome.Decibels/marker
mkdir -p /run/ilex-apps-lists
cp /usr/share/flatpak/preinstall.d/*.preinstall /run/ilex-apps-lists/
mount --bind /run/ilex-apps-lists /usr/share/flatpak/preinstall.d
python3 - <<'PY'
import configparser,pathlib
for path in pathlib.Path('/run/ilex-apps-lists').glob('*.preinstall'):
    c=configparser.ConfigParser();c.optionxform=str;c.read(path)
    c.remove_section('Flatpak Preinstall org.gnome.Decibels')
    with path.open('w') as f:c.write(f)
pathlib.Path('/run/ilex-apps-lists/changes.preinstall').write_text('[Flatpak Preinstall org.gnome.Chess]\nBranch=stable\n[Flatpak Preinstall org.gnome.Extensions]\nBranch=stable\n[Flatpak Preinstall org.ilex.Unavailable]\nBranch=stable\n')
PY
ilexctl apps sync
python3 - <<'PY'
import json
s=json.load(open('/var/lib/ilex/apps/state.json'))
assert s['result']=='degraded',s
assert any(a['id']=='org.ilex.Unavailable' and a['status']=='unavailable' for a in s['apps']),s
PY
! flatpak info --system org.gnome.Decibels
flatpak info --system org.gnome.Chess
grep retained /var/home/apps-data/.var/app/org.gnome.Decibels/marker
sed -i '/\[Flatpak Preinstall org.gnome.Extensions\]/,+1d; /\[Flatpak Preinstall org.ilex.Unavailable\]/,+1d' /run/ilex-apps-lists/changes.preinstall
ilexctl apps sync
flatpak info --system org.gnome.Extensions
flatpak uninstall --system -y --noninteractive org.gnome.Calculator
ilexctl apps sync
! flatpak info --system org.gnome.Calculator
ilexctl apps status --json | python3 -c 'import sys,json; assert any(a["id"]=="org.gnome.Calculator" and a["status"]=="declined" for a in json.load(sys.stdin)["apps"])'
# Simulate switching to an image with no former shell list.
rm /run/ilex-apps-lists/*.preinstall
ilexctl apps sync
! flatpak info --system org.gnome.TextEditor
flatpak info --system org.gnome.Extensions
grep retained /var/home/apps-data/.var/app/org.gnome.Decibels/marker
umount /usr/share/flatpak/preinstall.d
ilexctl apps sync
"#)?;
    println!("ok   list additions/removals, unavailable IDs, manual installs and retained data");
    Ok(())
}

fn policy(vm: &Vm) -> Result<()> {
    vm.ssh_ok(r#"set -eu
useradd -m -G wheel appsuser
for action in $(pkaction | grep '^org.freedesktop.Flatpak.'); do
    runuser -u appsuser -- sh -c 'pkcheck --action-id "$1" --process $$; test $? -eq 1' sh "$action"
done
! runuser -u appsuser -- flatpak remote-modify --system --enumerate flathub
! runuser -u appsuser -- flatpak install --system --noninteractive -y flathub org.gnome.Chess
runuser -u appsuser -- flatpak remote-add --user --if-not-exists flathub /usr/share/flatpak/remotes.d/flathub.flatpakrepo
runuser -u appsuser -- flatpak install --user --noninteractive -y flathub org.gnome.Chess
runuser -u appsuser -- flatpak info --user org.gnome.Chess
! flatpak info --system org.gnome.Chess
"#)?;
    println!("ok   system helper denied for wheel; user remote and real user install work");
    Ok(())
}

fn fast_path(vm: &mut Vm) -> Result<()> {
    vm.reboot(vmtest::BOOT_TIMEOUT)?;
    vm.wait_ssh(vmtest::BOOT_TIMEOUT)?;
    link(vm, false)?;
    let output = vm.ssh_ok("ilexctl apps sync --update")?;
    ensure!(
        output.contains("unchanged; no network work"),
        "no reboot fast path: {output}"
    );
    link(vm, true)?;
    Ok(())
}

fn updates(vm: &mut Vm) -> Result<()> {
    vm.ssh_ok(r#"set -eu
flatpak install --system -y --noninteractive flathub org.gnome.Extensions
for app in org.gnome.TextEditor org.gnome.Extensions; do
    flatpak remote-info --system --log flathub "$app" > /run/app-history
    commit=$(awk '/Commit:/ {if(first=="") first=$2; else if($2!=first) {print $2; exit}}' /run/app-history)
    test -n "$commit"
    # A retained older build may use an older platform than today's app.
    # Prepare that signed dependency explicitly while keeping no-enumerate.
    runtime=$(flatpak remote-info --system --commit="$commit" --show-runtime flathub "$app")
    if ! flatpak info --system "runtime/$runtime" >/dev/null 2>&1; then
        flatpak install --system --runtime --no-auto-pin -y --noninteractive flathub "runtime/$runtime"
    fi
    flatpak update --system -y --noninteractive --commit="$commit" "$app"
done
python3 - <<'PY'
import json,pathlib
p=pathlib.Path('/var/lib/ilex/apps/state.json');s=json.loads(p.read_text());s['last_update']=0;p.write_text(json.dumps(s))
PY
ilexctl apps sync --update
ilexctl updates list --json | python3 -c 'import sys,json; s=json.load(sys.stdin); refs=[u.get("ref","") for u in s["updates"] if u["kind"]=="flatpak"]; assert all(any(app+"/" in ref for ref in refs) for app in ["org.gnome.TextEditor","org.gnome.Extensions"]),s'
"#)?;
    let state = vm.ssh_ok("ilexctl updates list --json")?;
    qemu::write_artifact(
        &qemu::artifact_dir(&common::workspace()?, "apps")?,
        "updates.json",
        &state,
    )?;
    fast_path(vm)
}
