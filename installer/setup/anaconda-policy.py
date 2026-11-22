"""Installer-only checks with nonsecret diagnostics."""

import json
from pathlib import Path


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


def validate_maintenance(users):
    # Anaconda already holds the submitted account properties. Inspect only
    # whether a password was provided; never copy or format the value.
    if requested(selection()) and not any(
            "wheel" in user.groups and not user.lock and user.password for user in users):
        raise ValueError("Create a local maintenance administrator with a password.")


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
