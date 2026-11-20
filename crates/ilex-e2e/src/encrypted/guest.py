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
import shlex
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
SYSTEMD_UNITS = pathlib.Path('/run/systemd/system')
FINAL_SCAN_UNIT = 'ilex-private-final-scan.service'
FINAL_SCAN_TARGETS = PRIVATE / 'final-scan-targets'
INSTALL_TARGETS = ('/mnt/sysroot', '/mnt/sysimage')
DRIVE_STEP = 'not-started'
KEY_CODES = dict(zip(b'abcdefghijklmnopqrstuvwxyz',
                     [30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
                      49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44]))
KEY_CODES[ord('-')] = 12
KEY_CODES[ord('\n')] = 28


def fixture_module(filename):
    # -I -S deliberately excludes provisioning media from Python's search path.
    scope = {'__name__': 'ilex_fixture_module'}
    exec((SEED / 'log_stream.py').read_text(), scope)
    exec((SEED / filename).read_text(), scope)
    return scope


def privacy_module():
    return fixture_module('privacy.py')


def scan_roots():
    roots = ['/tmp', '/var/tmp', '/var/log', '/var/lib/anaconda', '/root',
             '/run/anaconda', '/run/ilex', '/run/ilex-enroll', '/var/lib/systemd/coredump',
             '/var/spool/abrt', str(PUBLIC)]
    for target in INSTALL_TARGETS:
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
    'luks-backing': b'backing device is not a direct dm-crypt LUKS2 mapping',
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
    scope = fixture_module('diagnostics.py')
    with (PRIVATE / 'diagnostic-uart.lock').open('wb') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        with open('/dev/ttyS0', 'wb') as serial:
            for name, data in records.items():
                scope['emit'](serial, name, data)


