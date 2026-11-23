"""Read the single bootable payload carried by the fixture's image."""
import hashlib
import json
from pathlib import Path

kernel, = (path for path in Path("/usr/lib/modules").glob("*/vmlinuz") if path.is_file())


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


print(json.dumps({
    "kernel_release": kernel.parent.name,
    "kernel_sha256": digest(kernel),
    "initramfs_sha256": digest(kernel.with_name("initramfs.img")),
}))
