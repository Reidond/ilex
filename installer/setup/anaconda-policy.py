"""Installer-only checks. Neither callback reads any credential field."""

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


def validate_storage(storage, root, report_error):
    profile = selection()
    if root is None or (not profile.get("id") and profile.get("mode") != "custom"):
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
