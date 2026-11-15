"""Validate manifests and stage generated files and add-only overlays.

This is the schema authority. Host tooling reads only the keys it needs.
Python's standard library is enough, including in a plain podman build.
"""

import argparse
import json
from pathlib import Path
import re
import sys
import tomllib

from image_tree import Tree, json_bytes


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
    data = table(path, "", load(path), {"name", "description", "packages", "session", "greeter", "theme"})
    identity(path, data)
    if data["name"] == "none":
        fail(path, "name", "must not be 'none' (reserved for shell selection)")
    packages(path, "packages", data.setdefault("packages", {}))
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
        if not re.fullmatch(r"[A-Za-z0-9_@.-]+\.service", unit):
            fail(path, "greeter.unit", "must name a systemd service")
    theme = table(path, "theme", data.setdefault("theme", {}), {"targets"})
    if strings(path, "theme.targets", theme.setdefault("targets", [])):
        fail(path, "theme.targets", "must be empty until theme targets are defined")
    return data


def design_system(path):
    data = table(path, "", load(path), {"name", "description", "tokens"})
    identity(path, data)
    relative = string(path, "tokens", data.get("tokens", "tokens.json"))
    source = path.parent / relative
    if not source.resolve().is_relative_to(path.parent.resolve()):
        fail(path, "tokens", "must name a file inside the design system directory")
    try:
        tokens = json.loads(source.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, ValueError) as error:
        fail(path, "tokens", f"{source}: {error}")
    if not isinstance(tokens, dict):
        fail(path, "tokens", f"{source} must contain a JSON object")
    data["tokens"] = f"/usr/share/ilex/design-systems/{data['name']}/tokens.json"
    return data, source


def generic(path):
    data = table(path, "", load(path), {"name", "description", "publish", "packages"})
    identity(path, data)
    # generic is the base, so even an explicit publish=false is not allowed.
    for key in ("publish", "packages"):
        if key in data:
            fail(path, key, "is not allowed for generic")
    if (path.parent / "rootfs").exists() or (path.parent / "rootfs").is_symlink():
        fail(path, "rootfs", "is not allowed for generic")
    return {"schema": 1, **data, "publish": False, "packages": {}}


def machine(args):
    path, out = args.dir / "machine.toml", args.out
    data = table(path, "", load(path), {"name", "description", "publish", "packages"})
    identity(path, data)
    value = data["name"]
    if value in {"generic", "latest", "candidate", "main", "rechunked", "upgrade", "base"} or value.startswith(("sha", "wt-")):
        fail(path, "name", f"'{value}' is reserved and cannot name a machine layer")
    if not isinstance(data.setdefault("publish", False), bool):
        fail(path, "publish", "must be a boolean")
    package_set = packages(path, "packages", data.setdefault("packages", {}))
    overlays = []
    overlay = args.dir / "rootfs"
    if overlay.exists() or overlay.is_symlink():
        directory, label = "overlays/000", f"machines/{value}/rootfs"
        with Tree(out / "install" / directory, label, create=True) as staged:
            staged.add_overlay(overlay, label)
        overlays.append({"name": label, "directory": directory})
    with Tree(out / "install/generated", "generated files", create=True) as generated:
        generated.write("usr/share/ilex/machine.json", json_bytes({"schema": 1, **data}))
    with Tree(out / "install", "installation staging", create=True) as staging:
        staging.write("overlays.json", json_bytes(overlays))
        staging.write("install-files.py", Path(__file__).with_name("image_tree.py").read_bytes())
    arguments = package_set.get("install", []) + [f"--exclude={p}" for p in package_set.get("exclude", [])]
    with Tree(out / "build", "package staging", create=True) as build:
        build.write("packages.args", "".join(p + "\n" for p in arguments).encode("utf-8"))
        build.write("install-packages.sh", Path(__file__).with_name("install-packages.sh").read_bytes())


def image(args):
    ctx, out = args.context, args.out
    path = ctx / "image.toml"
    data = table(path, "", load(path), {"shells", "default-shell", "design-systems", "default-design-system"})
    shells = selected(path, "shells", data.get("shells"))
    shell_default = default(path, "default-shell", data.get("default-shell"), shells)
    designs = selected(path, "design-systems", data.get("design-systems"))
    design_default = default(path, "default-design-system", data.get("default-design-system"), designs)
    if args.shells:
        shells = [] if args.shells == "none" else selected("--shells", "value", args.shells.split(","))
        shell_default = args.default_shell or (shells[0] if shells else None)
    elif args.default_shell:
        shell_default = args.default_shell
    default("--default-shell" if args.default_shell else "--shells" if args.shells else path,
            "value" if args.default_shell or args.shells else "default-shell", shell_default, shells)

    if not shells:
        designs, design_default = [], None

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
    design_manifests, token_files = [], []
    for selected_name in designs:
        if not (ctx / "design-systems" / selected_name).is_dir():
            fail(path, "design-systems", f"'{selected_name}' has no directory under {ctx / 'design-systems'}")
        manifest, source = design_system(ctx / "design-systems" / selected_name / "design-system.toml")
        design_manifests.append(manifest)
        token_files.append((manifest["tokens"], source))
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
        for destination, source in token_files:
            generated.write(destination.lstrip("/"), source.read_bytes())
        generated.write("usr/share/ilex/image.json", json_bytes({
            "schema": 1, "shells": manifests, "default-shell": shell_default,
            "design-systems": design_manifests, "default-design-system": design_default,
            "packages": base_packages, "shell-packages": shell_packages,
        }))
        generated.write("usr/share/ilex/machine.json", json_bytes(machine))
    with Tree(out / "install", "installation staging", create=True) as staging:
        staging.write("overlays.json", json_bytes(overlays))
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
    args = parser.parse_args()
    try:
        (image if args.command == "image" else machine)(args)
    except (ValueError, OSError) as error:
        sys.exit(f"compose: {error}")


if __name__ == "__main__":
    main()
