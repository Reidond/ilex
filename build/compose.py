"""Validate manifests and stage generated files and add-only overlays.

This is the schema authority. Host tooling reads only the keys it needs.
Python's standard library is enough, including in a plain podman build.
"""

import argparse
import hashlib
from pathlib import Path
import re
import stat
import sys
import tomllib

from image_tree import Tree, json_bytes, parts

PROFILE_INPUT_LIMIT = 65536  # Each public TOML manifest and initial dotfile.


def fail(path, key, message):
    text = f"{path}: `{key}` {message}"
    raise ValueError("".join(f"\\u{ord(c):04x}" if ord(c) < 32 or 127 <= ord(c) <= 159 else c for c in text))


def table(path, key, value, allowed):
    if not isinstance(value, dict):
        fail(path, key, "must be a table")
    for unknown in sorted(value.keys() - allowed):
        fail(path, f"{key}.{unknown}" if key else unknown, "is an unknown key")
    return value


def load(path):
    try:
        with path.open("rb") as source:
            return tomllib.load(source)
    except (OSError, tomllib.TOMLDecodeError) as error:
        fail(path, "manifest", str(error))


def string(path, key, value):
    if not isinstance(value, str) or not value or any(ord(c) < 32 or 127 <= ord(c) <= 159 for c in value):
        fail(path, key, "must be a non-empty string without control characters (C0, C1 or DEL)")
    return value


def strings(path, key, value):
    if not isinstance(value, list):
        fail(path, key, "must be a list of strings")
    for index, item in enumerate(value):
        string(path, f"{key}[{index}]", item)
    return value


def name(path, key, value):
    string(path, key, value)
    if not re.fullmatch(r"[a-z][a-z0-9-]*", value):
        fail(path, key, "must match [a-z][a-z0-9-]*")
    return value


def identity(path, data):
    if name(path, "name", data.get("name")) != path.parent.name:
        fail(path, "name", f"must equal directory name '{path.parent.name}'")
    string(path, "description", data.get("description"))


def packages(path, key, data):
    table(path, key, data, {"install", "exclude"})
    for kind, values in data.items():
        for index, value in enumerate(strings(path, f"{key}.{kind}", values)):
            # One argument per line, consumed by xargs: reject option injection
            # and quoting/whitespace which would change argument boundaries.
            if value.startswith("-") or re.search(r"[\s'\"\\]", value):
                fail(path, f"{key}.{kind}[{index}]", "must be a single package or @group argument")
            # Fedora's package-name alphabet is ASCII letters/digits and -._+.
            # Use stable comps IDs after @, rather than translated group names.
            # https://docs.fedoraproject.org/en-US/packaging-guidelines/Naming/
            literal = r"@?[A-Za-z0-9_][A-Za-z0-9._+-]*"
            # DNF supports *, ?, ranges and !/^ negation in bracket globs.
            # https://dnf5.readthedocs.io/en/latest/misc/specs.7.html#globs
            glob = r"@?(?:[A-Za-z0-9._+-]|[?*]|\[[!^]?[A-Za-z0-9._+-]+\])+"
            pattern = literal if kind == "install" else glob
            if not re.fullmatch(pattern, value) or (kind == "install" and value.lower().endswith(".rpm")):
                fail(path, f"{key}.{kind}[{index}]", "must be a package name or @group ID" +
                     (" (DNF exclude globs are allowed)" if kind == "exclude" else "") +
                     "; URLs, paths and RPM file specifications are forbidden")
    return data


def selected(path, key, values):
    values = strings(path, key, values)
    for value in values:
        name(path, key, value)
    if len(set(values)) != len(values):
        fail(path, key, "must not contain duplicates")
    return values


def default(path, key, value, choices):
    if choices:
        if value not in choices:
            fail(path, key, f"must name one of {choices}")
    elif value is not None:
        fail(path, key, "must be absent when the list is empty")
    return value


