# The Containerfile's builder step: build the Rust packages the root image ships,
# exactly as build/image-crates.toml declares them, and stage the binaries and
# shell completions under /out.
#
#   python3 -I -S build/build-image-crates.py build/image-crates.toml
#
# Run from /src. The declaration is the only place that names a package; the
# image fingerprint reads the same file for its crate closure.
#
# Why the build is the build the fingerprint hashes (drift detection, not a
# sandbox; `ilexctl doctor --self-check` checks every point):
#   - every cargo argv comes from cargo_commands() and nowhere else, and main()
#     runs that function's output at one place. `--commands DECLARATION` prints
#     it, and the self-check compares it with the argv the declaration implies:
#     --release --locked, -p for each declared package, --no-default-features
#     when declared, nothing else;
#   - cargo starts with cargo_environment(): nothing the caller set in CARGO*,
#     RUST* or KACHE*, only RUSTC_WRAPPER and the read-only host store set
#     there (`--environment` prints it), so RUSTFLAGS and CARGO_BUILD_TARGET
#     cannot turn on a cfg or a target the closure does not cover;
#   - the build refuses to start while any cargo configuration file is visible
#     (cargo_configs(), `--configs`): [build] target and rustflags, [env] and
#     [patch] there change what is built and nothing the fingerprint hashes;
#   - from `def cargo_commands` to the end this file is pinned verbatim by the
#     guard (crates/ilex-tooling-linux/src/build-image-crates.pinned.py, which
#     must be edited with it); before that line the guard allows six imports
#     and no way to run a program or load code, and the toolchain's name
#     nowhere but in comments.
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


def cargo_commands(builds):
    """Every cargo command line of the build; no other code makes one."""
    commands = []
    for build in builds:
        command = ["cargo", "build", "--release", "--locked"]
        for package in build["packages"]:
            command += ["-p", package]
        if build["no-default-features"]:
            command.append("--no-default-features")
        commands.append(command)
    return commands


def cargo_environment():
    """What cargo and kache start with: the caller's environment without
    anything that configures cargo, rustc or kache, plus the settings made here."""
    env = {
        name: value
        for name, value in os.environ.items()
        if not name.upper().startswith(("CARGO", "RUST", "KACHE"))
    }
    # Every compile goes through kache, whose store is a cache mount shared by
    # all worktrees; the host's own store, when `ilexctl build` mounts it, is
    # read-only.
    env["RUSTC_WRAPPER"] = "kache"
    if os.path.isfile("/kache-host/index.db"):
        env["KACHE_READONLY_STORE"] = "/kache-host"
    return env


def cargo_configs():
    """The cargo configuration files cargo would read from here: `.cargo/config`
    and `.cargo/config.toml` of the working directory and of each parent, and of
    cargo's home."""
    directories = [os.path.join(os.path.expanduser("~"), ".cargo")]
    here = os.getcwd()
    while True:
        directories.append(os.path.join(here, ".cargo"))
        parent = os.path.dirname(here)
        if parent == here:
            break
        here = parent
    names = ("config", "config.toml")
    found = [os.path.join(directory, name) for directory in directories for name in names]
    return sorted({path for path in found if os.path.lexists(path)})


def planned(path):
    """The cargo commands main() runs for a declaration, and `--commands` prints."""
    return cargo_commands(declared(path))


def main(argv):
    if len(argv) == 3 and argv[1] == "--commands":
        print(json.dumps(planned(argv[2])))
        return
    if argv[1:] == ["--environment"]:
        print(json.dumps(cargo_environment(), sort_keys=True))
        return
    if argv[1:] == ["--configs"]:
        print(json.dumps(cargo_configs()))
        return
    if len(argv) != 2 or argv[1].startswith("-"):
        fail("usage: build-image-crates.py DECLARATION | --commands DECLARATION | --environment | --configs")
    commands = planned(argv[1])
    configs = cargo_configs()
    if configs:
        fail(f"cargo configuration is visible ({', '.join(configs)}); it would change what is built without changing the crates the fingerprint hashes")
    env = cargo_environment()
    for command in commands:
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
