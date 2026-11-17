"""No-follow build-time file installation, shared by image and machine layers.

Overlays only add entries. Generated files live in a separate reserved tree;
their registry permits a later layer to replace only unchanged generated files.
All destination traversal uses directory descriptors and O_NOFOLLOW.
"""

import argparse
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import sys
import uuid

RESERVED = "usr/share/ilex"
REGISTRY = f"{RESERVED}/.generated.json"
PROVENANCE = f"{RESERVED}/.overlays.json"
DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


def error(label, path, message):
    raise ValueError(f"{label}: /{path}: {message}")


def parts(path):
    if not isinstance(path, str) or not path or path.startswith("/"):
        raise ValueError(f"invalid relative image path {path!r}")
    components = path.split("/")
    if any(p in ("", ".", "..") or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in p) for p in components):
        raise ValueError(f"invalid relative image path {path!r}")
    return components


def reserved(path):
    return path == RESERVED or path.startswith(RESERVED + "/")


class Tree:
    def __init__(self, root, label, create=False):
        self.label = label
        fd = os.open("/", DIR_FLAGS)
        try:
            for component in Path(os.path.abspath(root)).parts[1:]:
                if create:
                    try:
                        os.mkdir(component, 0o755, dir_fd=fd)
                    except FileExistsError:
                        pass
                next_fd = os.open(component, DIR_FLAGS, dir_fd=fd)
                os.close(fd)
                fd = next_fd
            self.fd = fd
        except OSError as exc:
            os.close(fd)
            raise ValueError(f"{label}: {root}: root ancestors must be real directories: {exc}") from exc

    def __enter__(self):
        return self

    def __exit__(self, *args):
        os.close(self.fd)

    @contextmanager
    def directory(self, components, path, create=False):
        fd = os.dup(self.fd)
        try:
            for index, component in enumerate(components):
                if create:
                    try:
                        os.mkdir(component, 0o755, dir_fd=fd)
                    except FileExistsError:
                        pass
                try:
                    child = os.open(component, DIR_FLAGS, dir_fd=fd)
                except FileNotFoundError:
                    raise
                except OSError as exc:
                    ancestor = "/".join(components[:index + 1])
                    error(self.label, path, f"ancestor /{ancestor} must be a real directory (no symlinks): {exc}")
                os.close(fd)
                fd = child
            yield fd
        finally:
            os.close(fd)

    @contextmanager
    def parent(self, path, create=False):
        components = parts(path)
        with self.directory(components[:-1], path, create) as fd:
            yield fd, components[-1]

    def entries(self, prefix=""):
        components = parts(prefix) if prefix else []
        with self.directory(components, prefix) as fd:
            for name in sorted(os.listdir(fd)):
                path = f"{prefix}/{name}" if prefix else name
                parts(path)
                mode = os.stat(name, dir_fd=fd, follow_symlinks=False).st_mode
                yield path, mode
                if stat.S_ISDIR(mode):
                    yield from self.entries(path)

    def read(self, path, optional=False, max_bytes=None):
        try:
            with self.parent(path) as (fd, name):
                try:
                    source = os.open(name, os.O_RDONLY | os.O_NONBLOCK | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
                except FileNotFoundError:
                    raise
                except OSError as exc:
                    error(self.label, path, f"cannot read a regular file without following symlinks: {exc}")
                with os.fdopen(source, "rb") as stream:
                    meta = os.fstat(stream.fileno())
                    if not stat.S_ISREG(meta.st_mode):
                        error(self.label, path, "must be a regular file")
                    if max_bytes is None:
                        return stream.read()
                    if meta.st_size > max_bytes:
                        error(self.label, path, f"must be at most {max_bytes} bytes")
                    value = stream.read(max_bytes + 1)
                    if len(value) > max_bytes:
                        error(self.label, path, f"must be at most {max_bytes} bytes")
                    return value
        except FileNotFoundError:
            if optional:
                return None
            raise

    def require_absent(self, path):
        try:
            with self.parent(path) as (fd, name):
                os.stat(name, dir_fd=fd, follow_symlinks=False)
        except FileNotFoundError:
            return
        error(self.label, path, "must be absent before a base install; reserved for the composer")

    def write(self, path, data, mode=0o644, previous_hash=None):
        with self.parent(path, create=True) as (fd, name):
            existing = None
            try:
                existing = os.stat(name, dir_fd=fd, follow_symlinks=False)
            except FileNotFoundError:
                pass
            if existing is not None:
                if not stat.S_ISREG(existing.st_mode):
                    error(self.label, path, "generated destination must be a regular file (no symlinks)")
                if previous_hash is None:
                    error(self.label, path, "already exists and is not a registered generated file")
                if hashlib.sha256(self.read(path)).hexdigest() != previous_hash:
                    error(self.label, path, "existing generated file differs from its registry")
            temporary = f".ilex-{uuid.uuid4().hex}"
            try:
                target = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=fd)
                with os.fdopen(target, "wb") as stream:
                    stream.write(data)
                    os.fchmod(stream.fileno(), mode)
                # rename replaces a leaf entry itself, never its symlink target.
                os.rename(temporary, name, src_dir_fd=fd, dst_dir_fd=fd)
            finally:
                try:
                    os.unlink(temporary, dir_fd=fd)
                except FileNotFoundError:
                    pass

    def add_overlay(self, source, label, staging_path=None):
        with Tree(source, label) as tree:
            entries = list(tree.entries())
            # Report the offending leaf, including the tokens.json exploit path.
            for path, mode in sorted(entries, key=lambda entry: stat.S_ISDIR(entry[1])):
                if reserved(path):
                    error(label, path, f"/{RESERVED} is reserved for generated files")
                if path in ("etc/ld.so.preload", "etc/ld.so.conf", "etc/ld.so.conf.d") or path.startswith("etc/ld.so.conf.d/"):
                    error(label, path, "dynamic-loader configuration is forbidden in overlays")
                if staging_path and (path == staging_path or path.startswith(staging_path + "/")):
                    error(label, path, "cannot write into the temporary installation staging")
                if not (stat.S_ISDIR(mode) or stat.S_ISREG(mode) or stat.S_ISLNK(mode)):
                    error(label, path, "only directories, regular files and leaf symlinks are supported")
            previous_label, self.label = self.label, label
            try:
                created_directories = []
                added = {}
                for path, mode in entries:
                    with self.parent(path, create=True) as (fd, name):
                        if stat.S_ISDIR(mode):
                            try:
                                os.mkdir(name, stat.S_IMODE(mode), dir_fd=fd)
                            except FileExistsError:
                                if not stat.S_ISDIR(os.stat(name, dir_fd=fd, follow_symlinks=False).st_mode):
                                    error(label, path, "already exists and is not a real directory (no symlinks)")
                            else:
                                created_directories.append((path, stat.S_IMODE(mode)))
                                added[path] = None
                            # Shared directories retain the image's mode/owner.
                        elif stat.S_ISLNK(mode):
                            with tree.parent(path) as (source_fd, source_name):
                                target = os.readlink(source_name, dir_fd=source_fd)
                            try:
                                os.symlink(target, name, dir_fd=fd)
                            except FileExistsError:
                                error(label, path, "already exists; overlays only add files")
                        else:
                            try:
                                target = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=fd)
                            except FileExistsError:
                                error(label, path, "already exists; overlays only add files")
                            with os.fdopen(target, "wb") as stream:
                                stream.write(tree.read(path))
                                os.fchmod(stream.fileno(), stat.S_IMODE(mode))
                        if not stat.S_ISDIR(mode):
                            added[path] = None
                for path, mode in reversed(created_directories):
                    with self.directory(parts(path), path) as fd:
                        os.fchmod(fd, mode)
                return {"name": label, "entries": {path: self.describe(path) for path in added}}
            finally:
                self.label = previous_label

    def describe(self, path):
        with self.parent(path) as (fd, name):
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            result = {"mode": info.st_mode, "uid": info.st_uid, "gid": info.st_gid}
            if stat.S_ISREG(info.st_mode):
                result["sha256"] = hashlib.sha256(self.read(path)).hexdigest()
            elif stat.S_ISLNK(info.st_mode):
                result["target"] = os.readlink(name, dir_fd=fd)
            elif not stat.S_ISDIR(info.st_mode):
                error(self.label, path, "unsupported inherited overlay entry")
            return result


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def registry(image):
    data = image.read(REGISTRY, optional=True)
    entries = {} if data is None else json.loads(data)
    if not isinstance(entries, dict):
        error("generated files", REGISTRY, "invalid generated-file registry")
    for path, digest in entries.items():
        parts(path)
        if not reserved(path) or path == REGISTRY or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
            error("generated files", REGISTRY, f"invalid generated-file registry entry {path!r}")
        if hashlib.sha256(image.read(path)).hexdigest() != digest:
            error("generated files", path, "existing generated file differs from its registry")
    return entries, data


