"""Installer-only checks with nonsecret diagnostics."""

import hmac
import json
import os
from pathlib import Path
import re


def selection():
    try:
        return json.loads(Path("/run/ilex-installer/setup.json").read_bytes())["profile"]
    except FileNotFoundError:
        return {}


def validate_users(users):
    reserved = selection().get("id")
    if reserved and any(user.name == reserved for user in users):
        raise ValueError(f"{reserved} is reserved for the daily home. "
                         "Choose a different maintenance username, such as ilexmaint.")


def requested(profile):
    return bool(profile.get("id")) or profile.get("mode") == "custom"


def password_hash(value):
    # Modular crypt encodings and the legacy DES encoding supported by libxcrypt.
    return re.fullmatch(r"\$[^:$\s]+\$(?:[^:$\s]+\$)*[./A-Za-z0-9]+|[./A-Za-z0-9]{13}", value)


def maintenance_users(users):
    return {user.name: user for user in users if "wheel" in user.groups
            and not user.lock and user.get_uid() != 0 and user.password
            and (not user.is_crypted or password_hash(user.password))
            and Path(user.shell or "/bin/bash").name not in {"false", "nologin"}}


def validate_maintenance(users):
    if requested(selection()):
        validate_users(users)
        if not maintenance_users(users):
            raise ValueError("Create a local maintenance administrator with a password and a login shell.")


def matches_password(user, stored):
    if not password_hash(stored):
        return False
    expected = user.password
    if not user.is_crypted:
        from pyanaconda.core.users import crypt_r
        try:
            expected = crypt_r.crypt(expected, stored)
        except (ValueError, OSError):
            return False
    return isinstance(expected, str) and hmac.compare_digest(expected, stored)


def validate_installed_maintenance(sysroot, users):
    if not requested(selection()):
        return
    candidates = maintenance_users(users)
    etc = Path(sysroot) / "etc"
    # Anaconda can log an account creation error and continue. Check the
    # installed account data without printing the records or password hashes.
    with (etc / "shadow").open() as shadow:
        unlocked = {fields[0] for line in shadow
                    if (fields := line.rstrip("\n").split(":"))[0] in candidates
                    and len(fields) == 9 and fields[7] in {"", "-1"}
                    and matches_password(candidates[fields[0]], fields[1])}
    with (etc / "group").open() as groups:
        wheel = next((fields for line in groups
                      if len(fields := line.rstrip("\n").split(":")) == 4
                      and fields[0] == "wheel"), None)
    if wheel:
        with (etc / "passwd").open() as passwd:
            for line in passwd:
                fields = line.rstrip("\n").split(":")
                if len(fields) != 7 or fields[0] not in unlocked or fields[2] == "0":
                    continue
                shell = Path(fields[6])
                if not shell.is_absolute() or shell.name in {"false", "nologin"}:
                    continue
                installed_shell = Path(sysroot) / shell.relative_to("/")
                if (fields[3] == wheel[2] or fields[0] in wheel[3].split(",")) and (
                        installed_shell.is_file() and os.access(installed_shell, os.X_OK)):
                    return
    raise ValueError("Installation could not create a usable local maintenance administrator.")


def validate_storage(storage, root, report_error):
    if root is None or not requested(selection()):
        return

    def encrypted(device):
        return any(parent.format.type == "luks" and parent.format.luks_version == "luks2"
                   for parent in device.ancestors)

    protected = [root, *storage.fsset.swap_devices]
    protected.extend(device for mount, device in storage.mountpoints.items()
                     if mount == "/home" or mount.startswith(("/home/", "/var/")) or mount == "/var")
    if not all(map(encrypted, protected)):
        report_error("Enable LUKS2 disk encryption for the system, home and on-disk swap "
                     "before installing with daily home setup.")
