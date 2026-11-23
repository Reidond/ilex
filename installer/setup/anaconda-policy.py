"""Installer-only checks with nonsecret diagnostics."""

import hmac
import json
import os
from pathlib import Path
import re


CRYPT_ALPHABET = "./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
# The final encoded digit carries two bits for SHA-512 or four for yescrypt.
SHA512_CRYPT = re.compile(r"\$6\$(?:rounds=[1-9][0-9]{3,8}\$)?[./A-Za-z0-9]{1,16}\$[./A-Za-z0-9]{85}[./01]")
# Standard libxcrypt costs 1–11 include Anaconda's j9T setting.
YESCRYPT = re.compile(r"\$y\$j(?:[78]5|[7-9A-F]T)\$([./A-Za-z0-9]{2,86})\$[./A-Za-z0-9]{42}[./0-9A-D]")


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
    if SHA512_CRYPT.fullmatch(value):
        return True
    match = YESCRYPT.fullmatch(value)
    if match is None:
        return False
    salt = match[1]
    # Final groups of 0, 1, 2 or 3 characters allow 6, 0, 2 or 4
    # bits in the last digit. A lone character cannot encode a byte.
    return CRYPT_ALPHABET.index(salt[-1]) < (64, 0, 4, 16)[len(salt) % 4]


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
