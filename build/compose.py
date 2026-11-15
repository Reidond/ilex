"""Validate image manifests and compose build arguments and the /usr overlay.

This is the schema authority. Host tooling reads only the keys it needs.
Python's standard library is enough, including in a plain podman build.
"""

import argparse
import json
from pathlib import Path
import re
import shutil
import sys
import tomllib


def fail(path, key, message):
    raise ValueError(f"{path}: `{key}` {message}")


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
    if not isinstance(value, str) or not value or any(c in value for c in "\r\n\0"):
        fail(path, key, "must be a non-empty, single-line string")
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
    if (path.parent / "rootfs").exists():
        fail(path, "rootfs", "is not allowed for generic")
    return {"schema": 1, **data, "publish": False, "packages": {}}


def write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def image(args):
    ctx, out = args.context, args.out
    path = ctx / "image.toml"
    data = table(path, "", load(path), {"shells", "default-shell", "design-systems", "default-design-system"})
    shells = selected(path, "shells", data.get("shells"))
    shell_default = default(path, "default-shell", data.get("default-shell"), shells)
    designs = selected(path, "design-systems", data.get("design-systems"))
    design_default = default(path, "default-design-system", data.get("default-design-system"), designs)
    if args.shells:
        shells = [] if args.shells == "none" else selected(path, "--shells", args.shells.split(","))
        shell_default = args.default_shell or (shells[0] if shells else None)
    elif args.default_shell:
        shell_default = args.default_shell
    default(path, "default-shell", shell_default, shells)

    package_path = ctx / "packages.toml"
    common = load(package_path)
    package_sets = [packages(package_path, key, value) for key, value in common.items()]
    manifests = []
    for selected_name in shells:
        if not (ctx / "shells" / selected_name).is_dir():
            fail(path, "shells", f"'{selected_name}' has no directory under {ctx / 'shells'}")
        manifest = shell(ctx / "shells" / selected_name / "shell.toml", selected_name == shell_default)
        manifests.append(manifest)
        package_sets.append(manifest["packages"])
    design_manifests, token_files = [], []
    for selected_name in designs:
        if not (ctx / "design-systems" / selected_name).is_dir():
            fail(path, "design-systems", f"'{selected_name}' has no directory under {ctx / 'design-systems'}")
        manifest, source = design_system(ctx / "design-systems" / selected_name / "design-system.toml")
        design_manifests.append(manifest)
        token_files.append((manifest["tokens"], source))
    machine = generic(ctx / "machines/generic/machine.toml")

    # Validate everything before creating output. Common rootfs is copied by
    # the Containerfile first; shell overlays follow in the declared order.
    for selected_name in shells:
        overlay = ctx / "shells" / selected_name / "rootfs"
        if overlay.exists():
            if not overlay.is_dir():
                fail(ctx / "shells" / selected_name / "shell.toml", "rootfs", "must be a directory")
            shutil.copytree(overlay, out / "rootfs", symlinks=True, dirs_exist_ok=True)
    for destination, source in token_files:
        target = out / "rootfs" / destination.lstrip("/")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source, target)
    write_json(out / "rootfs/usr/share/ilex/image.json", {
        "schema": 1, "shells": manifests, "default-shell": shell_default,
        "design-systems": design_manifests, "default-design-system": design_default,
    })
    write_json(out / "rootfs/usr/share/ilex/machine.json", machine)
    build = out / "build"
    build.mkdir(parents=True, exist_ok=True)
    install = [p for group in package_sets for p in group.get("install", [])]
    exclude = [f"--exclude={p}" for group in package_sets for p in group.get("exclude", [])]
    (build / "packages.args").write_text("".join(p + "\n" for p in install + exclude), encoding="utf-8")
    shutil.copyfile(ctx / "build/install-packages.sh", build / "install-packages.sh")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    compose = commands.add_parser("image")
    compose.add_argument("--context", type=Path, required=True)
    compose.add_argument("--out", type=Path, required=True)
    compose.add_argument("--shells", default="")
    compose.add_argument("--default-shell", default="")
    args = parser.parse_args()
    try:
        image(args)
    except (ValueError, OSError) as error:
        sys.exit(f"compose: {error}")


if __name__ == "__main__":
    main()