def shell(path, is_default):
    data = table(path, "", load(path), {"name", "description", "packages", "apps", "session", "greeter"})
    identity(path, data)
    if data["name"] == "none":
        fail(path, "name", "must not be 'none' (reserved for shell selection)")
    packages(path, "packages", data.setdefault("packages", {}))
    apps(path, data)
    session = table(path, "session", data.get("session"), {"desktop", "compositor"})
    for key in ("desktop", "compositor"):
        value = string(path, f"session.{key}", session.get(key))
        if not re.fullmatch(r"[A-Za-z0-9_+.-]+", value) or value in (".", ".."):
            fail(path, f"session.{key}", "must be a file name without a directory")
    if "greeter" in data or is_default:
        if "greeter" not in data:
            fail(path, "greeter", "is required for the default shell")
        greeter = table(path, "greeter", data.get("greeter"), {"unit"})
        unit = string(path, "greeter.unit", greeter.get("unit"))
        if not re.fullmatch(r"[A-Za-z0-9_@][A-Za-z0-9_@.-]*\.service", unit):
            fail(path, "greeter.unit", "must name a systemd service")
    return data


def apps(path, data):
    value = table(path, "apps", data.setdefault("apps", {}), {"flatpak", "rpm"})
    ids = strings(path, "apps.flatpak", value.setdefault("flatpak", []))
    for app_id in ids:
        if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]*(?:\.[A-Za-z][A-Za-z0-9_-]*){2,}", app_id):
            fail(path, "apps.flatpak", "must contain Flatpak application IDs")
    if len(ids) != len(set(ids)):
        fail(path, "apps.flatpak", "must not contain duplicates")
    packages(path, "apps.rpm", {"install": value.setdefault("rpm", [])})
    return value


def preinstall(generated, kind, value, ids):
    if ids:
        text = f"# Generated from {kind}/{value}. Do not edit.\n"
        text += "".join(f"[Flatpak Preinstall {app_id}]\nBranch=stable\n\n" for app_id in ids)
        generated.write(f"usr/share/ilex/apps/{kind}-{value}.preinstall", text.encode())


def machine_matches(path, data):
    """Installer-only rules: never emit them in machine.json."""
    if "match" not in data:
        return
    if data["name"] in ("generic", "vm"):
        fail(path, "match", "is forbidden: generic is the fallback; vm is never installed on hardware")
    rules = data["match"]
    if not isinstance(rules, list):
        fail(path, "match", "must be an array of tables")
    placeholders = {"to be filled by o.e.m.", "default string", "system manufacturer",
                    "system product name", "base board manufacturer", "base board product name",
                    "not applicable", "not specified", "none", "o.e.m.", "type1productconfigid", "invalid"}
    for rule in rules:
        table(path, "match", rule, {"sys_vendor", "product_name", "board_vendor", "board_name"})
        keys = set(rule)
        if keys not in ({"sys_vendor", "product_name"}, {"board_vendor", "board_name"},
                        {"sys_vendor", "product_name", "board_vendor", "board_name"}):
            fail(path, "match", "needs a vendor and a model of the same SMBIOS structure (no incomplete pairs)")
        for key, value in rule.items():
            string(path, f"match.{key}", value)
            if len(value) > 128 or value != value.strip():
                fail(path, f"match.{key}", "must have 1–128 characters without surrounding whitespace")
            if value.lower() in placeholders:
                fail(path, f"match.{key}", "is a firmware placeholder; it would match other machines")


def generic(path):
    data = table(path, "", load(path), {"name", "description", "packages", "apps", "match"})
    identity(path, data)
    machine_matches(path, data)
    for key in ("packages", "apps"):
        if key in data:
            fail(path, key, "is not allowed for generic")
    if (path.parent / "rootfs").exists() or (path.parent / "rootfs").is_symlink():
        fail(path, "rootfs", "is not allowed for generic")
    return {"schema": 1, **data, "packages": {}}


