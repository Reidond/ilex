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
import tty

PRIVATE = pathlib.Path('/run/ilex-encrypted-test')
SEED = PRIVATE / 'seed'
PUBLIC = SEED / 'public'
DRIVE_STEP = 'not-started'
KEY_CODES = dict(zip(b'abcdefghijklmnopqrstuvwxyz',
                     [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
                      49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44]))
KEY_CODES[ord('-')] = 12
KEY_CODES[ord('\n')] = 28


def privacy_module():
    # -I -S deliberately excludes provisioning media from Python's search path.
    scope = {'__name__': 'ilex_private_matcher'}
    exec((SEED / 'privacy.py').read_text(), scope)
    return scope


def scan_roots():
    roots = ['/tmp', '/var/tmp', '/var/log', '/var/lib/anaconda', '/root',
             '/run/anaconda', '/run/ilex', '/run/ilex-enroll', '/var/lib/systemd/coredump',
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


def emit_diagnostics(records):
    scope = {'__name__': 'ilex_fixture_diagnostics'}
    exec((SEED / 'diagnostics.py').read_text(), scope)
    with (PRIVATE / 'diagnostic-uart.lock').open('wb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with open('/dev/ttyS0', 'wb') as serial:
            for name, data in records.items():
                scope['emit'](serial, name, data)


def mirror_diagnostics():
    # Mirror bounded, redacted text on the installer UART. Enrollment still
    # reads credentials only from its reserved local virtual console.
    scope = {'__name__': 'ilex_fixture_diagnostics'}
    exec((SEED / 'diagnostics.py').read_text(), scope)
    sources = {name: pathlib.Path('/tmp') / name for name in
               ('anaconda.log', 'program.log', 'storage.log', 'packaging.log')}
    sources['driver.log'] = PRIVATE / 'driver.log'
    root = pathlib.Path('/mnt/sysroot')
    if not (root / 'etc').is_dir():
        root = pathlib.Path('/mnt/sysimage')
    sources['preload.log'] = root / 'var/log/anaconda/ilex-apps.log'
    records = {name: scope['tail'](path) for name, path in sources.items()}
    post_logs = sorted(pathlib.Path('/tmp').glob('ks-script-*.log'))
    records['post.log'] = b'\n'.join(scope['tail'](path) for path in post_logs[-12:])
    if (root / 'etc').is_dir():
        try:
            result = subprocess.run(['ilex-enroll', 'layout', '--root', str(root)],
                                    capture_output=True, timeout=15)
            records['post.log'] += b'\n' + result.stderr
            layout = json.loads(result.stdout) if result.returncode == 0 else None
            if layout and re.fullmatch('[0-9a-f-]{36}', layout['uuid']):
                dump = subprocess.run(['cryptsetup', 'luksDump', '/dev/disk/by-uuid/' + layout['uuid']],
                                      capture_output=True, timeout=15)
                records['luksDump.txt'] = dump.stdout + dump.stderr
        except (OSError, KeyError, ValueError, subprocess.TimeoutExpired):
            pass
    emit_diagnostics(records)


def diagnostic_loop():
    while True:
        try:
            mirror_diagnostics()
        except Exception:
            pass
        time.sleep(120)


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
    # Keep an active oneshot until shutdown. Stop ordering is reversed: with
    # Before=anaconda and After=the seed mount, ExecStop runs after Anaconda
    # stops but before the seed is unmounted. A new poweroff-start job alone
    # would race the mount's stop job and could lose the final report.
    unit.write_text('[Unit]\nDefaultDependencies=no\n'
                    'Before=anaconda.service anaconda-direct.service shutdown.target\n'
                    'Conflicts=shutdown.target\nRequiresMountsFor=' + str(SEED) + ' /tmp /var/log /var/lib/anaconda\n'
                    '[Service]\nType=oneshot\nRemainAfterExit=yes\n'
                    'TimeoutStopSec=1800\nExecStart=/usr/bin/true\n'
                    'ExecStop=/usr/bin/python3 -I -S ' + str(SEED / 'guest.py') + ' finalise\n')
    subprocess.run(['systemctl', 'daemon-reload'], check=True)
    subprocess.run(['systemctl', 'start', unit.name], check=True)
    subprocess.run(['systemd-run', '--quiet', '--no-block',
                    '--unit=ilex-encrypted-diagnostic-serial',
                    '/usr/bin/python3', '-I', '-S', str(SEED / 'guest.py'), 'diagnostics'],
                   capture_output=True)
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


def memory_sample():
    sample = {}
    sources = {
        '/proc/meminfo': ('MemTotal', 'MemAvailable', 'Dirty', 'Writeback', 'SwapTotal', 'SwapFree'),
        '/proc/vmstat': ('oom_kill', 'pswpin', 'pswpout'),
    }
    for path, keys in sources.items():
        try:
            for line in pathlib.Path(path).read_text().splitlines():
                key, value, *_ = line.split()
                if key.rstrip(':') in keys:
                    sample[key.rstrip(':')] = int(value)
        except (OSError, ValueError):
            pass
    try:
        sample['zram_devices'] = sum(line.startswith('/dev/zram')
                                     for line in pathlib.Path('/proc/swaps').read_text().splitlines())
        sample['memory_pressure_us'] = {
            kind: int(total)
            for kind, total in re.findall(r'^(some|full) .* total=(\d+)$',
                                          pathlib.Path('/proc/pressure/memory').read_text(), re.M)
        }
    except OSError:
        pass
    return sample


def scan_report(name, phase):
    stats = memory_sample()
    start = time.monotonic()
    try:
        fingerprints = json.loads((PUBLIC / 'credentials.json').read_text())
        result = privacy_module()['scan'](scan_roots(), fingerprints, journal=True)
    except Exception as error:
        result = {'schema': 2, 'files_scanned': 0, 'images_excluded': 0,
                  'read_errors': 1, 'journal_scanned': False, 'secret_found': False,
                  'credentials_checked': 0, 'findings': [
                      {'rule': 'verification-error', 'path': 'installer',
                       'region': phase, 'error': type(error).__name__}]}
    result.update(phase=phase, elapsed_seconds=round(time.monotonic() - start, 3),
                  memory_before=stats, memory_after=memory_sample())
    report = json.dumps(result, ensure_ascii=False).encode()
    (PUBLIC / name).write_bytes(report)
    emit_diagnostics({name: report})
    return result


def require_private(result):
    assert not result['secret_found'] and not result['read_errors'] and result['journal_scanned'], (
        'private guest scan failed')


def finalise():
    marker('FINALISER-STARTED')
    try:
        mirror_diagnostics()
    except Exception:
        pass
    result = scan_report('finaliser-leak-scan.json', 'anaconda-poweroff-after-finalisation')
    os.sync()
    require_private(result)
    marker('FINALISER-DONE')


def open_keyboard():
    global DRIVE_STEP
    DRIVE_STEP = 'uinput-load'
    subprocess.run(['modprobe', 'uinput'], check=True)
    DRIVE_STEP = 'uinput-open'
    keyboard = os.open('/dev/uinput', os.O_WRONLY | os.O_NONBLOCK)
    DRIVE_STEP = 'uinput-create'
    try:
        # Linux uinput ABI. This device exists only in this disposable guest.
        fcntl.ioctl(keyboard, 0x40045564, 1)  # UI_SET_EVBIT(EV_KEY)
        for code in set(KEY_CODES.values()):
            fcntl.ioctl(keyboard, 0x40045565, code)
        os.write(keyboard, struct.pack('80sHHHHI', b'ilex-private-test-keyboard', 3, 1, 1, 1, 0) + bytes(1024))
        fcntl.ioctl(keyboard, 0x5501)  # UI_DEV_CREATE
        return keyboard
    except Exception:
        os.close(keyboard)
        raise


def type_private(keyboard, value):
    for code in [KEY_CODES[character] for character in value] + [KEY_CODES[ord('\n')]]:
        for down in (1, 0):
            os.write(keyboard, struct.pack('llHHi', 0, 0, 1, code, down))
            os.write(keyboard, struct.pack('llHHi', 0, 0, 0, 0, 0))
            time.sleep(0.03)


def vt_input(length):
    # Installed-guest fixture. The host sends bytes through its private
    # serial-input socket after the helper puts the UART in no-echo raw mode.
    # The input still reaches ilex-enroll through the real kernel VT keyboard.
    assert length in (48, 71) and pathlib.Path('/sys/class/tty/tty0/active').read_text().strip() == 'tty8'
    keyboard = open_keyboard()
    data = bytearray()
    try:
        with open('/dev/ttyS0', 'rb', buffering=0) as serial:
            settings = termios.tcgetattr(serial)
            try:
                tty.setraw(serial)
                (PRIVATE / 'vt-input-ready').touch()
                deadline = time.monotonic() + 30
                while len(data) < length + 1:
                    assert select.select([serial], [], [], max(0, deadline - time.monotonic()))[0]
                    chunk = serial.read(length + 1 - len(data))
                    assert chunk
                    data.extend(chunk)
                assert data[-1] == ord('\n') and all(char in KEY_CODES and char != ord('\n')
                                                     for char in memoryview(data)[:-1])
                type_private(keyboard, memoryview(data)[:-1])
            finally:
                termios.tcsetattr(serial, termios.TCSANOW, settings)
    finally:
        data[:] = bytes(len(data))
        fcntl.ioctl(keyboard, 0x5502)
        os.close(keyboard)


def completion_pids():
    found = []
    for path in pathlib.Path('/proc').glob('[0-9]*/cmdline'):
        try:
            args = path.read_bytes().split(b'\0')
        except OSError:
            continue
        if (path.parent.name != str(os.getpid()) and args and
                args[0].endswith(b'/ilex-enroll') and b'complete' in args):
            found.append(int(path.parent.name))
    return found


def drive():
    global DRIVE_STEP
    # This %post starts before Flatpak preload. Its independent download
    # deadline can consume the whole passphrase wait on slow guests. The
    # host still enforces the overall install deadline.
    DRIVE_STEP = 'wait-enrollment-start'
    while not completion_pids():
        time.sleep(0.5)
    keyboard = open_keyboard()
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

    try:
        passphrase = (SEED / 'passphrase').read_bytes()
        DRIVE_STEP = 'wait-passphrase'
        wait_for(b'Re-enter the disk passphrase')
        DRIVE_STEP = 'capture-before-input'
        safe_capture('COMPLETION-SAFE-SCREEN')
        DRIVE_STEP = 'enter-passphrase'
        type_private(keyboard, passphrase)
        DRIVE_STEP = 'wait-recovery'
        wait_for(b'Press Enter when it is saved offline.')
        # Read character cells privately. Never take a screenshot while the
        # key is visible and never send the key to the host or serial console.
        match = re.search(rb'[cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}', screen())
        assert match is not None, 'recovery key was not shown locally'
        recovery = match.group(0)
        DRIVE_STEP = 'export-recovery'
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
            DRIVE_STEP = 'interrupt-completion'
            # Kill the shipped completion after its recovery slot/state commit,
            # before acknowledgement or TPM enrollment. No fake helper or ISO.
            found = completion_pids()
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
        type_private(keyboard, b'')
        wait_for(b'Enter the saved recovery key to confirm')
        type_private(keyboard, recovery)
        DRIVE_STEP = 'wait-ready'
        wait_for(b'Recovery key saved and privately verified.')
        assert recovery not in screen()
        DRIVE_STEP = 'capture-confirmation'
        safe_capture('RECOVERY-SAFE-SCREEN')
        type_private(keyboard, b'')
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
    if not json.loads((SEED / 'config.json').read_text()).get('local_diagnostic'):
        (PUBLIC / 'eventlog.bin').write_bytes(pathlib.Path('/sys/kernel/security/tpm0/binary_bios_measurements').read_bytes())
    (PUBLIC / 'enrollment.json').write_bytes((root / 'var/lib/ilex/security/enrollment.json').read_bytes())
    (PUBLIC / 'setup.json').write_bytes((root / 'etc/ilex/setup.json').read_bytes())
    state = json.loads((PUBLIC / 'enrollment.json').read_text())
    layout = json.loads((PUBLIC / 'layout.json').read_text())
    assert layout['uuid'] == state['uuid'], 'preload/enrollment changed the encrypted root layout'
    apps = root / 'var/lib/ilex/apps'
    before = json.loads((apps / 'installer-mounts-before').read_text())
    after = json.loads((apps / 'installer-mounts-after').read_text())
    assert before == after, 'preload changed the installer mount namespace'
    cleanup = (apps / 'installer-cleanup-result').read_text().strip()
    assert cleanup == '0', 'preload namespace cleanup failed'
    (PUBLIC / 'preload-check.json').write_text(json.dumps({
        'unchanged_mounts': True, 'cleanup': 0, 'root_uuid': layout['uuid'],
        'apps_result': int((apps / 'installer-result').read_text()),
    }))
    tokens = subprocess.run(['ilex-enroll', 'status', '--json', '--device',
                             '/dev/disk/by-uuid/' + state['uuid']], capture_output=True, check=True)
    (PUBLIC / 'tokens.json').write_bytes(tokens.stdout)
    start = time.monotonic()
    while not (PRIVATE / 'driver-done').exists():
        assert time.monotonic() - start < 120, 'private leak scan failed or timed out'
        time.sleep(0.2)
    require_private(scan_report('leak-scan.json', 'installer-checkpoint'))
    try:
        mirror_diagnostics()
    except Exception:
        pass
    os.sync()
    marker('DONE')


if __name__ == '__main__':
    harden()
    try:
        {'prepare': prepare, 'drive': drive, 'finish': finish, 'finalise': finalise,
         'diagnostics': diagnostic_loop}[sys.argv[1]]()
    except Exception as error:
        # Do not include exception repr: an input or tool failure may contain a
        # credential. Public artifacts expose only the failed phase.
        try:
            failure_snapshot(error)
        except Exception:
            pass
        try:
            mirror_diagnostics()
        except Exception:
            pass
        marker('FAILED-' + sys.argv[1])
        raise SystemExit(1)
