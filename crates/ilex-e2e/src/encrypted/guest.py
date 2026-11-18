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
import signal
import struct
import subprocess
import sys
import time
import termios

PRIVATE = pathlib.Path('/run/ilex-encrypted-test')
SEED = PRIVATE / 'seed'
PUBLIC = SEED / 'public'
DRIVE_STEP = 'not-started'


def privacy_module():
    # -I -S deliberately excludes provisioning media from Python's search path.
    scope = {'__name__': 'ilex_private_matcher'}
    exec((SEED / 'privacy.py').read_text(), scope)
    return scope


def scan_roots():
    roots = ['/tmp', '/var/tmp', '/var/log', '/var/lib/anaconda', '/root',
             '/run/anaconda', '/run/ilex-enroll', '/var/lib/systemd/coredump',
             '/var/spool/abrt', str(PUBLIC)]
    for target in ['/mnt/sysroot', '/mnt/sysimage']:
        roots.extend(target + suffix for suffix in
                     ['/etc', '/var/log', '/var/tmp', '/var/roothome',
                      '/var/lib/anaconda', '/var/lib/ilex', '/var/lib/systemd/coredump',
                      '/var/spool/abrt'])
    return roots

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
    # Anaconda calls poweroff only after its finalisation. This test-only unit
    # scans that final live/target log and kickstart set before unmounting it.
    unit = pathlib.Path('/run/systemd/system/ilex-private-final-scan.service')
    unit.write_text('[Unit]\nDefaultDependencies=no\n'
                    'After=anaconda.service anaconda-direct.service\n'
                    'Before=umount.target poweroff.target\n'
                    '[Service]\nType=oneshot\nTimeoutStartSec=1800\nExecStart=/usr/bin/python3 -I -S '
                    + str(SEED / 'guest.py') + ' finalise\n')
    subprocess.run(['systemctl', 'daemon-reload'], check=True)
    subprocess.run(['systemctl', 'add-wants', 'poweroff.target', unit.name], check=True)
    marker('PREPARED')


def screen():
    try:
        return pathlib.Path('/dev/vcs8').read_bytes()
    except OSError:
        return b''


def wait_for(text, deadline=1800):
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


def scan():
    fingerprints = json.loads((PUBLIC / 'credentials.json').read_text())
    return privacy_module()['scan'](scan_roots(), fingerprints, journal=True)