def machine(args):
    path, out = args.dir / "machine.toml", args.out
    data = table(path, "", load(path), {"name", "description", "packages", "apps", "match"})
    identity(path, data)
    machine_matches(path, data)
    data.pop("match", None)
    value = data["name"]
    if value in {"generic", "latest", "main", "rechunked", "upgrade", "base"} or value.startswith(("candidate", "sha", "wt-")):
        fail(path, "name", f"'{value}' is reserved and cannot name a machine layer")
    package_set = packages(path, "packages", data.setdefault("packages", {}))
    app_set = apps(path, data)
    overlays = []
    overlay = args.dir / "rootfs"
    if overlay.exists() or overlay.is_symlink():
        directory, label = "overlays/000", f"machines/{value}/rootfs"
        with Tree(out / "install" / directory, label, create=True) as staged:
            staged.add_overlay(overlay, label)
        overlays.append({"name": label, "directory": directory})
    with Tree(out / "install/generated", "generated files", create=True) as generated:
        generated.write("usr/share/ilex/machine.json", json_bytes({"schema": 1, **data}))
        preinstall(generated, "machine", value, app_set["flatpak"])
    with Tree(out / "install", "installation staging", create=True) as staging:
        staging.write("overlays.json", json_bytes(overlays))
        staging.write("install-files.py", Path(__file__).with_name("image_tree.py").read_bytes())
    arguments = (package_set.get("install", []) + app_set["rpm"] +
                 (["flatpak"] if app_set["flatpak"] else []) +
                 [f"--exclude={p}" for p in package_set.get("exclude", [])])
    with Tree(out / "build", "package staging", create=True) as build:
        build.write("packages.args", "".join(p + "\n" for p in arguments).encode("utf-8"))
        build.write("install-packages.sh", Path(__file__).with_name("install-packages.sh").read_bytes())


def publications(path, data):
    """Validate publication syntax before composing any selected image."""
    entries = data.get("publish", [])
    if not isinstance(entries, list):
        fail(path, "publish", "must be a list of tables")
    seen, tags, installer_tags = set(), {"base"}, set()
    for index, entry in enumerate(entries):
        key = f"publish[{index}]"
        table(path, key, entry, {"shells", "machines"})
        shells = selected(path, f"{key}.shells", entry.get("shells"))
        machines = selected(path, f"{key}.machines", entry.get("machines"))
        if "none" in shells:
            fail(path, f"{key}.shells", "use [] for no shell, not 'none'")
        if tuple(shells) in seen:
            fail(path, key, "duplicates an ordered shell list")
        seen.add(tuple(shells))
        if not machines:
            fail(path, f"{key}.machines", "must not be empty")
        if bool(shells) != ("generic" in machines):
            fail(path, f"{key}.machines", "must contain generic with shells, and must not contain generic without shells (that image is base)")
        for machine in machines:
            if machine in {"latest", "main", "rechunked", "upgrade", "base"} or machine.startswith(("candidate", "sha", "wt-")):
                fail(path, f"{key}.machines", f"'{machine}' is reserved")
            tag = ".".join([machine, *shells])
            if tag in tags or len(tag) + len("_sha-1234567") > 128:
                fail(path, key, f"derived tag '{tag}' is duplicated or exceeds 128 bytes with its suffix")
            tags.add(tag)
            if machine == "generic" and shells:
                installer_tags.add(tag)
    installer = string(path, "installer", data.get("installer"))
    if installer not in installer_tags:
        fail(path, "installer", "must name a published generic composition with at least one shell")


