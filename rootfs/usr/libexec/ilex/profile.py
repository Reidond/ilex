"""Apply public image profiles only as the owner of an authenticated homed home.

No hooks, record imports, chown, arbitrary settings paths or shell evaluation.
Profile content is untrusted; existing links/redirects are rejected or neutralised.
Home traversal uses pinned, owned, no-follow directory FDs. A concurrently
malicious process of the same UID already has the user's full write access and
is outside the prevention threat model. Writer replacement is detected, not
prevented by an authenticated writer protocol. Ordinary concurrent file saves
are checked at exchange; settings updates are best-effort because dconf has no CAS.
"""

import argparse
from contextlib import ExitStack, contextmanager
import ctypes
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
RENAME_EXCHANGE = 2
_renameat2 = ctypes.CDLL(None, use_errno=True).renameat2
_renameat2.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint]
_renameat2.restype = ctypes.c_int


def exchange(fd, source, destination):
    if _renameat2(fd, os.fsencode(source), fd, os.fsencode(destination), RENAME_EXCHANGE):
        error = ctypes.get_errno()
        raise OSError(error, os.strerror(error), destination)


class FileConflict(ValueError):
    def __init__(self, recovery=None):
        self.recovery = recovery
        super().__init__("file changed while applying profile" +
                         (f"; displaced edits retained at {recovery}" if recovery else ""))


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

    def read_at(self, fd, name):
        source = os.open(name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
        with os.fdopen(source, "rb") as stream:
            self.owned(stream.fileno())
            value = stream.read(1048577)
            if len(value) > 1048576:
                raise ValueError("profile file is too large")
            return value

    def read(self, path):
        try:
            with self.parent(path) as (fd, name):
                return self.read_at(fd, name)
        except FileNotFoundError:
            return None

    def write(self, path, data, expected):
        with self.parent(path, create=True) as (fd, name):
            temporary = f".ilex-profile-{uuid.uuid4().hex}"
            cleanup = True
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
                    raise FileConflict()
                if expected is None:
                    os.link(temporary, name, src_dir_fd=fd, dst_dir_fd=fd, follow_symlinks=False)
                else:
                    # Exchange is the replacement point: inspect the version
                    # actually displaced, not just an earlier pathname read.
                    exchange(fd, temporary, name)
                    cleanup = False  # Never discard an unverified displaced file.
                    try:
                        matches = digest(self.read_at(fd, temporary)) == expected
                    except (OSError, ValueError):
                        matches = False
                    if not matches:
                        recovery = "/".join([*parts(path)[:-1], temporary])
                        try:
                            exchange(fd, temporary, name)
                        except OSError:
                            raise FileConflict(recovery) from None
                        # Another save during rollback must survive too. Keep
                        # that version at the recovery name instead of unlinking.
                        try:
                            cleanup = digest(self.read_at(fd, temporary)) == digest(data)
                        except (OSError, ValueError):
                            cleanup = False
                        os.fsync(fd)
                        raise FileConflict(None if cleanup else recovery)
                    cleanup = True
                os.fsync(fd)
            finally:
                if cleanup:
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


def confined(home, directories, command, network=False, runtime=None, replace=False):
    """External writers see only pinned home subdirectories as writable.

    A path check alone cannot constrain the session's existing dconf service.
    Mask homes/runtime and clear inherited XDG/dconf/D-Bus redirects.
    The normal writer's filesystem access is anchored here. This does not
    prevent a malicious same-UID process from replacing that writer or using
    the user's IPC services to write elsewhere (see the module threat model).
    """
    with ExitStack() as opened:
        arguments = ["/usr/bin/bwrap", "--die-with-parent", "--unshare-all"]
        if network:
            arguments.append("--share-net")
        arguments += ["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
                      "--tmpfs", "/tmp", "--tmpfs", "/run", "--tmpfs", "/var/home",
                      "--perms", "0700", "--dir", home.path,
                      "--perms", "0700", "--dir", f"/run/user/{home.owner}", "--clearenv"]
        for key, value in {"PATH": "/usr/bin:/usr/sbin", "HOME": home.path,
                           "XDG_CONFIG_HOME": home.path + "/.config",
                           "XDG_DATA_HOME": home.path + "/.local/share",
                           "XDG_CACHE_HOME": home.path + "/.cache",
                           "XDG_RUNTIME_DIR": f"/run/user/{home.owner}",
                           "GSETTINGS_BACKEND": "dconf",
                           "DCONF_PROFILE": "/usr/libexec/ilex/dconf-profile"}.items():
            arguments += ["--setenv", key, value]
        descriptors = []
        if network:
            resolver = opened.enter_context(open("/etc/resolv.conf", "rb"))
            descriptors.append(resolver.fileno())
            arguments += ["--ro-bind-fd", str(resolver.fileno()), "/etc/resolv.conf"]
        if runtime is not None:
            descriptors.append(runtime.fd)
            arguments += ["--bind-fd", str(runtime.fd), runtime.path,
                          "--setenv", "DBUS_SESSION_BUS_ADDRESS", f"unix:path={runtime.path}/bus"]
        for directory in directories:
            fd, _ = opened.enter_context(home.parent(directory + "/.anchor", create=True))
            descriptors.append(fd)
            arguments += ["--dir", home.path + "/" + directory,
                          "--bind-fd", str(fd), home.path + "/" + directory]
        arguments += ["--chdir", home.path, "--", *command]
        if replace:
            for fd in descriptors:
                os.set_inheritable(fd, True)
            os.execv(arguments[0], arguments)
        return run(*arguments, pass_fds=descriptors)


def dconf_service():
    """The actual session writer, confined for homed users, with normal IPC."""
    account = pwd.getpwuid(os.getuid())
    if account.pw_uid == 0:
        raise ValueError("dconf profile writer must run as its home user")
    # Local maintenance accounts retain Fedora's writer. They cannot apply a
    # public profile. Never fall back after a homed authentication/path error.
    for line in read_image("/etc/passwd").decode().splitlines():
        fields = line.split(":")
        if len(fields) == 7 and fields[0] == account.pw_name and fields[2] == str(account.pw_uid):
            os.execv("/usr/libexec/dconf-service", ["/usr/libexec/dconf-service"])
    home = active_home()
    runtime = Home(f"/run/user/{home.owner}", home.owner)
    try:
        home.read(".config/dconf/user")
        confined(home, [".config/dconf"], ["/usr/libexec/dconf-service"], runtime=runtime, replace=True)
    finally:
        os.close(runtime.fd)
        os.close(home.fd)


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


def settings_worker(data, previous, conflicts, environment):
    managed = dict(previous)
    for key, value in data.get("gnome", {}).items():
        if key not in {"color-scheme", "clock-show-weekday", "clock-format"}:
            raise ValueError("unsupported GNOME setting")
        variant = ("true" if value else "false") if type(value) is bool else f"'{value}'"
        path = f"/org/gnome/desktop/interface/{key}"
        try:
            current = run("/usr/bin/dconf", "read", path, env=environment)
            if (key in previous and current != previous[key]) or (key not in previous and current):
                conflicts.append(f"settings.gnome.{key}")
                continue
            if key in previous and variant == previous[key]:
                continue
            # Desktop clients do not take our profile lock. Re-read next to
            # the write: an intervening reset/save needs no restoration if we
            # leave it untouched. No dconf CAS exists for the remaining window.
            before_write = run("/usr/bin/dconf", "read", path, env=environment)
            if before_write != current:
                conflicts.append(f"settings.gnome.{key} (changed before write)")
                continue
            run("/usr/bin/gsettings", "set", "org.gnome.desktop.interface", key, variant, env=environment)
            after_write = run("/usr/bin/dconf", "read", path, env=environment)
            if after_write != variant:
                # A later user value wins; never overwrite it with our rollback.
                conflicts.append(f"settings.gnome.{key} (changed during write)")
                continue
            managed[key] = variant
        except (ValueError, OSError, subprocess.TimeoutExpired):
            conflicts.append(f"settings.gnome.{key} (unavailable; retry after login)")
    return managed


def writer_identity(home, environment, start=False):
    home.read(".config/dconf/user")
    def unit_value(property_name):
        return run("/usr/bin/systemctl", "--user", "show", "dconf.service", "--value",
                   "--property=" + property_name, env=environment)
    dropins = unit_value("DropInPaths").split()
    if (unit_value("FragmentPath") != "/usr/lib/systemd/user/dconf.service"
        or "/usr/lib/systemd/user/dconf.service.d/50-ilex-home.conf" not in dropins
        or any(not path.startswith("/usr/lib/systemd/user/") for path in dropins)
        or not unit_value("ExecStart").startswith("{ path=/usr/bin/python3 ; argv[]=/usr/bin/python3 -I -S /usr/libexec/ilex/dconf-service.py ; ")):
        raise ValueError("dconf writer activation is overridden")
    if start:
        run("/usr/bin/systemctl", "--user", "start", "dconf.service", env=environment)
    bus = ("/usr/bin/busctl", "--user", "call", "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus")
    bus_id = run(*bus, "GetId", env=environment)
    connection = run(*bus, "GetNameOwner", "s", "ca.desrt.dconf", env=environment)
    if not re.fullmatch(r's ":[0-9]+\.[0-9]+"', connection):
        raise ValueError("cannot identify dconf connection")
    unique = json.loads(connection[2:])
    owner = run(*bus, "GetConnectionUnixProcessID", "s", unique, env=environment)
    if not re.fullmatch(r"u [1-9][0-9]*", owner):
        raise ValueError("cannot identify dconf writer")
    group = unit_value("ControlGroup")
    membership = read_image(f"/proc/{owner.split()[1]}/cgroup").decode().splitlines()
    if not group or not any(line.split(":", 2)[-1] == group for line in membership):
        raise ValueError("session dconf writer is outside the confined service")
    return bus_id, unique, owner, group


def settings(home, data, previous, shell, conflicts):
    if shell != "gnome" or not data.get("gnome"):
        return dict(previous)
    try:
        environment = {"HOME": home.path, "XDG_CONFIG_HOME": home.path + "/.config",
                       "XDG_RUNTIME_DIR": f"/run/user/{home.owner}", "GSETTINGS_BACKEND": "dconf",
                       "DCONF_PROFILE": "/usr/libexec/ilex/dconf-profile",
                       "DBUS_SESSION_BUS_ADDRESS": f"unix:path=/run/user/{home.owner}/bus"}
        before = writer_identity(home, environment, start=True)
        managed = settings_worker(data, previous, conflicts, environment)
        # Detection, not prevention against an already fully privileged same-UID
        # process. Do not claim success or adopt managed values after a switch.
        try:
            if writer_identity(home, environment) != before:
                raise ValueError("writer identity changed")
        except (ValueError, OSError, subprocess.TimeoutExpired):
            conflicts.append("settings.gnome (writer or bus changed during apply)")
            return dict(previous)
        return managed
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
            except FileConflict as error:
                conflicts.append(f"{path} ({error})")
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
