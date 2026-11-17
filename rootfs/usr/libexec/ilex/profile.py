"""Apply public image profiles only as the owner of an authenticated homed home.

No hooks, record imports, chown, arbitrary settings paths or shell evaluation.
All home traversal is relative to pinned, owned, no-follow directory FDs.
"""

import argparse
from contextlib import ExitStack, contextmanager
import fcntl
import hashlib
import json
import os
import pwd
import re
import stat
import subprocess
import sys
import uuid

ROOT = "/usr/share/ilex"
STATE = ".local/state/ilex/profile.json"
DIR = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
SETTINGS_WORKER = """import json, runpy, sys
module = runpy.run_path('/usr/libexec/ilex/profile.py', run_name='ilex_settings_worker')
request = json.loads(sys.stdin.read(1048577))
conflicts = []
managed = module['settings_worker'](request['data'], request['previous'], conflicts)
print(json.dumps({'managed': managed, 'conflicts': conflicts}))
"""


def digest(data):
    return hashlib.sha256(data).hexdigest()


def parts(path):
    if not isinstance(path, str) or not path or "\\" in path:
        raise ValueError("invalid relative profile path")
    values = path.split("/")
    if any(p in ("", ".", "..") or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in p) for p in values):
        raise ValueError("invalid relative profile path")
    return values


class Home:
    def __init__(self, path, owner):
        self.path = path
        self.owner = owner
        self.fd = os.open(path, DIR)
        self.owned(self.fd, directory=True)

    def owned(self, fd, directory=False):
        meta = os.fstat(fd)
        if meta.st_uid != self.owner or (not directory and (not stat.S_ISREG(meta.st_mode) or meta.st_nlink != 1)):
            raise ValueError("profile path must be owned by the user and be a regular, unlinked file or directory")
        if meta.st_mode & 0o022:
            raise ValueError("profile path must not be writable by other users")
        return meta

    @contextmanager
    def parent(self, path, create=False):
        components = parts(path)
        fd = os.dup(self.fd)
        try:
            for component in components[:-1]:
                if create:
                    try:
                        os.mkdir(component, 0o700, dir_fd=fd)
                    except FileExistsError:
                        pass
                child = os.open(component, DIR, dir_fd=fd)
                os.close(fd)
                fd = child
                self.owned(fd, directory=True)
            yield fd, components[-1]
        finally:
            os.close(fd)

    def read(self, path):
        try:
            with self.parent(path) as (fd, name):
                source = os.open(name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
                with os.fdopen(source, "rb") as stream:
                    self.owned(stream.fileno())
                    value = stream.read(1048577)
                    if len(value) > 1048576:
                        raise ValueError("profile file is too large")
                    return value
        except FileNotFoundError:
            return None

    def write(self, path, data, expected):
        with self.parent(path, create=True) as (fd, name):
            temporary = f".ilex-profile-{uuid.uuid4().hex}"
            try:
                target = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=fd)
                with os.fdopen(target, "wb") as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
                # Recheck after staging. First creation uses link's no-replace
                # semantics; existing unmanaged files can never be adopted.
                current = self.read(path)
                if (digest(current) if current is not None else None) != expected:
                    raise ValueError("file changed while applying profile")
                if expected is None:
                    os.link(temporary, name, src_dir_fd=fd, dst_dir_fd=fd, follow_symlinks=False)
                else:
                    os.rename(temporary, name, src_dir_fd=fd, dst_dir_fd=fd)
                os.fsync(fd)
            finally:
                try:
                    os.unlink(temporary, dir_fd=fd)
                except FileNotFoundError:
                    pass


def run(*args, **options):
    result = subprocess.run(args, capture_output=True, text=True, timeout=120, check=False, **options)
    if result.returncode:
        # Do not echo subprocess output or user configuration into the journal.
        raise ValueError(f"{os.path.basename(args[0])} failed (exit {result.returncode})")
    return result.stdout.strip()


def confined(home, directories, command, payload=None, network=False):
    """External writers see only pinned home subdirectories as writable.

    A path check alone cannot constrain the session's existing dconf service.
    Mask homes/runtime, clear inherited XDG/dconf/D-Bus redirects and use a
    private bus. Even a concurrent symlink swap cannot write outside the
    explicitly bound directory in the authenticated home; the rest is RO.
    """
    with ExitStack() as opened:
        arguments = ["/usr/bin/bwrap", "--die-with-parent", "--unshare-all"]
        if network:
            arguments.append("--share-net")
        arguments += ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
                      "--tmpfs", "/tmp", "--tmpfs", "/run", "--tmpfs", "/var/home",
                      "--dir", home.path, "--dir", f"/run/user/{home.owner}", "--clearenv"]
        for key, value in {"PATH": "/usr/bin:/usr/sbin", "HOME": home.path,
                           "XDG_CONFIG_HOME": home.path + "/.config",
                           "XDG_DATA_HOME": home.path + "/.local/share",
                           "XDG_CACHE_HOME": home.path + "/.cache",
                           "XDG_RUNTIME_DIR": f"/run/user/{home.owner}",
                           "GSETTINGS_BACKEND": "dconf",
                           "DCONF_PROFILE": "/usr/libexec/ilex/dconf-profile"}.items():
            arguments += ["--setenv", key, value]
        descriptors = []
        for directory in directories:
            fd, _ = opened.enter_context(home.parent(directory + "/.anchor", create=True))
            descriptors.append(fd)
            arguments += ["--dir", home.path + "/" + directory,
                          "--bind-fd", str(fd), home.path + "/" + directory]
        arguments += ["--chdir", home.path, "--", *command]
        return run(*arguments, pass_fds=descriptors, input=payload)