def user_profiles(ctx, names, generated=None):
    """Public templates, never homed records or executable build inputs.

    Deliberately small v1 dotfile/settings allowlists. Extend them by review,
    rather than allowing credentials, login hooks or arbitrary dconf paths.
    Every read (including manifests and ancestors) is descriptor/no-follow.
    """
    catalogue = []
    public = {}
    with Tree(ctx, "profile inputs") as source:
        for profile in names:
            prefix = f"users/{profile}"
            path = f"{prefix}/user.toml"

            def manifest(leaf, optional=False):
                raw = source.read(f"{prefix}/{leaf}", optional=optional, max_bytes=PROFILE_INPUT_LIMIT)
                if raw is None:
                    return {}
                try:
                    return tomllib.loads(raw.decode("utf-8"))
                except (UnicodeError, ValueError) as error:
                    fail(f"{prefix}/{leaf}", "manifest", str(error))

            data = table(path, "", manifest("user.toml"), {
                "schema", "revision", "username", "display-name", "shell", "locale",
                "groups", "storage", "disk-size-gib",
            })
            if type(data.get("schema")) is not int or data["schema"] != 1:
                fail(path, "schema", "must be 1")
            if type(data.get("revision")) is not int or not 1 <= data["revision"] <= 2**31 - 1:
                fail(path, "revision", "must be a positive 31-bit integer; increase when changing a profile")
            username = string(path, "username", data.get("username"))
            if username != profile or len(username) > 31 or not re.fullmatch(r"[a-z][a-z0-9-]*", username):
                fail(path, "username", "must equal the profile directory and be a Unix username of at most 31 bytes")
            display = string(path, "display-name", data.get("display-name"))
            if len(display) > 256:
                fail(path, "display-name", "must be at most 256 characters")
            if data.get("shell") not in ("bash",):
                fail(path, "shell", "must be bash (the supported login shell)")
            if not re.fullmatch(r"[a-z]{2,3}_[A-Z]{2}\.UTF-8", string(path, "locale", data.get("locale"))):
                fail(path, "locale", "must have the form en_US.UTF-8")
            groups = selected(path, "groups", data.get("groups", []))
            if set(groups) - {"wheel", "gamemode"}:
                fail(path, "groups", "only explicitly approved wheel and gamemode are supported")
            data["groups"] = groups
            if data.get("storage") != "luks-btrfs":
                fail(path, "storage", "must be luks-btrfs")
            size = data.get("disk-size-gib")
            if type(size) is not int or not 2 <= size <= 1048576:
                fail(path, "disk-size-gib", "must be an integer between 2 and 1048576")

            settings = table(path, "settings", manifest("settings.toml", True), {"gnome"})
            gnome = table(path, "settings.gnome", settings.get("gnome", {}), {
                "color-scheme", "clock-show-weekday", "clock-format",
            })
            for key, value in gnome.items():
                if key == "clock-show-weekday":
                    valid = type(value) is bool
                else:
                    valid = value in ({"default", "prefer-dark", "prefer-light"} if key == "color-scheme" else {"12h", "24h"}) if isinstance(value, str) else False
                if not valid:
                    fail(path, f"settings.gnome.{key}", "has an unsupported value")

            apps = table(path, "apps", manifest("apps.toml", True), {"flatpaks"})
            requests = apps.setdefault("flatpaks", [])
            if not isinstance(requests, list):
                fail(path, "apps.flatpaks", "must be an array of tables")
            seen_apps = set()
            for app in requests:
                table(path, "apps.flatpaks", app, {"id", "remote"})
                app_id = string(path, "apps.flatpaks.id", app.get("id"))
                if not re.fullmatch(r"[A-Za-z][A-Za-z0-9_-]*(?:\.[A-Za-z][A-Za-z0-9_-]*){2,}", app_id):
                    fail(path, "apps.flatpaks.id", "must be a Flatpak application ID")
                if app.get("remote") != "flathub" or app_id in seen_apps:
                    fail(path, "apps.flatpaks", "requires remote flathub and unique application IDs")
                seen_apps.add(app_id)

            files = {}
            for entry, mode in source.entries(prefix):
                relative = entry[len(prefix) + 1:]
                parts(relative)
                if "\\" in relative:
                    fail(path, relative, "backslashes are forbidden")
                if not (stat.S_ISDIR(mode) or stat.S_ISREG(mode)):
                    fail(path, relative, "only real directories and regular files are supported (no symlinks)")
                if stat.S_ISDIR(mode):
                    if relative not in {"home", "home/.config", "home/.config/ilex", "home/.config/git"}:
                        fail(path, relative, "unexpected profile directory")
                    continue
                if relative in {"user.toml", "settings.toml", "apps.toml"}:
                    continue
                target = relative.removeprefix("home/")
                # Nothing that carries authentication, credentials or session hooks.
                if not relative.startswith("home/") or not (
                    target in {".gitignore", ".editorconfig", ".config/git/ignore"}
                    or re.fullmatch(r"\.config/ilex/[a-z][a-z0-9-]*\.(?:txt|toml|json)", target)
                ):
                    fail(path, relative, "is outside the safe dotfile allowlist; identity, keyrings and credential paths are reserved")
                if mode & 0o111:
                    # Windows /mnt checkouts have synthetic mode bits; regular
                    # files there are non-executable with the supported mount.
                    fail(path, relative, "executable dotfiles are forbidden")
                raw = source.read(entry, max_bytes=PROFILE_INPUT_LIMIT)
                try:
                    text = raw.decode("utf-8")
                except UnicodeError:
                    fail(path, relative, "must be UTF-8 text")
                if any(ord(c) < 32 and c not in "\n\t" for c in text) or re.search(
                    r"(?i)-----BEGIN .*PRIVATE KEY-----|[\"']?(?:password|secret|token|recovery[_-]?key)[\"']?\s*[:=]", text
                ):
                    fail(path, relative, "contains forbidden control or credential material")
                files[target] = hashlib.sha256(raw).hexdigest()
                if generated is not None:
                    generated.write(f"usr/share/ilex/{prefix}/home/{target}", raw)
            if generated is not None:
                payload = json_bytes({**data, "files": files, "settings": settings, "apps": apps})
                generated.write(f"usr/share/ilex/{prefix}/user.json", payload)
                catalogue.append({"id": profile, "schema": 1, "revision": data["revision"],
                                  "sha256": hashlib.sha256(payload).hexdigest()})
            public[profile] = {"schema": data["schema"], "display-name": display}
    if generated is not None:
        generated.write("usr/share/ilex/users.json", json_bytes({"schema": 1, "profiles": catalogue}))
    return public


