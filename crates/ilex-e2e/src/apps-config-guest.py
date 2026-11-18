"""Offline assertions, always inside the booted image."""
import configparser
import hashlib
import json
from pathlib import Path
import subprocess

image = json.loads(Path('/usr/share/ilex/image.json').read_text())
machine = json.loads(Path('/usr/share/ilex/machine.json').read_text())
expected = set()
for entry in [*image['shells'], machine]:
    expected.update(entry.get('apps', {}).get('flatpak', []))
actual = set()
generated = sorted(Path('/usr/share/ilex/apps').glob('*.preinstall'))
copies = sorted(Path('/usr/share/flatpak/preinstall.d').glob('50-ilex-*.preinstall'))
assert len(generated) == len(copies), (generated, copies)
for source in generated:
    target = Path('/usr/share/flatpak/preinstall.d') / ('50-ilex-' + source.name)
    assert source.read_bytes() == target.read_bytes()
    config = configparser.ConfigParser()
    config.read(source)
    for section in config.sections():
        assert section.startswith('Flatpak Preinstall '), section
        assert dict(config[section]) == {'branch': 'stable'}, dict(config[section])
        actual.add(section.removeprefix('Flatpak Preinstall '))
assert actual == expected, (actual, expected)
remote = Path('/usr/share/flatpak/remotes.d/flathub.flatpakrepo')
assert hashlib.sha256(remote.read_bytes()).hexdigest() == '3371dd250e61d9e1633630073fefda153cd4426f72f4afa0c3373ae2e8fea03a'
timer = Path('/etc/systemd/system/timers.target.wants/ilex-apps.timer')
assert timer.exists(), 'shell-free transitions also need cleanup'
state = subprocess.run(['systemctl', 'is-enabled', 'flatpak-add-fedora-repos.service'], capture_output=True, text=True)
assert state.stdout.strip() == 'disabled', state
if any(shell['name'] == 'gnome' and shell.get('apps', {}).get('flatpak') for shell in image['shells']):
    assert not Path('/usr/bin/firefox').exists()
    for package in ['nautilus', 'ptyxis', 'gnome-control-center', 'gnome-software',
                    'gnome-system-monitor', 'gnome-disk-utility', 'sushi',
                    'papers-nautilus', 'papers-libs', 'papers-thumbnailer', 'papers-previewer']:
        subprocess.run(['rpm', '-q', package], check=True)
assert not Path('/var/lib/flatpak/app').exists(), 'default tests must not download Flathub apps'
subprocess.run(['ilexctl', 'apps', '--self-check'], check=True)
