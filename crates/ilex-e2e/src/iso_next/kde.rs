//! What only the KDE edition's session has to prove. KWin runs standalone (no
//! Plasma), and the claim of the edition is that the installer is its only
//! client: no shell, panel, notification daemon or other program, one window
//! that fills the screen. Three independent views of the running guest, all
//! recorded in `next-kde-session.txt`:
//!
//! * the processes of the installer's user, from `/proc` (a Plasma shell, KDE
//!   daemon, portal or Xwayland would be among them);
//! * the clients connected to KWin's Wayland socket: the peers of its accepted
//!   connections, found by socket inode in every process's descriptors (a
//!   second client of any kind would be a second peer);
//! * what KWin itself says: its D-Bus names on the session bus and a KWin
//!   script that reports its window list through a D-Bus call that
//!   `busctl monitor` captures (a script's own console output is not kept
//!   anywhere by default).

use super::*;

/// Programs that must not run in the installer's session: Plasma's shell and
/// its daemons, portals, an X server. Process names (`comm`) are cut at 15
/// bytes, so each is compared by that prefix.
const FORBIDDEN: &[&str] = &[
    "plasmashell",
    "kded6",
    "kglobalacceld",
    "krunner",
    "ksmserver",
    "kscreenlocker_greet",
    "kactivitymanagerd",
    "kwalletd6",
    "ksplashqml",
    "startplasma-wayland",
    "plasma_session",
    "plasma-xdg-desktop-portal-kde",
    "xdg-desktop-portal",
    "xdg-document-portal",
    "Xwayland",
    "Xorg",
];

/// Is `comm` (a process name as `/proc/<pid>/comm` has it) one of [`FORBIDDEN`]?
fn is_forbidden(comm: &str) -> bool {
    FORBIDDEN
        .iter()
        .any(|name| comm == &name[..name.len().min(15)])
}

/// The session bus names' owners that are allowed: KWin itself, the installer,
/// the user manager that owns the bus (systemd --user, dbus-broker and its
/// launcher) and the tool this probe runs. `-` is a name that is only
/// activatable: nothing owns it.
const BUS_OWNERS: &[&str] = &[
    "kwin_wayland",
    "ilex-installer",
    "systemd",
    "dbus-broker",
    "dbus-broker-lau",
    "busctl",
    "-",
];

const REPORT_SCRIPT: &str = r#"
const windows = workspace.windowList();
const outputs = workspace.screens;
callDBus("org.ilex.Probe", "/probe", "org.ilex.Probe", "Report",
    "windows=" + windows.length +
    " list=" + windows.map(w => w.resourceClass + "|fullscreen=" + w.fullScreen + "|" +
        w.frameGeometry.width + "x" + w.frameGeometry.height + "|normal=" + w.normalWindow).join(";") +
    " outputs=" + outputs.length +
    " screen=" + outputs.map(o => o.geometry.width + "x" + o.geometry.height).join(";"));
"#;

const CLOSE_SCRIPT: &str = r#"
const windows = workspace.windowList();
windows.forEach(w => w.closeWindow());
callDBus("org.ilex.Probe", "/probe", "org.ilex.Probe", "Report", "closed=" + windows.length);
"#;

/// Run `script` inside KWin as the installer's user and return the text it
/// reports. The call is made through the session bus the user manager gives
/// the session; the report is read from a bus monitor.
fn kwin_script(vm: &Vm, script: &str) -> Result<String> {
    let command = format!(
        r#"set -u
uid=$(id -u ilex-installer)
runtime=/run/user/$uid
as_user() {{ runuser -u ilex-installer -- env XDG_RUNTIME_DIR=$runtime DBUS_SESSION_BUS_ADDRESS=unix:path=$runtime/bus "$@"; }}
file=/run/ilex-kwin-probe.js
monitor=/run/ilex-kwin-probe.json
cat > $file <<'SCRIPT'
{script}
SCRIPT
chmod 644 $file
: > $monitor; chmod 666 $monitor
as_user busctl --user monitor --json=short > $monitor 2>/dev/null &
mon=$!
sleep 1
name=ilex-probe-$$
id=$(as_user busctl --user call org.kde.KWin /Scripting org.kde.kwin.Scripting loadScript ss $file $name | awk '{{print $2}}')
as_user busctl --user call org.kde.KWin /Scripting/Script$id org.kde.kwin.Script run
sleep 2
kill $mon 2>/dev/null
as_user busctl --user call org.kde.KWin /Scripting org.kde.kwin.Scripting unloadScript s $name >/dev/null
grep -a '"member":"Report"' $monitor | head -1
rm -f $file $monitor
"#
    );
    let line = vm
        .ssh_ok(&command)
        .context("running a KWin script as the installer's user")?;
    let json: serde_json::Value = serde_json::from_str(line.trim())
        .with_context(|| format!("no report from the KWin script: {line:?}"))?;
    json["payload"]["data"][0]
        .as_str()
        .map(str::to_owned)
        .with_context(|| format!("the KWin script's report has no text: {line}"))
}

