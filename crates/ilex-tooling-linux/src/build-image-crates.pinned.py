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
