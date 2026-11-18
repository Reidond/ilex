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
DRIVE_STEP = 'not-started'

ERRORS = {
    'var-unencrypted': b'/var does not share encrypted root',
    'root-layout': b'requires one LUKS2 ancestor and btrfs root',
    'boot-encrypted': b'ESP and /boot must be outside LUKS',
    'crypttab': b'root crypttab is ambiguous or contains a keyfile',
    'swap': b'disk swap',
    'text-console': b'completion requires a text-mode virtual console',
    'tty-required': b'completion requires the reserved local tty',
    'tool-failed': b'enrollment tool failed',
    'memory-lock': b'cannot lock secret memory',
    'btrfs-devices': b'requires single-device Btrfs filesystem',
    'btrfs-inspect': b'cannot inspect Btrfs devices',
    'mount-selection': b'ambiguous mount',
    'mount-id': b'cannot identify containing mount',
}


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
    if config.get('preserve_data', False):
        # This fixture owns a freshly created blank vda. Seed one public data
        # partition, then ask Anaconda to use only the remaining free space.
        with open('/dev/vda', 'rb') as disk:
            assert disk.read(4096) == bytes(4096), 'fixture disk is not blank'
        subprocess.run(['sfdisk', '/dev/vda'],
                       input=b'label: gpt\nstart=2048,size=524288,type=linux,name=ilex-e2e-keep\n',
                       capture_output=True, check=True)
        subprocess.run(['udevadm', 'settle'], check=True)
        subprocess.run(['mkfs.ext4', '-q', '-F', '/dev/vda1'], capture_output=True, check=True)
        mount = PRIVATE / 'preserved-data'
        mount.mkdir()
        subprocess.run(['mount', '/dev/vda1', str(mount)], check=True)
        try:
            with (mount / 'sentinel').open('w') as output:
                output.write('ilex-public-sentinel-v1\n')
                output.flush()
                os.fsync(output.fileno())
        finally:
            subprocess.run(['umount', str(mount)], check=True)
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


def failure_snapshot(error):
    """Export only fixed codes/booleans; never an exception, log or screen."""
    observed = set()
    helper_errno = None
    for path in pathlib.Path('/tmp').glob('ks-script-*.log'):
        if path.is_symlink() or not path.is_file():
            continue
        with path.open('rb') as source:
            data = source.read(2 * 1024 * 1024)
        if b'ilex-enroll:' not in data:
            continue
        observed.update(code for code, text in ERRORS.items() if text in data)
        match = re.search(rb'os error ([0-9]{1,3})', data)
        if match:
            helper_errno = int(match.group(1))
    root = pathlib.Path('/mnt/sysroot')
    if not (root / 'etc').is_dir():
        root = pathlib.Path('/mnt/sysimage')
    layout = 'not-present'
    if root.is_dir():
        try:
            result = subprocess.run(['ilex-enroll', 'layout', '--root', str(root)],
                                    capture_output=True, timeout=30)
            if result.returncode:
                layout = 'error'
                observed.update(code for code, text in ERRORS.items() if text in result.stderr)
            else:
                layout = 'unencrypted' if json.loads(result.stdout) is None else 'encrypted'
        except (subprocess.TimeoutExpired, OSError, ValueError):
            layout = 'unavailable'
    mode = None
    try:
        tty = os.open('/dev/tty8', os.O_RDWR | os.O_NOCTTY)
        try:
            mode = struct.unpack('i', fcntl.ioctl(tty, 0x4b3b, bytes(4)))[0]
        finally:
            os.close(tty)
    except OSError:
        pass
    cells = screen()
    report = {'schema': 1, 'phase': sys.argv[1], 'drive_step': DRIVE_STEP,
              'errno': error.errno if isinstance(error, OSError) else None,
              'helper_errno': helper_errno, 'uinput_loaded': pathlib.Path('/sys/module/uinput').exists(),
              'tty8_mode': mode, 'target_exists': (root / 'etc').is_dir(), 'layout': layout,
              'helper_errors': sorted(observed),
              'passphrase_prompt': b'Re-enter the disk passphrase' in cells,
              'recovery_prompt': b'Press Enter when it is saved offline.' in cells,
              'confirmation_prompt': b'Enter the saved recovery key to confirm' in cells,
              'ready_prompt': b'Installation completion is ready.' in cells}
    with (PUBLIC / 'failure.json').open('w') as output:
        json.dump(report, output)
        output.flush()
        os.fsync(output.fileno())
    os.sync()


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
    global DRIVE_STEP
    DRIVE_STEP = 'uinput-load'
    subprocess.run(['modprobe', 'uinput'], check=True)
    DRIVE_STEP = 'uinput-open'
    keyboard = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK)
    DRIVE_STEP = 'uinput-create'
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
    DRIVE_STEP = 'input-observe'
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
        DRIVE_STEP = 'wait-passphrase'
        wait_for(b'Re-enter the disk passphrase')
        DRIVE_STEP = 'capture-before-input'
        safe_capture('COMPLETION-SAFE-SCREEN')
        DRIVE_STEP = 'enter-passphrase'
        type_private(passphrase)
        DRIVE_STEP = 'wait-recovery'
        wait_for(b'Press Enter when it is saved offline.')
        # Read character cells privately. Never take a screenshot while the
        # key is visible and never send the key to the host or serial console.
        match = re.search(rb'[cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}', screen())
        assert match is not None, 'recovery key was not shown locally'
        recovery = match.group(0)
        DRIVE_STEP = 'acknowledge-recovery'
        type_private(b'')
        wait_for(b'Enter the saved recovery key to confirm')
        type_private(recovery)
        DRIVE_STEP = 'wait-ready'
        wait_for(b'Recovery key saved and privately verified.')
        assert recovery not in screen()
        DRIVE_STEP = 'capture-confirmation'
        safe_capture('RECOVERY-SAFE-SCREEN')
        type_private(b'')
        DRIVE_STEP = 'wait-final-scan'
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
    except Exception as error:
        # Do not include exception repr: an input or tool failure may contain a
        # credential. Public artifacts expose only the failed phase.
        try:
            failure_snapshot(error)
        except Exception:
            pass
        marker('FAILED-' + sys.argv[1])
        raise SystemExit(1)
