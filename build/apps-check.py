"""Check all configured Flatpaks against Flathub; no Flatpak installation."""
import concurrent.futures
import base64
import configparser
import datetime
import hashlib
import json
from pathlib import Path
import sys
import subprocess
import tempfile
import tomllib
import urllib.error
import urllib.request

root = Path(sys.argv[1])
ids = set()
extensions = {}  # id -> branch
for directory, filename in (("shells", "shell.toml"), ("machines", "machine.toml")):
    for path in (root / directory).glob(f"*/{filename}"):
        apps = tomllib.loads(path.read_text()).get("apps", {})
        ids.update(apps.get("flatpak", []))
        extensions.update({e["id"]: e["branch"] for e in apps.get("flatpak-extension", [])})


def get(url):
    with urllib.request.urlopen(url, timeout=30) as response:
        return response.read()


def check(app_id):
    try:
        summary = json.loads(get(f"https://flathub.org/api/v2/summary/{app_id}"))
        if "stable" not in summary.get("branches", {}) or "x86_64" not in summary.get("arches", []):
            return "error", f"{app_id}: no x86_64 stable build"
        appstream = json.loads(get(f"https://flathub.org/api/v2/appstream/{app_id}"))
        if appstream.get("is_eol") or summary.get("metadata", {}).get("runtimeIsEol"):
            return "warning", f"{app_id}: end of life"
        return "ok", app_id
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return "error", f"{app_id}: {error}"
        return "warning", f"not verified: Flathub unreachable for {app_id}: {error}"
    except (OSError, ValueError) as error:
        return "warning", f"not verified: Flathub unreachable for {app_id}: {error}"


def check_extension(item):
    """A runtime extension has no appstream entry: its summary names its branches.

    Flathub's API does not report an extension's end of life (a replaced theme
    still installs); the manifest comment is where that is recorded.
    """
    extension_id, branch = item
    try:
        summary = json.loads(get(f"https://flathub.org/api/v2/summary/{extension_id}"))
        if branch not in summary.get("branches", {}) or "x86_64" not in summary.get("arches", []):
            return "error", f"{extension_id}//{branch}: no x86_64 build on that branch"
        return "ok", f"{extension_id}//{branch}"
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return "error", f"{extension_id}: {error}"
        return "warning", f"not verified: Flathub unreachable for {extension_id}: {error}"
    except (OSError, ValueError) as error:
        return "warning", f"not verified: Flathub unreachable for {extension_id}: {error}"


errors = 0
with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
    for level, message in [*pool.map(check, sorted(ids)), *pool.map(check_extension, sorted(extensions.items()))]:
        print(f"{level}: {message}")
        errors += level == "error"
try:
    shipped = (root / "rootfs/usr/share/flatpak/remotes.d/flathub.flatpakrepo").read_bytes()
    live = get("https://dl.flathub.org/repo/flathub.flatpakrepo")
    if shipped != live:
        print("warning: Flathub trust file changed; review and update the checked-in key")
    print(f"Flathub trust file sha256: {hashlib.sha256(shipped).hexdigest()}")
    config = configparser.ConfigParser()
    config.read_string(shipped.decode())
    with tempfile.TemporaryDirectory(prefix=".ilex-flathub-key-", dir=root) as directory:
        key = Path(directory) / "flathub.gpg"
        key.write_bytes(base64.b64decode(config["Flatpak Repo"]["GPGKey"]))
        result = subprocess.run(["gpg", "--homedir", directory, "--batch", "--with-colons", "--show-keys", str(key)], capture_output=True, text=True, check=True)
        for line in result.stdout.splitlines():
            fields = line.split(":")
            if fields[0] == "pub" and fields[6]:
                expiry = datetime.datetime.fromtimestamp(int(fields[6]), datetime.timezone.utc)
                if expiry - datetime.datetime.now(datetime.timezone.utc) <= datetime.timedelta(days=90):
                    print(f"warning: Flathub signing key expires {expiry.date()}; refresh the committed trust file")
except OSError as error:
    print(f"warning: not verified: Flathub unreachable: {error}")
sys.exit(1 if errors else 0)
