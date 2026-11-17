"""Executed through a real password-authenticated TTY, as its homed user."""
import argparse
import contextlib
import hashlib
import io
import importlib.util
import json
import os
import pwd
from pathlib import Path
import subprocess
import tempfile

home = Path.home()
state = home / ".local/state/ilex/profile.json"
welcome = home / ".config/ilex/welcome.txt"


def cli(*args, code=0, env=None):
    result = subprocess.run(["/usr/bin/ilex-agent", "profile", "apply", *args], capture_output=True, text=True, env=env)
    assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
    return result


spec = importlib.util.spec_from_file_location("ilex_profile", "/usr/libexec/ilex/profile.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def fixture(root, code, apps=False):
    module.ROOT = root
    args = argparse.Namespace(name="reidond", selected=False, shell=None, install_apps=apps)
    try:
        if apps:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                result = module.apply(args)
            assert any(c.startswith('app ') for c in json.loads(output.getvalue())['conflicts'])
        else:
            result = module.apply(args)
    except (ValueError, OSError):
        result = 2
    assert result == code, (root, result, code)


assert os.getuid() != 0 and os.environ.get("SYSTEMD_HOME") == "1"
record = json.loads(subprocess.check_output(['homectl', 'inspect', pwd.getpwuid(os.getuid()).pw_name, '--json=short'], text=True))
print('homed profile admission:', {key: record.get(key) for key in ('userName', 'uid', 'homeDirectory', 'storage')},
      'states:', [v.get('state') for v in record.get('status', {}).values()],
      'NSS:', pwd.getpwuid(os.getuid()).pw_dir, 'mount:', os.path.ismount(pwd.getpwuid(os.getuid()).pw_dir),
      'binding UIDs:', [v.get('uid') for v in record.get('binding', {}).values()])
cli("--selected")
assert not state.exists()
welcome.parent.mkdir(parents=True, exist_ok=True)
welcome.write_text("preexisting user file\n")
gnome = json.loads(Path('/usr/share/ilex/image.json').read_text())['default-shell'] == 'gnome'
if gnome:
    # This target is outside the authenticated encrypted home. Neither an
    # ancestor nor a database leaf link may redirect a settings write there.
    external = tempfile.TemporaryDirectory(prefix="ilex-profile-external-", dir="/var/tmp")
    external_path = Path(external.name)
    sentinel_db = external_path / "user"
    sentinel_db.write_bytes(b"outside encrypted home sentinel\n")
    dconf_dir = home / ".config/dconf"
    saved_dconf = dconf_dir.with_name("dconf-saved")
    if dconf_dir.exists():
        dconf_dir.rename(saved_dconf)
    dconf_dir.symlink_to(external_path, target_is_directory=True)
    result = cli("reidond", code=1)
    assert any(c.startswith("settings.gnome") for c in json.loads(result.stdout)["conflicts"])
    assert sentinel_db.read_bytes() == b"outside encrypted home sentinel\n"
    assert sorted(p.name for p in external_path.iterdir()) == ["user"]
    dconf_dir.unlink()
    dconf_dir.mkdir(mode=0o700)
    (dconf_dir / "user").symlink_to(sentinel_db)
    result = cli("reidond", code=1)
    assert any(c.startswith("settings.gnome") for c in json.loads(result.stdout)["conflicts"])
    assert sentinel_db.read_bytes() == b"outside encrypted home sentinel\n"
    (dconf_dir / "user").unlink()
    dconf_dir.rmdir()
    if saved_dconf.exists():
        saved_dconf.rename(dconf_dir)
    # Deliberate inherited redirects are neutralised by the writer's clean
    # environment and private bus, not propagated to the session's service.
    redirected = dict(os.environ, XDG_CONFIG_HOME=external.name, XDG_DATA_HOME=external.name,
                      XDG_CACHE_HOME=external.name, DCONF_PROFILE=str(sentinel_db),
                      GSETTINGS_BACKEND="keyfile", DBUS_SESSION_BUS_ADDRESS="unix:path=/nonexistent")
    cli("reidond", code=1, env=redirected)  # preexisting welcome remains a conflict
    assert sentinel_db.read_bytes() == b"outside encrypted home sentinel\n"
    assert sorted(p.name for p in external_path.iterdir()) == ["user"]
    print("real homed dconf directory/leaf symlinks and inherited redirects: passed")
cli("reidond", code=1)
assert welcome.read_text() == "preexisting user file\n"
welcome.unlink()
cli("reidond")
original = welcome.read_bytes()
if gnome:
    assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/clock-show-weekday'], text=True).strip() == 'true'
    subprocess.run(['gsettings', 'reset', 'org.gnome.desktop.interface', 'clock-show-weekday'], check=True)
    assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/clock-show-weekday'], text=True).strip() == ''
    result = cli("reidond", code=1)
    assert "settings.gnome.clock-show-weekday" in json.loads(result.stdout)["conflicts"]
    assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/clock-show-weekday'], text=True).strip() == ''
    assert json.loads(state.read_text())["settings"]["clock-show-weekday"] == "true"
    verified_home = module.active_home()
    try:
        conflicts = []
        managed = module.settings(verified_home, {"gnome": {"clock-show-weekday": False}},
                                  {"clock-show-weekday": "true"}, "gnome", conflicts)
        assert conflicts == ["settings.gnome.clock-show-weekday"] and managed["clock-show-weekday"] == "true"
        assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/clock-show-weekday'], text=True).strip() == ''
    finally:
        os.close(verified_home.fd)
    print("user setting reset survives reapply and is reported as a conflict: passed")
    subprocess.run(['gsettings', 'set', 'org.gnome.desktop.interface', 'clock-show-weekday', 'true'], check=True)
    cli("reidond")
assert json.loads(state.read_text())["files"][".config/ilex/welcome.txt"] == hashlib.sha256(original).hexdigest()
welcome.write_text("edited by user\n")
fixture("/run/ilex-profile-v2", 1)
assert welcome.read_text() == "edited by user\n"
added = home / ".config/ilex/added.txt"
assert added.read_text() == "new public file\n"
welcome.write_bytes(original)
fixture("/run/ilex-profile-v2", 0)
assert welcome.read_text() == "profile revision two\n"
if gnome:
    assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/clock-show-weekday'], text=True).strip() == 'false'
    subprocess.run(['gsettings', 'set', 'org.gnome.desktop.interface', 'color-scheme', 'prefer-dark'], check=True)
added.unlink()
fixture("/run/ilex-profile-v2", 1)
assert not added.exists()

# The actual older shipped profile must decline newer mutable home state.
before = state.read_bytes()
cli("reidond", code=2)
assert state.read_bytes() == before
fixture("/run/ilex-profile-rewritten-v2", 2)
assert state.read_bytes() == before
future = json.loads(before)
future["schema"] = 99
state.write_text(json.dumps(future))
future_raw = state.read_bytes()
fixture("/run/ilex-profile-v2", 2)
assert state.read_bytes() == future_raw
state.write_bytes(before)

outside = home / "outside-profile"
outside.mkdir()
sentinel = outside / "welcome.txt"
sentinel.write_text("outside sentinel\n")
welcome.unlink()
welcome.symlink_to(sentinel)
fixture("/run/ilex-profile-v2", 1)
assert welcome.is_symlink() and sentinel.read_text() == "outside sentinel\n"
welcome.unlink()
welcome.write_text("profile revision two\n")
os.link(welcome, home / "linked-profile")
fixture("/run/ilex-profile-v2", 1)
(home / "linked-profile").unlink()
directory = welcome.parent
saved = directory.with_name("ilex-saved")
directory.rename(saved)
directory.symlink_to(outside, target_is_directory=True)
fixture("/run/ilex-profile-v2", 1)
assert sentinel.read_text() == "outside sentinel\n"
if gnome:
    assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/color-scheme'], text=True).strip() == "'prefer-dark'"
directory.unlink()
saved.rename(directory)
saved_state = state.parent.with_name("ilex-saved")
state.parent.rename(saved_state)
state.parent.symlink_to(outside, target_is_directory=True)
fixture("/run/ilex-profile-v2", 2)
assert not (outside / "profile.json").exists()
state.parent.unlink()
saved_state.rename(state.parent)
fixture("/run/ilex-profile-v2", 1, apps=True)
if gnome:
    flatpak_dir = home / ".local/share/flatpak"
    saved_flatpak = flatpak_dir.with_name("flatpak-saved")
    if flatpak_dir.exists():
        flatpak_dir.rename(saved_flatpak)
    flatpak_dir.symlink_to(external_path, target_is_directory=True)
    fixture("/run/ilex-profile-v2", 1, apps=True)
    assert sentinel_db.read_bytes() == b"outside encrypted home sentinel\n"
    assert sorted(p.name for p in external_path.iterdir()) == ["user"]
    flatpak_dir.unlink()
    if saved_flatpak.exists():
        saved_flatpak.rename(flatpak_dir)
    external.cleanup()
assert sentinel.read_text() == "outside sentinel\n"
print("profile apply, rollback, edits/deletions, symlinks/hardlinks and offline apps: passed")