def installer_profiles(args):
    with Tree(args.context, "profile inputs") as source:
        raw = source.read("image.toml", max_bytes=PROFILE_INPUT_LIMIT)
    data = tomllib.loads(raw.decode("utf-8"))
    names = selected("image.toml", "users", data.get("users", []))
    sys.stdout.buffer.write(json_bytes(user_profiles(args.context, names)))


def image(args):
    ctx, out = args.context, args.out
    path = ctx / "image.toml"
    data = table(path, "", load(path), {"shells", "default-shell", "installer", "publish", "users"})
    publications(path, data)
    shells = selected(path, "shells", data.get("shells"))
    shell_default = default(path, "default-shell", data.get("default-shell"), shells)
    if args.shells:
        shells = [] if args.shells == "none" else selected("--shells", "value", args.shells.split(","))
        shell_default = args.default_shell or (shells[0] if shells else None)
    elif args.default_shell:
        shell_default = args.default_shell
    default("--default-shell" if args.default_shell else "--shells" if args.shells else path,
            "value" if args.default_shell or args.shells else "default-shell", shell_default, shells)

    def package_file(package_path):
        return [packages(package_path, key, value) for key, value in load(package_path).items()]

    def flattened(package_sets):
        return {kind: [p for group in package_sets for p in group.get(kind, [])]
                for kind in ("install", "exclude")}

    base_packages = flattened(package_file(ctx / "packages.toml"))
    shell_packages = flattened(package_file(ctx / "shells/packages.toml") if shells else [])
    shell_sets = [shell_packages]
    manifests = []
    for selected_name in shells:
        if not (ctx / "shells" / selected_name).is_dir():
            fail(path, "shells", f"'{selected_name}' has no directory under {ctx / 'shells'}")
        manifest = shell(ctx / "shells" / selected_name / "shell.toml", selected_name == shell_default)
        manifests.append(manifest)
        shell_sets.append(manifest["packages"])
        shell_sets.append({"install": manifest["apps"]["rpm"]})
    machine = generic(ctx / "machines/generic/machine.toml")

    # Stage each overlay separately. The final-stage installer can see package
    # and common-rootfs paths and rejects every collision, including symlinks.
    overlays = []
    for index, selected_name in enumerate(shells):
        overlay = ctx / "shells" / selected_name / "rootfs"
        if overlay.exists() or overlay.is_symlink():
            if not overlay.is_dir() or overlay.is_symlink():
                fail(ctx / "shells" / selected_name / "shell.toml", "rootfs", "must be a directory")
            directory = f"overlays/{index:03d}"
            label = f"shells/{selected_name}/rootfs"
            with Tree(out / "install" / directory, label, create=True) as staged:
                staged.add_overlay(overlay, label)
            overlays.append({"name": label, "directory": directory})
    with Tree(out / "install/generated", "generated files", create=True) as generated:
        user_profiles(ctx, selected(path, "users", data.get("users", [])), generated)
        for manifest in manifests:
            preinstall(generated, "shell", manifest["name"], manifest["apps"]["flatpak"])
        generated.write("usr/share/ilex/image.json", json_bytes({
            "schema": 1, "shells": manifests, "default-shell": shell_default,
            "packages": base_packages, "shell-packages": shell_packages,
        }))
        generated.write("usr/share/ilex/machine.json", json_bytes(machine))
    # display-manager.service has one owner: the default shell's greeter. Every
    # shell's package enables its own greeter when it installs, and the first
    # package to install wins that alias, which need not be the default's. The
    # Containerfile therefore disables every other greeter and `enable --force`s
    # the first line of `greeters`. A unit shared by several shells is listed once.
    greeters = []
    for unit in ([m["greeter"]["unit"] for m in manifests if m["name"] == shell_default and "greeter" in m] +
                 [m["greeter"]["unit"] for m in manifests if m["name"] != shell_default and "greeter" in m]):
        if unit not in greeters:
            greeters.append(unit)
    with Tree(out / "install", "installation staging", create=True) as staging:
        staging.write("overlays.json", json_bytes(overlays))
        staging.write("greeters", "".join(unit + "\n" for unit in greeters).encode("utf-8"))
        staging.write("install-files.py", Path(__file__).with_name("image_tree.py").read_bytes())
    for part, package_set in (("base", base_packages), ("shells", flattened(shell_sets))):
        arguments = package_set["install"] + [f"--exclude={p}" for p in package_set["exclude"]]
        with Tree(out / "build" / part, "package staging", create=True) as build:
            build.write("packages.args", "".join(p + "\n" for p in arguments).encode("utf-8"))
            build.write("install-packages.sh", (ctx / "build/install-packages.sh").read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    compose = commands.add_parser("image")
    compose.add_argument("--context", type=Path, required=True)
    compose.add_argument("--out", type=Path, required=True)
    compose.add_argument("--shells", default="")
    compose.add_argument("--default-shell", default="")
    layer = commands.add_parser("machine")
    layer.add_argument("--dir", type=Path, required=True)
    layer.add_argument("--out", type=Path, required=True)
    installer = commands.add_parser("installer-profiles")
    installer.add_argument("--context", type=Path, required=True)
    args = parser.parse_args()
    try:
        {"image": image, "machine": machine, "installer-profiles": installer_profiles}[args.command](args)
    except (ValueError, OSError) as error:
        sys.exit(f"compose: {error}")


if __name__ == "__main__":
    main()
