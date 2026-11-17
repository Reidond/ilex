"""Disposable exact-ISO fixture. Never installed in the OS or installer.

Credentials stay on private provisioning media/in memory, never in the input
kickstart, argv, diagnostics, screenshots or public artifacts. The local
console driver uses a guest-only virtual keyboard; it prints stage names only.
"""
import ctypes
import fcntl
import json
import os
import pathlib
import re
import resource
import select
import struct
import subprocess
import sys
import time
import termios

PRIVATE = pathlib.Path('/run/ilex-encrypted-test')
SEED = PRIVATE / 'seed'
PUBLIC = SEED / 'public'


def harden():
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    ctypes.CDLL(None).prctl(4, 0, 0, 0, 0)  # PR_SET_DUMPABLE


def marker(text):
    with open('/dev/ttyS0', 'w') as console:
        console.write('ILEX-ENCRYPTED-' + text + '\n')


def prepare():
    PRIVATE.mkdir(mode=0o700)
    SEED.mkdir()
    subprocess.run(['mount', '/dev/disk/by-label/OEMDRV', str(SEED)], check=True)
    config = json.loads((SEED / 'config.json').read_text())
    # Only the isolated test registry gains another signing authority. The
    # shipped default reject and production repository requirements remain.
    policy_path = pathlib.Path('/etc/containers/policy.json')
    policy = json.loads(policy_path.read_text())
    assert policy['default'] == [{'type': 'reject'}]
    policy['transports'].setdefault('docker', {})[config['repository']] = [{
        'type': 'sigstoreSigned', 'keyPath': str(SEED / 'test.pub'),
        'signedIdentity': {'type': 'matchRepository'},
    }]
    policy_path.write_text(json.dumps(policy))
    pathlib.Path('/etc/containers/registries.conf.d/99-encrypted-test.conf').write_text(
        '[[registry]]\nlocation = ' + json.dumps(config['repository']) + '\ninsecure = true\n')
    pathlib.Path('/etc/containers/registries.d/encrypted-test.yaml').write_text(
        'docker:\n  ' + json.dumps(config['repository']) + ':\n    use-sigstore-attachments: true\n')
    bootc = pathlib.Path('/tmp/ilex-bootc.ks')
    text = bootc.read_text()
    text, count = re.subn(r'--source-imgref\s+registry:\S+',
                         '--source-imgref registry:' + config['source'], text)
    assert count == 1
    bootc.write_text(text)
    # The root credential is generated at runtime, and the only key-bearing
    # include is in the live RAM filesystem. Input ks.cfg contains no secret.
    passphrase = (SEED / 'passphrase').read_bytes()
    assert re.fullmatch(b'[a-z]{48}', passphrase)
    storage = PRIVATE / 'storage.ks'
    storage.write_bytes(b'autopart --type=btrfs --nohome --noswap --encrypted '
                        b'--luks-version=luks2 --passphrase=' + passphrase + b'\n')
    storage.chmod(0o600)
    PUBLIC.mkdir(mode=0o700)
    marker('PREPARED')


def screen():
    try:
        return pathlib.Path('/dev/vcs8').read_bytes()
    except OSError:
        return b''


def wait_for(text, deadline=300):
    start = time.monotonic()
    while text not in screen():
        if time.monotonic() - start > deadline:
            raise RuntimeError('local completion stage timed out')
        time.sleep(0.2)


def scan(secrets):
    roots = ['/tmp', '/var/log', '/root', '/mnt/sysroot/var/log',
             '/mnt/sysroot/var/roothome', str(PUBLIC)]
    count = 0
    leaked = False
    for root in roots:
        for directory, _, files in os.walk(root, followlinks=False):
            for name in files:
                path = pathlib.Path(directory) / name
                if path.is_symlink() or not path.is_file():
                    continue
                try:
                    data = path.read_bytes()
                except OSError:
                    continue
                count += 1
                leaked |= any(value and value in data for value in secrets)
    journal = subprocess.run(['journalctl', '--no-pager', '-o', 'cat'], capture_output=True, check=True).stdout
    leaked |= any(value and value in journal for value in secrets)
    return {'schema': 1, 'files_scanned': count, 'journal_scanned': True, 'secret_found': leaked}