/// A close request for every window, as the compositor's close shortcut sends
/// one (KWin's own shortcut belongs to Plasma's global shortcut daemon, which
/// the session does not run).
pub(super) fn kwin_close_windows(vm: &Vm) -> Result<()> {
    let report = kwin_script(vm, CLOSE_SCRIPT)?;
    ensure!(
        report == "closed=1",
        "expected KWin to ask one window to close: {report:?}"
    );
    Ok(())
}

/// The installer is the only client of the KWin session; the evidence goes to
/// `next-kde-session.txt`.
pub(super) fn kde_session(vm: &mut Vm, artifacts: &Path) -> Result<()> {
    const SCRIPT: &str = r#"set -u
uid=$(id -u ilex-installer)
runtime=/run/user/$uid
socket=$runtime/wayland-0
as_user() { runuser -u ilex-installer -- env XDG_RUNTIME_DIR=$runtime DBUS_SESSION_BUS_ADDRESS=unix:path=$runtime/bus "$@"; }
echo "=== processes of ilex-installer"
for p in /proc/[0-9]*; do
    [ "$(stat -c %U "$p" 2>/dev/null)" = ilex-installer ] || continue
    echo "PROCESS $(cat "$p/comm") :: $(tr '\0' ' ' < "$p/cmdline")"
done
echo "=== clients of $socket"
ss -xpnH | awk -v s="$socket" '$5 == s && $9 ~ /kwin_wayland/ {print $8}' | sort -u > /run/ilex-kde-peers
echo "PEERS $(tr '\n' ' ' < /run/ilex-kde-peers)"
for inode in $(cat /run/ilex-kde-peers); do
    for fd in /proc/[0-9]*/fd/*; do
        [ "$(readlink "$fd" 2>/dev/null)" = "socket:[$inode]" ] || continue
        pid=${fd#/proc/}; pid=${pid%%/*}
        echo "CLIENT $(cat "/proc/$pid/comm")"
    done
done
rm -f /run/ilex-kde-peers
echo "=== names on the session bus"
as_user busctl --user list --no-legend --no-pager | awk '{print "NAME", $1, $3}'
echo "=== end"
"#;
    let report = vm
        .ssh_ok(SCRIPT)
        .context("collecting the KDE session's processes, clients and bus names")?;
    let windows = kwin_script(vm, REPORT_SCRIPT)?;
    let mut text = report.clone();
    text.push_str(&format!(
        "\n=== KWin's own window list\nWINDOWS {windows}\n"
    ));
    fs::write(artifacts.join("next-kde-session.txt"), &text)?;

    let lines = |prefix: &str| -> Vec<&str> {
        report
            .lines()
            .filter_map(|line| line.strip_prefix(prefix))
            .collect()
    };

    // 1. The processes: KWin and the installer, nothing that is Plasma's.
    let processes: Vec<&str> = lines("PROCESS ")
        .into_iter()
        .filter_map(|line| line.split(" :: ").next())
        .collect();
    for needed in ["kwin_wayland", "ilex-installer"] {
        ensure!(
            processes.contains(&needed),
            "no {needed} among the installer user's processes (next-kde-session.txt)"
        );
    }
    let forbidden: Vec<&&str> = processes.iter().filter(|comm| is_forbidden(comm)).collect();
    ensure!(
        forbidden.is_empty(),
        "programs that belong to Plasma or X11 run in the installer's session: {forbidden:?} (next-kde-session.txt)"
    );

    // 2. The clients of KWin's socket: exactly the installer.
    let clients = lines("CLIENT ");
    ensure!(
        clients == ["ilex-installer"],
        "KWin's Wayland clients are {clients:?}, expected only ilex-installer (next-kde-session.txt)"
    );

    // 3. Who owns a name on the session bus.
    let names: Vec<(&str, &str)> = lines("NAME ")
        .into_iter()
        .filter_map(|line| line.split_once(' '))
        .collect();
    ensure!(
        names.iter().any(|(_, owner)| *owner == "kwin_wayland"),
        "KWin owns no name on the session bus (next-kde-session.txt)"
    );
    let strangers: Vec<&(&str, &str)> = names
        .iter()
        .filter(|(_, owner)| !BUS_OWNERS.contains(owner))
        .collect();
    ensure!(
        strangers.is_empty(),
        "names on the session bus belong to other programs: {strangers:?} (next-kde-session.txt)"
    );

    // 4. What KWin says about its windows: one, the installer's, fullscreen,
    //    as large as the output.
    let words: BTreeMap<&str, &str> = windows
        .split_whitespace()
        .filter_map(|word| word.split_once('='))
        .collect();
    ensure!(
        words.get("windows") == Some(&"1") && words.get("outputs") == Some(&"1"),
        "KWin reports {windows:?}: expected one window on one output"
    );
    let list = words.get("list").copied().unwrap_or_default();
    let mut parts = list.split('|');
    let (class, fullscreen, size, normal) = (
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    );
    ensure!(
        class == "org.ilex.Installer" && fullscreen == "fullscreen=true" && normal == "normal=true",
        "KWin's only window is not the installer's fullscreen window: {windows:?}"
    );
    ensure!(
        words.get("screen") == Some(&size),
        "the window ({size}) is not the size of the output: {windows:?}"
    );
    println!("iso::next kde session: one client, one window ({size}), no Plasma");
    Ok(())
}
