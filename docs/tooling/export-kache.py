"""Export a kache 2.0.0 store from WSL using Windows Python 3.11 or newer."""

import argparse
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import platform
import re
import sqlite3
import stat
import subprocess
import sys
import tarfile


VERSION = "2.0.0"
HEX = re.compile(r"[0-9a-f]{64}")


def require(condition, message):
    if not condition:
        raise ValueError(message)


@contextlib.contextmanager
def regular_file(path, store):
    require(path.resolve(strict=True).is_relative_to(store), "store path escaped")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as file:
        info = os.fstat(file.fileno())
        require(stat.S_ISREG(info.st_mode), f"non-regular store file: {path.name}")
        yield file, info.st_size


def archive_entry(archive, name, data, size):
    info = tarfile.TarInfo(name)
    info.size = size
    info.mode = 0o644
    archive.addfile(info, data)


def stream_snapshot():
    require(platform.system() == "Linux" and platform.machine() == "x86_64",
            "cache export needs x86_64 Linux")
    version = subprocess.check_output(["kache", "--version"], text=True).strip()
    require(version == f"kache {VERSION}", f"unsupported cache version: {version}")
    home_cache = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    root = Path(os.environ.get("KACHE_CACHE_DIR") or home_cache / "kache").expanduser()
    store = (root / "store").resolve(strict=True)
    # The online backup reads committed pages including WAL changes. Keep its
    # destination in memory; the live connection is read-only and closes first.
    with contextlib.closing(sqlite3.connect(":memory:")) as snapshot:
        with contextlib.closing(sqlite3.connect(
            (root / "index.db").absolute().as_uri() + "?mode=ro", uri=True
        )) as live:
            live.backup(snapshot, pages=1024)
        require(snapshot.execute("PRAGMA quick_check").fetchone() == ("ok",),
                "inconsistent kache index backup")
        entries = {key: set() for key, in snapshot.execute(
            "SELECT cache_key FROM entries WHERE committed = 1 ORDER BY cache_key"
        )}
        blobs = {}
        for key, blob, size in snapshot.execute("""
            SELECT refs.cache_key, refs.hash, blobs.size
            FROM entry_blobs AS refs
            JOIN entries ON entries.cache_key = refs.cache_key AND entries.committed = 1
            LEFT JOIN blobs ON blobs.hash = refs.hash
        """):
            require(HEX.fullmatch(blob) and isinstance(size, int) and size >= 0,
                    "invalid blob reference in the kache index")
            entries[key].add(blob)
            blobs[blob] = size
        index = snapshot.serialize()

    require(entries, "the kache index has no committed entries")
    with tarfile.open(mode="w|", fileobj=sys.stdout.buffer, bufsize=1024 * 1024) as archive:
        archive_entry(archive, "index.db", io.BytesIO(index), len(index))
        for key, refs in entries.items():
            require(HEX.fullmatch(key), "invalid committed cache key")
            relative = f"{key}/meta.json"
            with regular_file(store / relative, store) as (file, size):
                metadata = file.read()
            entry = json.loads(metadata)
            require(entry["cache_key"] == key and {f["hash"] for f in entry["files"]} == refs,
                    f"cache metadata changed after index backup: {key}")
            archive_entry(archive, f"store/{relative}", io.BytesIO(metadata), size)
        for blob, expected in sorted(blobs.items()):
            relative = f"blobs/{blob[:2]}/{blob}"
            with regular_file(store / relative, store) as (file, size):
                require(size == expected, f"blob size changed after index backup: {blob}")
                archive_entry(archive, f"store/{relative}", file, size)
    print(f"{version}, x86_64-linux, {len(entries)} entries, {len(blobs)} blobs",
          file=sys.stderr)


def export_windows(args):
    output = args.output.resolve()
    require(not output.exists(), "the destination already exists")
    partial = output.with_name(output.name + ".partial")
    request = [
        str(Path(os.environ["WINDIR"]) / "System32" / "wsl.exe"),
        "-d", args.distribution, "-u", args.user, "-e", "/usr/bin/bash", "-lc",
        "exec /usr/bin/python3 -B - --stream",
    ]
    source = Path(__file__).read_bytes()
    checksum = hashlib.sha256()
    size = 0
    with partial.open("xb") as destination:
        try:
            with subprocess.Popen(request, stdin=subprocess.PIPE, stdout=subprocess.PIPE) as child:
                child.stdin.write(source)
                child.stdin.close()
                while block := child.stdout.read(4 * 1024 * 1024):
                    destination.write(block)
                    checksum.update(block)
                    size += len(block)
                require(child.wait() == 0 and size > 0, "kache export failed")
        except BaseException:
            destination.close()
            partial.unlink()
            raise
    partial.rename(output)
    print(json.dumps({"path": str(output), "version": VERSION, "target": "x86_64-linux",
                      "bytes": size, "sha256": f"sha256:{checksum.hexdigest()}"}))


if __name__ == "__main__":
    if sys.argv[1:] == ["--stream"]:
        stream_snapshot()
    else:
        require(os.name == "nt", "run the export from native Windows")
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--distribution", default="FedoraLinux-45")
        parser.add_argument("--user", default="root")
        parser.add_argument("--output", type=Path, required=True)
        export_windows(parser.parse_args())