def read_image(path):
    # Immutable root is trusted, but reject symlink inputs at every level.
    fd = os.open("/", DIR)
    try:
        components = parts(path.lstrip("/"))
        for component in components[:-1]:
            child = os.open(component, DIR, dir_fd=fd)
            os.close(fd)
            fd = child
        leaf = os.open(components[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=fd)
        with os.fdopen(leaf, "rb") as stream:
            meta = os.fstat(stream.fileno())
            if not stat.S_ISREG(meta.st_mode):
                raise ValueError("image profile must be a regular file")
            if meta.st_size > 1048576:
                raise ValueError("image profile is too large")
            value = stream.read(1048577)
            if len(value) > 1048576:
                raise ValueError("image profile is too large")
            return value
    finally:
        os.close(fd)


def catalogue():
    data = json.loads(read_image(f"{ROOT}/users.json"))
    if data.get("schema") != 1 or not isinstance(data.get("profiles"), list):
        raise ValueError("unsupported user catalogue schema")
    return data


def profile(name):
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,30}", name):
        raise ValueError("invalid profile ID")
    entry = next((p for p in catalogue()["profiles"] if p["id"] == name), None)
    if entry is None:
        raise ValueError("profile is not installed in this image")
    raw = read_image(f"{ROOT}/users/{name}/user.json")
    data = json.loads(raw)
    if entry.get("schema") != 1 or entry["sha256"] != digest(raw) or data.get("schema") != 1 or data.get("revision") != entry["revision"]:
        raise ValueError("profile schema, revision or digest differs from catalogue")
    for path, expected in data["files"].items():
        parts(path)
        if path not in {".gitignore", ".editorconfig", ".config/git/ignore"} and not re.fullmatch(
            r"\.config/ilex/[a-z][a-z0-9-]*\.(?:txt|toml|json)", path
        ):
            raise ValueError("profile path is outside the safe dotfile allowlist")
        if digest(read_image(f"{ROOT}/users/{name}/home/{path}")) != expected:
            raise ValueError("profile file hash mismatch")
    return entry, data


def active_home():
    uid = os.getuid()
    if uid == 0 or os.geteuid() != uid:
        raise ValueError("run profile apply as the authenticated home user, without sudo")
    account = pwd.getpwuid(uid)
    record = json.loads(run("/usr/bin/homectl", "inspect", account.pw_name, "--json=short"))
    machine = read_image("/etc/machine-id").decode("ascii").strip()
    binding = record.get("binding", {}).get(machine, {})
    if (record.get("userName") != account.pw_name or record.get("uid", binding.get("uid")) != uid
        or record.get("homeDirectory", binding.get("homeDirectory")) != account.pw_dir
        or record.get("storage", binding.get("storage")) != "luks"
        or record.get("status", {}).get(machine, {}).get("state") != "active"
        or not os.path.ismount(account.pw_dir)):
        raise ValueError("profile application requires the caller's active LUKS homed home")
    # /home is the image's intentional /var/home alias, not profile data.
    path = os.path.realpath(account.pw_dir)
    if path != f"/var/home/{account.pw_name}":
        raise ValueError("home is outside homed's supported namespace")
    return Home(path, uid)


def settings_worker(data, previous, conflicts):
    managed = dict(previous)
    for key, value in data.get("gnome", {}).items():
        if key not in {"color-scheme", "clock-show-weekday", "clock-format"}:
            raise ValueError("unsupported GNOME setting")
        variant = ("true" if value else "false") if type(value) is bool else f"'{value}'"
        path = f"/org/gnome/desktop/interface/{key}"
        try:
            current = run("/usr/bin/dconf", "read", path)
            if (key in previous and current != previous[key]) or (key not in previous and current):
                conflicts.append(f"settings.gnome.{key}")
                continue
            if key in previous and variant == previous[key]:
                continue
            run("/usr/bin/gsettings", "set", "org.gnome.desktop.interface", key, variant)
            managed[key] = run("/usr/bin/dconf", "read", path)
        except (ValueError, OSError, subprocess.TimeoutExpired):
            conflicts.append(f"settings.gnome.{key} (unavailable; retry after login)")
    return managed