def record(image, path, data):
    entries, previous = registry(image)
    image.write(path, data, previous_hash=entries.get(path))
    entries[path] = hashlib.sha256(data).hexdigest()
    image.write(REGISTRY, json_bytes(entries), previous_hash=(hashlib.sha256(previous).hexdigest() if previous else None))


def prepare(root, replay):
    """Reconstruct the trusted pre-overlay tree before launching dnf/scriptlets.

    Verify everything first. Only recorded additions may be removed; rmdir
    refuses unexpected children. The replay payload is outside package cleanup.
    """
    with Tree(root, "inherited overlays") as image:
        registered, _ = registry(image)
        machine_path = f"{RESERVED}/machine.json"
        if machine_path not in registered:
            error("machine base", machine_path, "must be an unchanged registered generated file")
        machine = json.loads(image.read(machine_path))
        if machine.get("schema") != 1 or machine.get("name") != "generic":
            error("machine base", machine_path, f"base machine is {machine.get('name')!r}; machine layers require generic; machines do not stack")
        if PROVENANCE not in registered:
            error("inherited overlays", PROVENANCE, "missing registered overlay provenance; rebuild the base with cargo xtask build")
        overlays = json.loads(image.read(PROVENANCE))
        seen = set()
        for overlay in overlays:
            for path, expected in overlay["entries"].items():
                parts(path)
                if reserved(path) or path in seen:
                    error(overlay["name"], path, "invalid inherited overlay provenance")
                seen.add(path)
                if image.describe(path) != expected:
                    error(overlay["name"], path, "inherited overlay differs from its provenance")
        with Tree(replay, "inherited overlay staging", create=True) as staged:
            staged.require_absent("overlays.json")
            manifests = []
            for index, overlay in enumerate(overlays):
                directory = f"overlays/{index:03d}"
                with Tree(replay / directory, overlay["name"], create=True) as saved:
                    for path, info in overlay["entries"].items():
                        mode = info["mode"]
                        if stat.S_ISDIR(mode):
                            with saved.directory(parts(path), path, create=True) as fd:
                                os.fchmod(fd, stat.S_IMODE(mode))
                        elif stat.S_ISREG(mode):
                            saved.write(path, image.read(path), stat.S_IMODE(mode))
                        else:
                            with saved.parent(path, create=True) as (fd, name):
                                os.symlink(info["target"], name, dir_fd=fd)
                manifests.append({"name": overlay["name"], "directory": directory})
            staged.write("overlays.json", json_bytes(manifests))
        for overlay in reversed(overlays):
            for path, info in reversed(list(overlay["entries"].items())):
                with image.parent(path) as (fd, name):
                    (os.rmdir if stat.S_ISDIR(info["mode"]) else os.unlink)(name, dir_fd=fd)


