# The Containerfile's builder step: build the Rust packages the root image ships,
# exactly as build/image-crates.toml declares them, and stage the binaries and
# shell completions under /out.
#
#   python3 -I -S build/build-image-crates.py build/image-crates.toml
#
# Run from /src. The declaration is the only place that names a package; the
# image fingerprint reads the same file for its crate closure, and
# `ilexctl doctor --self-check` refuses a Containerfile that runs the Rust
# toolchain anywhere else, and this script if it names the toolchain on any
# line but the two below (the wrapper and the one build command).
#
# `--list DECLARATION` prints the declaration as JSON and builds nothing; the
# self-check compares it with the fingerprint's own reading of the file.
#
# Standard library only, as every build helper here: -I -S runs no site hooks.
import json
import os
import re
import subprocess
import sys
import tomllib

PACKAGE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_-]*")
KEYS = {"packages", "no-default-features"}

# Files the image takes from target/release, and where.
BINARIES = [
    ("ilexctl", "/usr/bin/ilexctl"),
    ("ilex-enroll", "/usr/bin/ilex-enroll"),
    ("drupe", "/usr/bin/drupe"),
    ("drupe-enter", "/usr/libexec/drupe/drupe-enter"),
]
COMPLETIONS = [
    ("bash", "/usr/share/bash-completion/completions/ilexctl"),
    ("zsh", "/usr/share/zsh/site-functions/_ilexctl"),
    ("fish", "/usr/share/fish/vendor_completions.d/ilexctl.fish"),
]


def fail(message):
    sys.exit(f"build-image-crates: {message}")


def declared(path):
    with open(path, "rb") as file:
        data = tomllib.load(file)
    entries = data.get("build")
    if set(data) != {"build"} or not isinstance(entries, list) or not entries:
        fail(f"{path}: want a non-empty [[build]] array and nothing else")
    builds = []
    for number, entry in enumerate(entries, 1):
        where = f"{path}: [[build]] {number}"
        if not isinstance(entry, dict) or "packages" not in entry or not set(entry) <= KEYS:
            fail(f"{where}: want `packages` and optionally `no-default-features`")
        packages = entry["packages"]
        flag = entry.get("no-default-features", False)
        if (
            not isinstance(packages, list)
            or not packages
            or not all(isinstance(name, str) and PACKAGE.fullmatch(name) for name in packages)
        ):
            fail(f"{where}: `packages` must be a non-empty list of plain package names")
        if not isinstance(flag, bool):
            fail(f"{where}: `no-default-features` must be true or false")
        builds.append({"packages": packages, "no-default-features": flag})
    return builds


def main(argv):
    if len(argv) == 3 and argv[1] == "--list":
        print(json.dumps(declared(argv[2]), sort_keys=True))
        return
    if len(argv) != 2:
        fail("usage: build-image-crates.py DECLARATION")
    builds = declared(argv[1])

    # Every compile goes through kache, whose store is a cache mount shared by
    # all worktrees; the host's own store, when `ilexctl build` mounts it, is
    # read-only.
    env = dict(os.environ, RUSTC_WRAPPER="kache")
    if os.path.isfile("/kache-host/index.db"):
        env["KACHE_READONLY_STORE"] = "/kache-host"
    for build in builds:
        command = ["cargo", "build", "--release", "--locked"]
        for package in build["packages"]:
            command += ["-p", package]
        if build["no-default-features"]:
            command.append("--no-default-features")
        subprocess.run(command, check=True, env=env)
    # Stop the daemon so it flushes what it queued, then report restored vs
    # compiled crates.
    subprocess.run(["kache", "daemon", "stop"], env=env)
    subprocess.run(["kache", "stats", "--last-build", "--root", "/src"], check=True, env=env)

    for name, destination in BINARIES:
        subprocess.run(
            ["install", "-Dm755", f"target/release/{name}", f"/out{destination}"], check=True
        )
    for shell, destination in COMPLETIONS:
        target = f"/out{destination}"
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as file:
            subprocess.run(["target/release/ilexctl", "completion", shell], check=True, stdout=file)
        os.chmod(target, 0o644)


main(sys.argv)