def drive():
    subprocess.run(['modprobe', 'uinput'], check=True)
    keyboard = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK)
    # Linux uinput ABI. This device exists only in this disposable guest.
    fcntl.ioctl(keyboard, 0x40045564, 1)  # UI_SET_EVBIT(EV_KEY)
    codes = dict(zip('abcdefghijklmnopqrstuvwxyz',
                     [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
                      49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44]))
    codes['-'] = 12
    codes['\n'] = 28
    for code in set(codes.values()):
        fcntl.ioctl(keyboard, 0x40045565, code)
    os.write(keyboard, struct.pack('80sHHHHI', b'ilex-private-test-keyboard', 3, 1, 1, 1, 0) + bytes(1024))
    fcntl.ioctl(keyboard, 0x5501)  # UI_DEV_CREATE
    input_fds = [os.open(str(path), os.O_RDONLY | os.O_NONBLOCK)
                 for path in pathlib.Path('/dev/input').glob('event*')]

    def safe_capture(stage):
        # Stop until the host captured this known key-free screen. A timer
        # would allow a delayed host to photograph the recovery key.
        marker(stage)
        start = time.monotonic()
        while time.monotonic() - start < 180:
            ready, _, _ = select.select(input_fds, [], [], 1)
            for fd in ready:
                data = os.read(fd, 24 * 64)
                for at in range(0, len(data), 24):
                    _, _, kind, code, value = struct.unpack('llHHi', data[at:at + 24])
                    if kind == 1 and code == 42 and value == 0:  # Shift release: no terminal characters
                        with open('/dev/tty8', 'rb', buffering=0) as tty:
                            termios.tcflush(tty, termios.TCIFLUSH)
                        return
        raise RuntimeError('public screenshot handshake timed out')

    def type_private(value):
        for character in value.decode('ascii') + '\n':
            code = codes[character]
            for down in (1, 0):
                os.write(keyboard, struct.pack('llHHi', 0, 0, 1, code, down))
                os.write(keyboard, struct.pack('llHHi', 0, 0, 0, 0, 0))
                time.sleep(0.03)

    try:
        passphrase = (SEED / 'passphrase').read_bytes()
        wait_for(b'Re-enter the disk passphrase')
        safe_capture('COMPLETION-SAFE-SCREEN')
        type_private(passphrase)
        wait_for(b'Press Enter when it is saved offline.')
        # Read character cells privately. Never take a screenshot while the
        # key is visible and never send the key to the host or serial console.
        match = re.search(rb'[cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}', screen())
        assert match is not None, 'recovery key was not shown locally'
        recovery = match.group(0)
        type_private(b'')
        wait_for(b'Enter the saved recovery key to confirm')
        type_private(recovery)
        wait_for(b'Recovery key saved and privately verified.')
        assert recovery not in screen()
        safe_capture('RECOVERY-SAFE-SCREEN')
        type_private(b'')
        while not (PRIVATE / 'final-scan').exists():
            time.sleep(0.2)
        result = scan([passphrase, recovery])
        (PUBLIC / 'leak-scan.json').write_text(json.dumps(result))
        assert not result['secret_found'], 'secret found; matching bytes and lines suppressed'
        (PRIVATE / 'scanned').touch()
    finally:
        fcntl.ioctl(keyboard, 0x5502)
        os.close(keyboard)
        for fd in input_fds:
            os.close(fd)


def finish():
    root = pathlib.Path('/mnt/sysroot')
    (PRIVATE / 'storage.ks').unlink(missing_ok=True)
    for name, command in [
        ('probe.json', ['ilex-enroll', 'probe', '--json']),
        ('layout.json', ['ilex-enroll', 'layout', '--root', str(root)]),
        ('versions.txt', ['cat', '/usr/lib/ilex/security/tools.json']),
    ]:
        output = subprocess.run(command, capture_output=True, check=True)
        (PUBLIC / name).write_bytes(output.stdout)
    (PUBLIC / 'eventlog.bin').write_bytes(pathlib.Path('/sys/kernel/security/tpm0/binary_bios_measurements').read_bytes())
    (PUBLIC / 'enrollment.json').write_bytes((root / 'var/lib/ilex/security/enrollment.json').read_bytes())
    (PUBLIC / 'setup.json').write_bytes((root / 'etc/ilex/setup.json').read_bytes())
    (PRIVATE / 'final-scan').touch()
    start = time.monotonic()
    while not (PRIVATE / 'scanned').exists():
        assert time.monotonic() - start < 120, 'private leak scan failed or timed out'
        time.sleep(0.2)
    os.sync()
    marker('DONE')


if __name__ == '__main__':
    harden()
    try:
        {'prepare': prepare, 'drive': drive, 'finish': finish}[sys.argv[1]]()
    except Exception:
        # Do not include exception repr: an input or tool failure may contain a
        # credential. Public artifacts expose only the failed phase.
        marker('FAILED-' + sys.argv[1])
        raise SystemExit(1)
