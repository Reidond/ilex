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
import shutil
import sqlite3
import stat
import subprocess
import sys
import tarfile


VERSION = "2.0.0"
HEX = re.compile(r"[0-9a-f]{64}")
WRITE_ACCESS = (
    "fs:write-file,truncate,remove-dir,remove-file,make-char,make-dir,make-reg,"
    "make-sock,make-fifo,make-block,make-sym,refer"
)


def require(condition, message):
    if not condition:
        raise ValueError(message)


@contextlib.contextmanager
def regular_file(path, store):
    require(path.resolve(strict=True).is_relative_to(store), "store path escaped")
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as file:
        info = os.fstat(file.fileno())
        require(stat.S_ISREG(info.st_mode), f"non-regular store file: {path.name}")
        yield file, info


def file_identity(info):
    return info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns, info.st_ctime_ns


def backup_index(index, snapshot):
    wal, shm, journal = (index.with_name(index.name + suffix) for suffix in ("-wal", "-shm", "-journal"))
    require(not journal.exists(), "the cache index has a rollback journal")
    uri = index.absolute().as_uri() + "?mode=ro"
    if wal.exists() or shm.exists():
        # The Landlock wrapper also denies writes when SQLite opens an existing
        # WAL or tries to recreate a sidecar that disappeared after this check.
        require(wal.is_file() and shm.is_file(), "incomplete WAL sidecars; retry the export")
        with contextlib.closing(sqlite3.connect(uri, uri=True)) as live:
            live.backup(snapshot, pages=1024)
    else:
        before = file_identity(index.stat())
        with contextlib.closing(sqlite3.connect(uri + "&immutable=1", uri=True)) as closed:
            closed.backup(snapshot)
        require(file_identity(index.stat()) == before and
                not any(sidecar.exists() for sidecar in (wal, shm, journal)),
                "cache index changed during the immutable backup; retry the export")


def archive_entry(archive, name, data, size):
    info = tarfile.TarInfo(name)
    info.size = size
    info.mode = 0o644
    archive.addfile(info, data)


def blob_path(blob):
    return f"blobs/{blob[:2]}/{blob}"


@contextlib.contextmanager
def checksum_tool(hasher):
    if hasher == "b3sum":
        require(shutil.which(hasher), "b3sum unavailable; pass --b3sum-linux from Windows")
        yield hasher, ()
        return

    # A Windows staging volume can be noexec. Run the explicitly supplied
    # binary from an anonymous executable file instead of installing it.
    max_binary = 8 * 1024 * 1024
    with Path(hasher).open("rb") as source:
        binary = source.read(max_binary + 1)
    require(0 < len(binary) <= max_binary, "invalid b3sum binary size")
    flags = os.MFD_CLOEXEC | getattr(os, "MFD_EXEC", 0x10)
    with os.fdopen(os.memfd_create("ilex-b3sum", flags), "wb") as executable:
        executable.write(binary)
        executable.flush()
        fd = executable.fileno()
        yield f"/proc/self/fd/{fd}", (fd,)


def verified_blobs(store, blobs, command, inherited):
    verified = {}
    for blob, expected in blobs.items():
        with regular_file(store / blob_path(blob), store) as (file, info):
            if info.st_size != expected:
                print(f"Skipping wrong-size blob {blob}", file=sys.stderr)
                continue
            result = subprocess.run([command, "--no-names"], stdin=file, pass_fds=inherited,
                                    capture_output=True, check=True)
            if result.stdout.strip() != blob.encode():
                print(f"Skipping BLAKE3 mismatch for blob {blob}", file=sys.stderr)
                continue
            require(file_identity(os.fstat(file.fileno())) == file_identity(info),
                    f"cache blob changed while hashing: {blob}")
            verified[blob] = file_identity(info)
    return verified


