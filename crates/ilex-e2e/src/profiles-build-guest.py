"""Run inside the booted image: compiler and generated-registry regressions."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, "/run/ilex-profiles-build")
import compose
from image_tree import Tree

catalogue = json.loads(Path("/usr/share/ilex/users.json").read_bytes())
registry = json.loads(Path("/usr/share/ilex/.generated.json").read_bytes())
assert catalogue["schema"] == 1
owner = next(p for p in catalogue["profiles"] if p["id"] == "andriishafar")
owner_data = json.loads(Path("/usr/share/ilex/users", owner["id"], "user.json").read_bytes())
assert owner_data["username"] == "andriishafar" and owner_data["display-name"] == "Andrii Shafar"
for profile in catalogue["profiles"]:
    path = f"users/{profile['id']}/user.json"
    raw = Path("/usr/share/ilex", path).read_bytes()
    assert hashlib.sha256(raw).hexdigest() == profile["sha256"]
    # The registry is generated-file authority, including every template.
    assert registry["usr/share/ilex/" + path] == profile["sha256"]

USER = '''schema = 1
revision = 1
username = "sample"
display-name = "Public Example"
shell = "bash"
locale = "en_US.UTF-8"
groups = []
storage = "luks-btrfs"
disk-size-gib = 2
'''

with tempfile.TemporaryDirectory(prefix="ilex-profiles-") as temporary:
    ctx = Path(temporary, "ctx")
    user = ctx / "users/sample"
    user.mkdir(parents=True)

    def reset():
        shutil.rmtree(user)
        user.mkdir()
        (user / "user.toml").write_text(USER)

    def compile_profile():
        with tempfile.TemporaryDirectory(dir=temporary) as out:
            with Tree(Path(out), "e2e generated") as generated:
                compose.user_profiles(ctx, ["sample"], generated)

    def refused(change):
        reset()
        change()
        try:
            compile_profile()
        except (ValueError, OSError):
            return
        raise AssertionError("invalid public profile was accepted")

    reset()
    compile_profile()
    for key in ("secret", "privileged", "password", "hashedPassword", "recoveryKey",
                "binding", "status", "signature", "uid", "homeDirectory", "emailAddress", "unknown"):
        refused(lambda key=key: (user / "user.toml").write_text(USER + f'{key} = "forbidden"\n'))
    for field, replacement in (("schema = 1", "schema = 2"), ("revision = 1", "revision = true"),
                               ('username = "sample"', 'username = "../sample"'),
                               ("groups = []", 'groups = ["root"]'),
                               ('storage = "luks-btrfs"', 'storage = "directory"'),
                               ("disk-size-gib = 2", "disk-size-gib = 1")):
        refused(lambda field=field, replacement=replacement: (user / "user.toml").write_text(USER.replace(field, replacement)))
    refused(lambda: (user / "user.toml").write_text("[broken"))
    refused(lambda: (user / "settings.toml").write_text('[gnome]\ncommand = "touch /tmp/unsafe"\n'))
    refused(lambda: (user / "settings.toml").write_text('[gnome]\nclock-show-weekday = "true"\n'))
    refused(lambda: (user / "apps.toml").write_text('[[flatpaks]]\nid = "org.example.App"\nremote = "https://example.test"\n'))

    def homefile(path, content="public\n", mode=0o644):
        destination = user / "home" / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(content)
        destination.chmod(mode)

    for path in (".identity", ".identity-blob", ".ssh/authorized_keys", ".local/share/keyrings/login.keyring",
                 ".config/systemd/user/hook.service", ".bashrc", ".config/ilex/a\\b.txt"):
        refused(lambda path=path: homefile(path))
    refused(lambda: homefile(".config/ilex/public.json", '{"password": "private"}'))
    refused(lambda: homefile(".config/ilex/public.txt", mode=0o755))
    refused(lambda: (user / "user.toml").unlink() or (user / "user.toml").symlink_to("/etc/passwd"))
    refused(lambda: (user / "home").symlink_to("/etc", target_is_directory=True))
    refused(lambda: (user / "settings.toml").symlink_to("/etc/passwd"))
    refused(lambda: os.mkfifo(user / "apps.toml"))

    # A full read of any of these sparse 512 MiB inputs would raise
    # MemoryError under this child's 128 MiB address-space limit. Require the
    # opened-FD size rejection, including all optional public manifests.
    for oversized in ("user.toml", "settings.toml", "apps.toml", "home/.config/ilex/public.txt"):
        reset()
        destination = user / oversized
        destination.parent.mkdir(parents=True, exist_ok=True)
        with destination.open("wb") as stream:
            stream.truncate(512 * 1024 * 1024)
        script = '''import resource, sys
from pathlib import Path
resource.setrlimit(resource.RLIMIT_AS, (128 * 1024 * 1024,) * 2)
sys.path.insert(0, '/run/ilex-profiles-build')
import compose
from image_tree import Tree
try:
    with Tree(Path(sys.argv[2]), 'oversized regression', create=True) as generated:
        compose.user_profiles(Path(sys.argv[1]), ['sample'], generated)
except ValueError as error:
    assert 'must be at most 65536 bytes' in str(error), str(error)
else:
    raise AssertionError('oversized profile input accepted')
'''
        result = subprocess.run([sys.executable, "-I", "-S", "-c", script, str(ctx), str(Path(temporary, "oversized-out"))],
                                capture_output=True, text=True, timeout=15)
        assert result.returncode == 0, (oversized, result.stdout, result.stderr)
    print("512 MiB sparse manifests/templates rejected under a 128 MiB memory limit: passed")

    reset()
    homefile(".config/ilex/public.txt")
    (user / "home/.config/ilex/public.txt").unlink()
    (user / "home/.config/ilex/public.txt").symlink_to("/etc/passwd")
    try:
        compile_profile()
    except (OSError, ValueError):
        pass
    else:
        raise AssertionError("leaf symlink accepted")

    for invalid in ("../outside", "/absolute", "a//b", "a/./b", "a/../b"):
        try:
            compose.parts(invalid)
        except ValueError:
            pass
        else:
            raise AssertionError("noncanonical path accepted")
    try:
        compose.selected("fixture", "users", ["sample", "sample"])
    except ValueError:
        pass
    else:
        raise AssertionError("duplicate catalogue ID accepted")

    reset()
    homefile(".config/ilex/public.txt")
    output = Path(temporary, "out")
    with Tree(output, "e2e generated", create=True) as generated:
        compose.user_profiles(ctx, ["sample"], generated)
    sys.path.insert(0, str(Path(compose.__file__).parent))
    import image_tree
    # Use the real generated installer and require absence of the reserved
    # tree on a base. A preexisting unregistered payload cannot be adopted.
    payload = Path(temporary, "payload")
    shutil.copytree(output, payload / "generated")
    (payload / "overlays.json").write_text("[]")
    root = Path(temporary, "root")
    root.mkdir()
    image_tree.install(payload, root, "generated", True, False, None)
    registered = json.loads((root / "usr/share/ilex/.generated.json").read_text())
    leaf = "usr/share/ilex/users/sample/home/.config/ilex/public.txt"
    assert leaf in registered
    (root / leaf).write_text("changed outside composer\n")
    try:
        image_tree.install(payload, root, "generated", False, False, None)
    except ValueError:
        pass
    else:
        raise AssertionError("changed generated file collision accepted")

print("public profile schema, credentials, paths, symlinks, special files and generated collisions: passed")