def install(payload, root, phase="all", base=False, remove_payload=False, inherited=None):
    try:
        staging_path = Path(os.path.abspath(payload)).relative_to(Path(os.path.abspath(root))).as_posix()
    except ValueError:
        staging_path = None
    with Tree(root, "generated files") as image:
        if phase != "overlays":
            if base:
                image.require_absent(RESERVED)
            registered, _ = registry(image)
            with Tree(payload / "generated", "generated files") as generated:
                for path, mode in generated.entries():
                    if stat.S_ISDIR(mode):
                        continue
                    if not reserved(path) or path == REGISTRY or not stat.S_ISREG(mode):
                        error("generated files", path, "must be a regular file below the reserved directory")
                    data = generated.read(path)
                    if not base and path == f"{RESERVED}/machine.json" and path not in registered:
                        error("generated files", path, "machine replacement requires an unchanged registered generated file")
                    record(image, path, data)
        if phase != "generated":
            provenance = []
            for source in ([inherited] if inherited else []) + [payload]:
                with Tree(source, "overlay staging") as staged:
                    overlays = json.loads(staged.read("overlays.json"))
                for overlay in overlays:
                    directory = overlay["directory"]
                    parts(directory)
                    provenance.append(image.add_overlay(source / directory, overlay["name"], staging_path))
            record(image, PROVENANCE, json_bytes(provenance))
    if remove_payload:
        # Do this in the already-running, site-disabled interpreter: no new
        # command may execute overlay content between installation and lint.
        shutil.rmtree(payload)
        if inherited:
            shutil.rmtree(inherited)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=Path("/"))
    parser.add_argument("--phase", choices=("all", "generated", "overlays"), default="all")
    parser.add_argument("--base", action="store_true", help="require the reserved generated directory to be absent")
    parser.add_argument("--remove-payload", action="store_true")
    parser.add_argument("--prepare", action="store_true")
    parser.add_argument("--inherited", type=Path)
    args = parser.parse_args()
    if args.base and args.phase == "overlays":
        parser.error("--base requires generated-file installation")
    try:
        if args.prepare:
            prepare(args.root, args.payload)
        else:
            install(args.payload, args.root, args.phase, args.base, args.remove_payload, args.inherited)
    except (OSError, ValueError, KeyError) as exc:
        sys.exit(f"install-files: {exc}")
