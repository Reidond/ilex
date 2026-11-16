"""Executed through a real password-authenticated TTY, as its homed user."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess

home = Path.home()
state = home / ".local/state/ilex/profile.json"
welcome = home / ".config/ilex/welcome.txt"


def cli(*args, code=0):
    result = subprocess.run(["/usr/bin/ilex-agent", "profile", "apply", *args], capture_output=True, text=True)
    assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
    return result


spec = importlib.util.spec_from_file_location("ilex_profile", "/usr/libexec/ilex/profile.py")
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


def fixture(root, code, apps=False):
    module.ROOT = root
    args = argparse.Namespace(name="reidond", selected=False, shell=None, install_apps=apps)
    try:
        result = module.apply(args)
    except (ValueError, OSError):
        result = 2
    assert result == code, (root, result, code)


assert os.getuid() != 0 and os.environ.get("SYSTEMD_HOME") == "1"
cli("--selected")
assert not state.exists()
welcome.parent.mkdir(parents=True, exist_ok=True)
welcome.write_text("preexisting user file\n")
cli("reidond", code=1)
assert welcome.read_text() == "preexisting user file\n"
welcome.unlink()
cli("reidond")
original = welcome.read_bytes()
gnome = bool(json.loads(Path('/usr/share/ilex/image.json').read_text())['shells'])
if gnome:
    assert subprocess.check_output(['dconf', 'read', '/org/gnome/desktop/interface/clock-show-weekday'], text=True).strip() == 'true'
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
assert sentinel.read_text() == "outside sentinel\n"
print("profile apply, rollback, edits/deletions, symlinks/hardlinks and offline apps: passed")