def settings(home, data, previous, shell, conflicts):
    if shell != "gnome" or not data.get("gnome"):
        return dict(previous)
    try:
        # Reject redirected databases before launching any writer. The
        # namespace also closes the check/use race and contains service writes.
        home.read(".config/dconf/user")
        result = json.loads(confined(home, [".config/dconf"], [
            "/usr/bin/dbus-run-session", "--", "/usr/bin/python3", "-I", "-S",
            "-c", SETTINGS_WORKER,
        ], json.dumps({"data": data, "previous": previous})))
        conflicts.extend(result["conflicts"])
        return result["managed"]
    except (ValueError, OSError, subprocess.TimeoutExpired):
        conflicts.append("settings.gnome (unsafe storage or unavailable writer; retry after login)")
        return dict(previous)


def apply(args):
    home = active_home()
    lock = None
    try:
        # Lock the directory inode, leaving no lock pathname to replace.
        with home.parent(STATE, create=True) as (fd, _):
            lock = os.dup(fd)
        fcntl.flock(lock, fcntl.LOCK_EX)
        old_raw = home.read(STATE)
        old = json.loads(old_raw) if old_raw is not None else None
        if old is not None and (old.get("schema") != 1 or type(old.get("revision")) is not int):
            raise ValueError("unsupported profile state; declining changes")
        if args.selected:
            if old is None:
                return 0
            name = old["profile"]
        else:
            name = args.name
        if not name:
            raise ValueError("choose an installed profile ID")
        entry, data = profile(name)
        image = json.loads(read_image(f"{ROOT}/image.json"))
        shell = args.shell
        if shell and shell not in [s["name"] for s in image["shells"]]:
            raise ValueError("requested shell is not installed")
        if shell is None:
            shell = image["default-shell"]
        if old is not None:
            if old.get("profile") != name:
                raise ValueError("a different profile is already selected; declining changes")
            if old["revision"] > entry["revision"]:
                raise ValueError("home profile is newer than this image; declining downgrade")
            if old["revision"] == entry["revision"] and old.get("digest") != entry["sha256"]:
                raise ValueError("profile changed without increasing its revision; declining changes")
        managed = dict(old.get("files", {})) if old else {}
        conflicts = []
        for path, expected in data["files"].items():
            try:
                current = home.read(path)
                current_hash = digest(current) if current is not None else None
                if current is not None and current_hash != managed.get(path):
                    conflicts.append(path)
                    continue
                # A deletion of a previously managed file is a user edit too.
                if current is None and path in managed:
                    conflicts.append(path)
                    continue
                if current_hash != expected:
                    home.write(path, read_image(f"{ROOT}/users/{name}/home/{path}"), current_hash)
                managed[path] = expected
            except (OSError, ValueError):
                conflicts.append(path)
        setting_hashes = settings(home, data["settings"], old.get("settings", {}) if old else {}, shell, conflicts)
        new = {"schema": 1, "profile": name, "revision": entry["revision"], "digest": entry["sha256"],
               "files": managed, "settings": setting_hashes}
        home.write(STATE, (json.dumps(new, sort_keys=True, indent=2) + "\n").encode(), digest(old_raw) if old_raw is not None else None)
        if args.install_apps:
            for app in data["apps"]["flatpaks"]:
                try:
                    confined(home, [".config/flatpak", ".local/share/flatpak", ".cache/flatpak"], [
                        "/usr/bin/flatpak", "--user", "install", "--app", "--or-update", "--noninteractive", "--", app["remote"], app["id"],
                    ], network=True)
                except (OSError, ValueError, subprocess.TimeoutExpired):
                    conflicts.append(f"app {app['id']} (unavailable; retry explicitly)")
        print(json.dumps({"profile": name, "revision": entry["revision"], "conflicts": conflicts}))
        return 1 if conflicts else 0
    finally:
        if lock is not None:
            os.close(lock)
        os.close(home.fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("list")
    commands.add_parser("check")
    command = commands.add_parser("apply")
    selection = command.add_mutually_exclusive_group()
    selection.add_argument("name", nargs="?")
    selection.add_argument("--selected", action="store_true")
    command.add_argument("--shell", choices=["gnome"])
    command.add_argument("--install-apps", action="store_true", help="opt in to installing/updating user apps from an already configured flathub remote")
    args = parser.parse_args()
    if args.command in {"list", "check"}:
        data = catalogue()
        for entry in data["profiles"]:
            profile(entry["id"])
        print(json.dumps(data))
        return 0
    # Ordinary maintenance/system users have no selected profile. In
    # particular, don't create state directories just because user@ started.
    if args.selected and not os.path.lexists(os.path.join(pwd.getpwuid(os.getuid()).pw_dir, STATE)):
        if os.getuid() == 0:
            raise ValueError("profile apply must not run as root")
        return 0
    return apply(args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (ValueError, OSError, KeyError, TypeError, subprocess.TimeoutExpired) as error:
        print(f"ilex profile: {error}", file=sys.stderr)
        sys.exit(2)
