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

    def read(self, path, optional=False):
        try:
            with self.parent(path) as (fd, name):
                try:
                    source = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
                except FileNotFoundError:
                    raise
                except OSError as exc:
                    error(self.label, path, f"cannot read a regular file without following symlinks: {exc}")
                with os.fdopen(source, "rb") as stream:
                    if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                        error(self.label, path, "must be a regular file")
                    return stream.read()
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
                for path, mode in reversed(created_directories):
                    with self.directory(parts(path), path) as fd:
                        os.fchmod(fd, mode)
            finally:
                self.label = previous_label


def json_bytes(value):
    return (json.dumps(value, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def install(payload, root, phase="all", base=False, remove_payload=False):
    try:
        staging_path = Path(os.path.abspath(payload)).relative_to(Path(os.path.abspath(root))).as_posix()
    except ValueError:
        staging_path = None
    with Tree(root, "generated files") as image:
        if phase != "overlays":
            if base:
                image.require_absent(RESERVED)
            registry_bytes = image.read(REGISTRY, optional=True)
            registry = {} if registry_bytes is None else json.loads(registry_bytes)
            if not isinstance(registry, dict):
                error("generated files", REGISTRY, "invalid generated-file registry")
            for path, digest in registry.items():
                try:
                    parts(path)
                except ValueError as exc:
                    error("generated files", REGISTRY, f"invalid registry key: {exc}")
                if not reserved(path) or path == REGISTRY or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest):
                    error("generated files", REGISTRY, f"invalid generated-file registry entry {path!r}")
            with Tree(payload / "generated", "generated files") as generated:
                for path, mode in generated.entries():
                    if stat.S_ISDIR(mode):
                        continue
                    if not reserved(path) or path == REGISTRY or not stat.S_ISREG(mode):
                        error("generated files", path, "must be a regular file below the reserved directory")
                    data = generated.read(path)
                    image.write(path, data, previous_hash=registry.get(path))
                    registry[path] = hashlib.sha256(data).hexdigest()
            image.write(REGISTRY, json_bytes(registry), previous_hash=(
                hashlib.sha256(registry_bytes).hexdigest() if registry_bytes is not None else None
            ))
        if phase != "generated":
            with Tree(payload, "overlay staging") as staged:
                overlays = json.loads(staged.read("overlays.json"))
            for overlay in overlays:
                directory = overlay["directory"]
                parts(directory)
                image.add_overlay(payload / directory, overlay["name"], staging_path)
    if remove_payload:
        # Do this in the already-running, site-disabled interpreter: no new
        # command may execute overlay content between installation and lint.
        shutil.rmtree(payload)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--payload", type=Path, required=True)
    parser.add_argument("--root", type=Path, default=Path("/"))
    parser.add_argument("--phase", choices=("all", "generated", "overlays"), default="all")
    parser.add_argument("--base", action="store_true", help="require the reserved generated directory to be absent")
    parser.add_argument("--remove-payload", action="store_true")
    args = parser.parse_args()
    if args.base and args.phase == "overlays":
        parser.error("--base requires generated-file installation")
    try:
        install(args.payload, args.root, args.phase, args.base, args.remove_payload)
    except (OSError, ValueError, KeyError) as exc:
        sys.exit(f"install-files: {exc}")