def finalise():
    result = scan()
    result['phase'] = 'anaconda-poweroff-after-finalisation'
    (PUBLIC / 'finaliser-leak-scan.json').write_text(json.dumps(result))
    os.sync()
    assert not result['secret_found'] and not result['read_errors'], 'private final scan failed'


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
        # Only an OAEP envelope leaves this guest. Its throwaway private key
        # stays in the host's 0700 fixture; decrypted bytes go to locked memory.
        envelope = subprocess.run(
            ['openssl', 'pkeyutl', '-encrypt', '-pubin', '-inkey', str(SEED / 'recovery.pub'),
             '-pkeyopt', 'rsa_padding_mode:oaep', '-pkeyopt', 'rsa_oaep_md:sha256'],
            input=recovery, capture_output=True, check=True).stdout
        (SEED / 'recovery.oaep').write_bytes(envelope)
        matcher = privacy_module()
        # Both matchers must catch overlapping candidates across a read boundary
        # inside this VM. The intentional private probe is never a log/export.
        check_dir = PRIVATE / 'matcher-self-check'
        check_dir.mkdir()
        check = check_dir / 'not-an-image.png'
        try:
            for value in [passphrase, recovery]:
                check.write_bytes(b'X' * (1024 * 1024 - 3) + value)
                report = matcher['scan']([check_dir], [matcher['fingerprint'](value)])
                assert report['secret_found'], 'private matcher self-check failed'
                check.unlink()
                assert not matcher['scan']([check_dir], [matcher['fingerprint'](value)])['secret_found']
                for codec in ['gzip', 'lzma', 'bz2', 'zstd', 'zipfile']:
                    check = check_dir / codec
                    if codec == 'zipfile':
                        with matcher[codec].ZipFile(check, 'w', compression=matcher[codec].ZIP_DEFLATED) as archive:
                            archive.writestr('private-fixture.log', value)
                    else:
                        assert matcher[codec] is not None, 'private decompressor unavailable'
                        check.write_bytes(matcher[codec].compress(value))
                    report = matcher['scan']([check_dir], [matcher['fingerprint'](value)])
                    assert report['secret_found'] and not report['read_errors'], 'private compressed matcher failed'
                    check.unlink()
                check = check_dir / 'not-an-image.png'
        finally:
            check.unlink(missing_ok=True)
            check_dir.rmdir()
        (PUBLIC / 'credentials.json').write_text(json.dumps(
            [matcher['fingerprint'](value) for value in [passphrase, recovery]]))
        config = json.loads((SEED / 'config.json').read_text())
        if config.get('interrupt_completion', False):
            # Kill the shipped completion after its recovery slot/state commit,
            # before acknowledgement or TPM enrollment. No fake helper or ISO.
            found = []
            for path in pathlib.Path('/proc').glob('[0-9]*/cmdline'):
                try:
                    args = path.read_bytes().split(b'\0')
                except OSError:
                    continue
                if (path.parent.name != str(os.getpid()) and args and
                        args[0].endswith(b'/ilex-enroll') and b'complete' in args):
                    found.append(int(path.parent.name))
            assert len(found) == 1, 'ambiguous completion process'
            os.kill(found[0], signal.SIGKILL)
            time.sleep(2)
            root = pathlib.Path('/mnt/sysroot')
            state = json.loads((root / 'var/lib/ilex/security/enrollment.json').read_text())
            assert state['state'] == 'recovery-incomplete'
            assert state['recovery']['acknowledged'] is False and state['tpm'] is None
            output = subprocess.run(['ilex-enroll', 'status', '--json', '--device',
                                     '/dev/disk/by-uuid/' + state['uuid']],
                                    capture_output=True, check=True)
            tokens = json.loads(output.stdout)
            assert len(tokens['slots']) == 2 and len(tokens['tokens']) == 1
            assert tokens['tokens'][0]['type'] == 'systemd-recovery'
            assert '0' in tokens['slots']
            assert tokens['tokens'][0]['id'] == str(state['recovery']['token'])
            assert tokens['tokens'][0]['keyslots'] == [str(state['recovery']['slot'])]
            assert sorted(p.name for p in (root / 'var/lib/ilex/security').iterdir()) == ['enrollment.json']
            (PUBLIC / 'interrupted-tokens.json').write_text(json.dumps(tokens))
            (PRIVATE / 'driver-done').touch()
            finish()
            subprocess.run(['systemctl', 'poweroff'], check=True)
            return
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
        (PRIVATE / 'driver-done').touch()
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
    state = json.loads((PUBLIC / 'enrollment.json').read_text())
    tokens = subprocess.run(['ilex-enroll', 'status', '--json', '--device',
                             '/dev/disk/by-uuid/' + state['uuid']], capture_output=True, check=True)
    (PUBLIC / 'tokens.json').write_bytes(tokens.stdout)
    start = time.monotonic()
    while not (PRIVATE / 'driver-done').exists():
        assert time.monotonic() - start < 120, 'private leak scan failed or timed out'
        time.sleep(0.2)
    result = scan()
    (PUBLIC / 'leak-scan.json').write_text(json.dumps(result))
    assert not result['secret_found'] and not result['read_errors'], 'private scan failed; details suppressed'
    os.sync()
    marker('DONE')


if __name__ == '__main__':
    harden()
    try:
        {'prepare': prepare, 'drive': drive, 'finish': finish, 'finalise': finalise}[sys.argv[1]]()
    except Exception as error:
        # Do not include exception repr: an input or tool failure may contain a
        # credential. Public artifacts expose only the failed phase.
        try:
            failure_snapshot(error)
        except Exception:
            pass
        marker('FAILED-' + sys.argv[1])
        raise SystemExit(1)
