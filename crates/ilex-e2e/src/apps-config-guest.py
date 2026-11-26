"""Offline assertions, always inside the booted image."""
import configparser
import hashlib
import json
from pathlib import Path
import subprocess

image = json.loads(Path('/usr/share/ilex/image.json').read_text())
machine = json.loads(Path('/usr/share/ilex/machine.json').read_text())
# id -> the preinstall keys it must carry: applications are on stable, runtime
# extensions (GTK themes) name their branch and IsRuntime.
expected = {}
for entry in [*image['shells'], machine]:
    apps = entry.get('apps', {})
    expected.update({app: {'branch': 'stable'} for app in apps.get('flatpak', [])})
    expected.update({
        extension['id']: {'branch': extension['branch'], 'isruntime': 'true'}
        for extension in apps.get('flatpak-extension', [])
    })
actual = {}
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
        actual[section.removeprefix('Flatpak Preinstall ')] = dict(config[section])
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
if any(shell['name'] == 'kde' and shell.get('apps', {}).get('flatpak') for shell in image['shells']):
    assert not Path('/usr/bin/firefox').exists()
    for package in ['dolphin', 'konsole', 'spectacle', 'plasma-systemmonitor', 'kde-partitionmanager',
                    'plasma-discover', 'plasma-discover-flatpak', 'flatpak-kcm', 'xdg-desktop-portal-kde',
                    'xdg-desktop-portal-gtk', 'breeze-gtk', 'ark', 'kio-admin', 'kdegraphics-thumbnailers',
                    'ffmpegthumbs']:
        subprocess.run(['rpm', '-q', package], check=True)
    # Nothing in the KDE shell writes GTK files into the home, and Discover's
    # notifier would announce system updates the user cannot apply.
    for package in ['kde-gtk-config', 'plasma-discover-notifier']:
        assert subprocess.run(['rpm', '-q', package], capture_output=True).returncode == 1, package
    # The GTK 3 themes Flatpak applications resolve in Plasma.
    kde_extensions = {e['id']: e['branch'] for shell in image['shells'] if shell['name'] == 'kde'
                      for e in shell['apps']['flatpak-extension']}
    assert kde_extensions == {'org.gtk.Gtk3theme.Breeze': '3.22', 'org.gtk.Gtk3theme.Breeze-Dark': '3.22'}, kde_extensions
    # Applications delivered as Flatpaks are not also host packages.
    for package in ['kwrite', 'kcalc', 'okular', 'gwenview', 'haruna', 'elisa-player', 'krdc', 'ktorrent',
                    'neochat', 'kolourpaint', 'kamoso', 'filelight', 'kfind', 'kcharselect', 'marble',
                    'merkuro', 'kclock', 'kweather', 'skanpage', 'kwalletmanager5', 'plasma-discover-packagekit']:
        assert subprocess.run(['rpm', '-q', package], capture_output=True).returncode == 1, package
assert not Path('/var/lib/flatpak/app').exists(), 'default tests must not download Flathub apps'
subprocess.run(['ilexctl', 'apps', '--self-check'], check=True)