def mirror_diagnostics():
    # Mirror bounded, redacted text on the installer UART. Enrollment still
    # reads credentials only from its reserved local virtual console.
    scope = fixture_module('diagnostics.py')
    sources = {name: pathlib.Path('/tmp') / name for name in
               ('anaconda.log', 'program.log', 'storage.log', 'packaging.log')}
    sources['driver.log'] = PRIVATE / 'driver.log'
    root = pathlib.Path('/mnt/sysroot')
    if not (root / 'etc').is_dir():
        root = pathlib.Path('/mnt/sysimage')
    sources['preload.log'] = root / 'var/log/anaconda/ilex-apps.log'
    records = {}
    for name, path in sources.items():
        try:
            records[name] = scope['tail'](path)
        except (OSError, ValueError):
            continue
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
    unit = SYSTEMD_UNITS / FINAL_SCAN_UNIT
    # Keep an active oneshot until shutdown. Stop ordering is reversed: with
    # ExecStop runs after Anaconda stops and before the seed, log mounts and
    # journald are stopped. A new poweroff-start job alone could race teardown.
    unit.write_text('[Unit]\nDefaultDependencies=no\n'
                    'Before=anaconda.service anaconda-direct.service shutdown.target\n'
                    'After=systemd-journald.service systemd-journal-flush.service\n'
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
        pinned = {}
        mounts = {}
        if name == 'finaliser-leak-scan.json' and (PRIVATE / 'interruption-requested').exists():
            scope = json.loads((PRIVATE / 'interrupted-target-mounts.json').read_text())
            pinned, mounts = scope['paths'], scope['mounts']
            assert pinned and mounts, 'interrupted target mount snapshot missing'
        roots = list(dict.fromkeys(scan_paths(pinned)))
        labels = {alias: root for root, alias in pinned.items()}
        before = missing_mounts(mounts)
        fingerprints = json.loads((PUBLIC / 'credentials.json').read_text())
        matcher = privacy_module()
        result = matcher['scan'](roots, fingerprints, journal=True,
                                 sync_journal=name == 'finaliser-leak-scan.json',
                                 path_labels=labels)
        if mounts:
            after = missing_mounts(mounts)
            result.update(target_directories_checked=sum(
                              is_target_root(root) for root in scan_roots()),
                          target_mounts_retained=not (before or after))
            for absent, region in ((before, 'before-shutdown-scan'), (after, 'after-shutdown-scan')):
                for path in sorted(absent):
                    result['findings'].append({'rule': 'scan-scope-changed',
                                               'path': matcher['public_path'](path, labels),
                                               'region': region})
            if before or after:
                result['read_errors'] += 1
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


def pinned_mounts():
    mounts = {}
    prefix = str(FINAL_SCAN_TARGETS) + '/'
    for line in pathlib.Path('/proc/self/mountinfo').read_text(errors='surrogateescape').splitlines():
        fields = line.split(' ', 5)
        path = re.sub(r'\\([0-7]{3})', lambda encoded: chr(int(encoded[1], 8)), fields[4])
        if path.startswith(prefix):
            mounts[path] = int(fields[0])
    return mounts


def missing_mounts(expected):
    if not expected:
        return set()
    current = pinned_mounts()
    return {path for path, mount_id in expected.items() if current.get(path) != mount_id}


def is_target_root(root):
    return any(root.startswith(target + '/') for target in INSTALL_TARGETS)


def scan_paths(pinned):
    for root in scan_roots():
        if not pinned or not is_target_root(root):
            yield root
            continue
        path = pathlib.Path(root)
        parent = next((directory for directory in path.parents if str(directory) in pinned), None)
        if root in pinned:
            parent = path
        assert parent is not None, 'target scan path has no retained ancestor'
        alias = pathlib.Path(pinned[str(parent)])
        retained = alias / path.relative_to(parent)
        assert retained.resolve().is_relative_to(alias), 'target scan path escaped retained mount'
        yield str(retained)


def hold_interrupted_target_mounts():
    global DRIVE_STEP
    # Anaconda can detach its target /var bind during shutdown independently
    # of systemd's stop job. Pin the target parents so directories created
    # later still resolve on the retained filesystem during shutdown.
    DRIVE_STEP = 'bind-interrupted-target'
    FINAL_SCAN_TARGETS.mkdir()
    canonical = {}
    pinned = {}
    parents = set()
    for target in map(pathlib.Path, INSTALL_TARGETS):
        assert target.is_dir(), 'installer target mount missing'
        for scope in ('etc', 'var'):
            directory = target / scope
            parents.add(directory if directory.is_dir() else target)
    for source in sorted(parents):
        resolved = source.resolve(strict=True)
        if resolved not in canonical:
            alias = FINAL_SCAN_TARGETS / str(len(canonical))
            alias.mkdir()
            # When a parent is absent, the target root supplies its future
            # directory without cloning the installer's /dev, /proc or /sys.
            mode = '--bind' if str(source) in INSTALL_TARGETS else '--rbind'
            subprocess.run(['mount', mode, str(resolved), str(alias)], check=True)
            subprocess.run(['mount', '--make-rprivate', str(alias)], check=True)
            assert os.path.samestat(resolved.stat(), alias.stat()), 'wrong interrupted target bind'
            canonical[resolved] = str(alias)
        pinned[str(source)] = canonical[resolved]
    list(scan_paths(pinned))  # Require coverage for every target scan path.
    DRIVE_STEP = 'verify-interrupted-target-binds'
    mounts = pinned_mounts()
    assert pinned and set(pinned.values()) <= mounts.keys(), 'interrupted target bind missing'
    DRIVE_STEP = 'order-interrupted-target-binds'
    drop_in = SYSTEMD_UNITS / (FINAL_SCAN_UNIT + '.d')
    drop_in.mkdir()
    (drop_in / '10-interrupted-target.conf').write_text(
        '[Unit]\nRequiresMountsFor=' + ' '.join(
            json.dumps(path.replace('%', '%%'), ensure_ascii=False) for path in sorted(mounts)) + '\n')
    subprocess.run(['systemctl', 'daemon-reload'], check=True)
    # systemctl quotes unit names whose escaped path contains backslashes.
    ordered = set(shlex.split(subprocess.check_output(
        ['systemctl', 'show', '-p', 'After', '--value', FINAL_SCAN_UNIT], text=True)))
    missing = []
    for path in mounts:
        unit = subprocess.check_output(['systemd-escape', '--path', '--suffix=mount', path],
                                       text=True).strip()
        if unit not in ordered:
            missing.append(path)
    if missing:
        matcher = privacy_module()
        labels = {alias: root for root, alias in pinned.items()}
        print(json.dumps({'rule': 'mount-order-missing',
                          'paths': [matcher['public_path'](path, labels) for path in missing],
                          'manager_namespace': os.readlink('/proc/self/ns/mnt') ==
                          os.readlink('/proc/1/ns/mnt')}), flush=True)
        raise RuntimeError('interrupted target bind lacks shutdown ordering')
    (PRIVATE / 'interrupted-target-mounts.json').write_text(
        json.dumps({'paths': pinned, 'mounts': mounts}))


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
            (PRIVATE / 'interruption-requested').touch()
            pidfd = os.pidfd_open(found[0])
            try:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                assert select.select([pidfd], [], [], 60)[0], 'completion did not exit after SIGKILL'
            finally:
                os.close(pidfd)
            marker('INTERRUPTION-EXITED')
            DRIVE_STEP = 'check-interrupted-slots'
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
            hold_interrupted_target_mounts()
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