def stream_snapshot(hasher):
    require(platform.system() == "Linux" and platform.machine() == "x86_64",
            "cache export needs x86_64 Linux")
    version = subprocess.check_output(["kache", "--version"], text=True).strip()
    require(version == f"kache {VERSION}", f"unsupported cache version: {version}")
    home_cache = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    root = Path(os.environ.get("KACHE_CACHE_DIR") or home_cache / "kache").expanduser()
    store = (root / "store").resolve(strict=True)
    # Keep both the backup and all subsequent index edits in memory.
    with contextlib.closing(sqlite3.connect(":memory:")) as snapshot:
        backup_index(root / "index.db", snapshot)
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
        with checksum_tool(hasher) as (command, inherited):
            verified = verified_blobs(store, blobs, command, inherited)
        omitted = [key for key, refs in entries.items() if not refs <= verified.keys()]
        if omitted:
            print(f"Skipping {len(omitted)} entries that reference damaged blobs", file=sys.stderr)
            snapshot.executemany("DELETE FROM entries WHERE cache_key = ?", ((key,) for key in omitted))
            for key in omitted:
                del entries[key]
        snapshot.executescript("""
            DELETE FROM entries WHERE committed != 1;
            DELETE FROM entry_blobs WHERE cache_key NOT IN (SELECT cache_key FROM entries);
            DELETE FROM blobs WHERE hash NOT IN (SELECT hash FROM entry_blobs);
            UPDATE blobs SET refcount = (SELECT SUM(refs) FROM entry_blobs WHERE hash = blobs.hash);
        """)
        index = snapshot.serialize()

    require(entries, "the kache index has no committed entries")
    with tarfile.open(mode="w|", fileobj=sys.stdout.buffer, bufsize=1024 * 1024) as archive:
        archive_entry(archive, "index.db", io.BytesIO(index), len(index))
        for key, refs in entries.items():
            require(HEX.fullmatch(key), "invalid committed cache key")
            relative = f"{key}/meta.json"
            with regular_file(store / relative, store) as (file, info):
                metadata = file.read()
            entry = json.loads(metadata)
            require(entry["cache_key"] == key and {f["hash"] for f in entry["files"]} == refs,
                    f"cache metadata changed after index backup: {key}")
            archive_entry(archive, f"store/{relative}", io.BytesIO(metadata), info.st_size)
        retained = set().union(*entries.values())
        for blob in sorted(retained):
            relative = blob_path(blob)
            with regular_file(store / relative, store) as (file, info):
                require(file_identity(info) == verified[blob], f"cache blob changed after verification: {blob}")
                archive_entry(archive, f"store/{relative}", file, info.st_size)
                require(file_identity(os.fstat(file.fileno())) == verified[blob],
                        f"cache blob changed while streaming: {blob}")
    print(f"{version}, x86_64-linux, {len(entries)} entries, {len(retained)} blobs",
          file=sys.stderr)


def export_windows(args):
    output = args.output.resolve()
    require(not output.exists(), "the destination already exists")
    partial = output.with_name(output.name + ".partial")
    request = [
        str(Path(os.environ["WINDIR"]) / "System32" / "wsl.exe"),
        "-d", args.distribution, "-u", args.user, "-e",
        "/usr/bin/setpriv", "--no-new-privs", "--landlock-access", WRITE_ACCESS,
        "--landlock-rule", "path-beneath:write-file:/dev/null",
        "--landlock-rule", "path-beneath:truncate:/dev/null",
        "/usr/bin/bash", "-lc",
        'if [ -n "$1" ]; then hasher=$(wslpath -a "$1") || exit; else hasher=b3sum; fi; '
        'exec /usr/bin/python3 -B - --stream "$hasher"',
        "ilex-cache-export", str(args.b3sum_linux.resolve(strict=True)) if args.b3sum_linux else "",
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
    if len(sys.argv) == 3 and sys.argv[1] == "--stream":
        stream_snapshot(sys.argv[2])
    else:
        require(os.name == "nt", "run the export from native Windows")
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("--distribution", default="FedoraLinux-45")
        parser.add_argument("--user", default="root")
        parser.add_argument("--b3sum-linux", type=Path, help="x86_64 Linux b3sum binary on Windows")
        parser.add_argument("--output", type=Path, required=True)
        export_windows(parser.parse_args())
