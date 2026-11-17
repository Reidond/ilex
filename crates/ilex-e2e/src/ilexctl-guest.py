"""The installed machine CLI contract, exercised only in a booted guest."""
import json
from pathlib import Path
import subprocess


def cli(*args, code=0):
    result = subprocess.run(["/usr/bin/ilexctl", *args], capture_output=True, text=True)
    assert result.returncode == code, (args, result.returncode, result.stdout, result.stderr)
    if code == 2:
        assert not result.stdout and result.stderr.startswith("ilexctl: error:"), result
    return result.stdout


assert "(machine)" in cli("--version")
assert not Path("/usr/bin/ilex-agent").exists()
root_help = cli("--help")
assert "On an ilex machine:" in root_help and "Both builds:" in root_help
for group in ("system", "home", "profile"):
    assert group in root_help
    assert "Usage:" in cli(group, "--help")
for args in [
    ("system", "check"), ("system", "--self-check"),
    ("system", "config"), ("system", "enforce-sigpolicy"),
    ("home", "create"), ("home", "label"),
    ("profile", "list"), ("profile", "check"), ("profile", "--self-check"),
    ("profile", "apply"), ("profile", "prune-backups"),
]:
    assert "Usage:" in cli(*args, "--help")
assert "label" not in cli("home", "--help")
config = json.loads(cli("system", "config", "--json"))
assert config["schema"] == 1 and config["image"]["schema"] == 1 and config["machine"]["schema"] == 1
assert json.loads(cli("profile", "list"))["schema"] == 1
assert cli("profile", "check") == cli("profile", "--self-check")
assert "nothing to do" in cli("system", "enforce-sigpolicy", "--dry-run")

for args in [
    (), ("--self-check",), ("build",), ("mcp",), ("security",), ("tool",),
    ("system",), ("system", "config", "--json", "extra"),
    ("system", "enforce-sigpolicy", "--dry-run", "extra"),
    ("home", "create", "validuser"),
    ("home", "create", "INVALID", "--disk-size=2"),
    ("home", "create", "validuser", "--disk-size=1"),
    ("home", "create", "validuser", "--disk-size=2", "--disk-size=3"),
    ("home", "create", "validuser", "--disk-size=2", "--real-name="),
    ("home", "create", "validuser", "--disk-size=2", "--password=secret"),
    ("home", "label", "not-a-uid"), ("home", "label", "1000", "extra"),
    ("profile", "apply"), ("profile", "apply", "andriishafar", "--selected"),
    ("profile", "apply", "--selected", "--shell=unknown"),
    ("profile", "prune-backups", "--keep=-1"),
    ("profile", "list", "--json"), ("completion", "unknown"),
]:
    cli(*args, code=2)
cli("profile", "apply", "andriishafar", code=1)  # Root cannot apply a daily profile.
cli("profile", "apply", "--", "--help", code=1)  # An ID is never a helper option.
cli("profile", "prune-backups", code=1)

for shell, path in [
    ("bash", "/usr/share/bash-completion/completions/ilexctl"),
    ("zsh", "/usr/share/zsh/site-functions/_ilexctl"),
    ("fish", "/usr/share/fish/vendor_completions.d/ilexctl.fish"),
]:
    generated = cli("completion", shell)
    assert generated == Path(path).read_text(), path
    assert "ilexctl" in generated and "prune-backups" in generated
assert "ilexctl" in cli("completion", "powershell")
for path, command in [
    ("/usr/lib/systemd/system/ilex-sigpolicy.service", "/usr/bin/ilexctl system enforce-sigpolicy"),
    ("/usr/lib/systemd/system/ilex-homed-label@.service", "/usr/bin/ilexctl home label %i"),
    ("/usr/lib/systemd/user/ilex-profile.service", "/usr/bin/ilexctl profile apply --selected"),
    ("/usr/lib/systemd/system/drupe.service", "/usr/bin/drupe activate"),
]:
    assert f"ExecStart={command}\n" in Path(path).read_text(), path
assert Path("/usr/bin/drupe").is_file()
print("ilexctl machine CLI: ok")
