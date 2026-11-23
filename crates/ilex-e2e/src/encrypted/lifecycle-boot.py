"""Hash the running OSTree boot entry's deployed payloads."""
import hashlib
import json
import os
import shlex
from pathlib import Path

cmdline = shlex.split(Path("/proc/cmdline").read_text())
ostree, = (arg for arg in cmdline if arg.startswith("ostree="))
boot = Path("/boot")
entries = []
for entry in sorted((boot / "loader/entries").glob("*.conf")):
    fields = {}
    for line in entry.read_text().splitlines():
        parts = line.split(maxsplit=1)
        if len(parts) == 2:
            fields.setdefault(parts[0], []).append(parts[1])
    if any(ostree in shlex.split(options) for options in fields.get("options", [])):
        entries.append(fields)

entry, = entries
linux, = entry["linux"]
release = os.uname().release
assert Path(linux).name == "vmlinuz-" + release
assert entry["initrd"], "boot entry has no initramfs"


def digest(path):
    assert path.startswith("/") and not path.startswith("//")
    resolved = (boot / path[1:]).resolve(strict=True)
    assert resolved.is_relative_to(boot)
    with resolved.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


# OSTree's Fedora entry has one initramfs. If the boot entry grows multiple
# initrd lines, still identify and hash the one built for the running kernel.
initramfs, = (path for path in entry["initrd"]
              if Path(path).name in ("initramfs-" + release + ".img", "initramfs-" + release))
print(json.dumps({
    "kernel_release": release,
    "kernel_sha256": digest(linux),
    "initramfs_sha256": digest(initramfs),
}))
