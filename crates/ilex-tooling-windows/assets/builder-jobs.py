#!/usr/bin/python3
"""Finite SSH job protocol. No shell, user paths, environment or credentials.

The maintenance key already has guest-root authority through recipe updates.
The framing and path checks prevent accidental interpolation and cross-job reuse;
they do not sandbox executable source supplied by that key holder.

Job lifecycle (state.json, including the immutable terminal result):
  submit: absent -> registered (PTY) or starting (automatic)
  client: registered -> registered (heartbeat every 15s, expires after 60s)
  attach: registered -> starting (consumes the lease, 45s to claim)
  worker: starting -> running -> collecting -> complete/cancelled
  controller: registered/starting -> cancelled or interrupted
  recovery: absent after a partial submit, or running/collecting with an
    exited unit -> recovering
  recovery owner: recovering -> interrupted (a successor claims after a crash)
Cancel intent and successful delivery are fields on the same record. A live
running unit receives TERM, then KILL after 10s. Only the worker can confirm
quiescence by entering collecting. Collection takes at most 300s in the
worker or 120s in recovery, including cleanup. The unit has an 8h runtime
limit. The controller may retire an expired or abandoned registration.
Each transition compares the record under a short dispatch lock and makes one
atomic replacement. A recovery flock fences publication and is released on exit.
Maintenance fences registered/starting leases and retries named resource
cleanup separately from the immutable result and the active capacity lease.
"""
import contextlib
import fcntl
import grp
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import sqlite3
import stat
import subprocess
import sys
import tarfile
import tempfile
import time
import traceback
import uuid
import xml.etree.ElementTree as ET

BASE = Path('/var/lib/ilex-data')
CHUNKS = BASE / 'chunks'
JOBS = BASE / 'jobs'
KACHE_IMPORT_LAYOUT = 'kache-imports/read-only-v1'
MAX_FRAME = 32 * 1024 * 1024
CHUNK = 4 * 1024 * 1024
OEMDRV_BYTES = 32 * 1024 * 1024
TERMINAL_LEASE_SECONDS = 60
START_SECONDS = 45
CANCEL_GRACE_SECONDS = 10
CANCEL_WAIT_SECONDS = 18
KILL_RETRY_SECONDS = 2
EVIDENCE_SECONDS = 300
RECOVERY_SECONDS = 120
CLEANUP_SECONDS = 25
MAINTENANCE_SECONDS = 30
JOB_SECONDS = 8 * 60 * 60
CONSOLE_ROOT = Path('/var/log/libvirt/qemu/ilex-jobs')
TERMINAL_STATES = {'complete', 'cancelled', 'interrupted'}
ENV = {'PATH': '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin',
       'HOME': '/root', 'LANG': 'C.UTF-8', 'XDG_RUNTIME_DIR': '/run/ilex-builder',
       'CARGO_HOME': str(BASE / 'cargo/home'), 'KACHE_CACHE_DIR': str(BASE / 'kache')}
ALLOWED_ENV = {'ILEX_IMAGE', 'ILEX_UPGRADE_FROM', 'ILEX_SHELLS', 'ILEX_MACHINE',
               'ILEX_UPGRADE_PREPARE', 'ILEX_SIGPOLICY_LEGACY', 'ILEX_TOOL_REBUILD',
               'ILEX_ISO_MACHINE', 'ILEX_ISO_EXPECT', 'ILEX_ISO_TARGET',
               'ILEX_ISO_DIGEST', 'ILEX_ISO_INSTALL_TIMEOUT', 'ILEX_VM_SLOTS', 'ILEX_KACHE_IMPORT'}


def encode(value):
    return json.dumps(value, ensure_ascii=False, separators=(',', ':')).encode()


def digest(data):
    return 'sha256:' + hashlib.sha256(data).hexdigest()


def sha(value):
    assert isinstance(value, str) and re.fullmatch('sha256:[0-9a-f]{64}', value), 'invalid digest'
    return value[7:]


def ident(value):
    assert str(uuid.UUID(value)) == value, 'invalid owner/job UUID'
    return value


def relative(value):
    assert isinstance(value, str) and value and '\\' not in value and ':' not in value, 'unsafe path'
    assert all(p not in ('', '.', '..', '.git') for p in value.split('/')), 'unsafe path component'
    assert not PurePosixPath(value).is_absolute() and '\x00' not in value, 'absolute path'
    return value


def private(path):
    return any(p in {'.git', '.ssh', '.gnupg', '.env', 'ilex-secrets', 'credentials',
                     '.aws', '.azure', '.docker', '.kube', '.netrc', '_netrc', '.npmrc', '.pypirc', '.git-credentials', 'auth.json'}
               or p.endswith(('.key', '.pem', '.p12', '.pfx')) or p.startswith('.env.')
               for p in (part.lower() for part in Path(path).parts))


@contextlib.contextmanager
def lock(path, shared=False, wait=True, seconds=30):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+b') as f:
        end = time.monotonic() + seconds
        while True:
            try:
                fcntl.flock(f, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX) | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if not wait or time.monotonic() >= end:
                    raise
                time.sleep(.05)
        yield


def atomic(path, data):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.partial')
    with temporary.open('xb') as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temporary, path)
    fd = os.open(path.parent, os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def read_json(path):
    return json.loads(Path(path).read_bytes())


def checked(command, **kwargs):
    return subprocess.run(command, check=True, env=ENV, stdin=subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, **kwargs).stdout


def owner_dir(snapshot):
    identity = snapshot['identity']
    owner = ident(identity['owner'])
    key = ('ilex-owner-v1\0' + identity['repository_id'] + '\0' + identity['checkout_id']).encode()
    raw = bytearray(hashlib.sha256(key).digest()[:16])
    raw[6] = (raw[6] & 15) | 128
    raw[8] = (raw[8] & 63) | 128
    assert str(uuid.UUID(bytes=bytes(raw))) == owner, 'checkout owner mismatch'
    slug = identity['slug']
    assert re.fullmatch('[a-z0-9-]{1,48}', slug), 'unsafe slug'
    assert (identity['is_main'] and slug == 'main') or (
        not identity['is_main'] and slug.endswith(hashlib.sha256(owner.encode()).hexdigest()[:32])), 'namespace mismatch'
    result = BASE / 'checkouts' / owner
    result.mkdir(parents=True, exist_ok=True)
    receipt = result / 'identity.json'
    stable = {k: identity[k] for k in ('repository_id', 'checkout_id', 'owner', 'slug', 'is_main')}
    with lock(result / 'identity.lock'):
        if receipt.exists():
            assert read_json(receipt) == stable, 'foreign checkout allocation'
        else:
            atomic(receipt, encode(stable))
    return result


def validate_snapshot(snapshot):
    assert snapshot['schema'] == 1 and snapshot['source']['schema'] == 1, 'unsupported snapshot schema'
    source = snapshot['source']
    # Serde struct field order is the canonical manifest encoding.
    canonical = []
    for entry in source['entries']:
        kind = {'kind': entry['kind']['kind']}
        if kind['kind'] == 'file':
            kind['executable'] = entry['kind']['executable']
        elif kind['kind'] == 'symlink':
            kind['target'] = entry['kind']['target']
        else:
            assert kind['kind'] == 'deleted', 'unknown source entry kind'
        canonical.append({'path': entry['path'], 'kind': kind,
                          'sha256': entry['sha256'], 'bytes': entry['bytes']})
    assert digest(encode(canonical)) == source['digest'], 'manifest checksum mismatch'
    assert snapshot['identity']['snapshot_digest'] == source['digest'], 'source identity mismatch'
    assert re.fullmatch('[0-9a-f]{40}', source['revision']), 'invalid source revision'
    assert snapshot['identity']['source_revision'] == source['revision'] + ('-dirty' if source['dirty'] else ''), 'revision mismatch'
    previous = ''
    active = set()
    required = set(snapshot['git_pack'])
    for entry in source['entries']:
        path = relative(entry['path'])
        assert path > previous and not private(path), 'unsorted/private source'
        previous = path
        kind = entry['kind']
        if kind['kind'] == 'deleted':
            assert entry['sha256'] is None and entry['bytes'] == 0
            continue
        assert all(str(p) not in active for p in PurePosixPath(path).parents if str(p) != '.'), 'source parent is a leaf'
        active.add(path)
        sha(entry['sha256'])
        if kind['kind'] == 'symlink':
            target = kind['target']
            assert isinstance(target, str) and target and not target.startswith('/') and '\\' not in target and ':' not in target and '\x00' not in target, 'unsafe link'
            components = path.split('/')[:-1]
            for part in target.split('/'):
                if part == '..':
                    assert components, 'escaping source link'
                    components.pop()
                elif part not in ('', '.'):
                    components.append(part)
            assert digest(target.encode()) == entry['sha256'] and len(target.encode()) == entry['bytes']
        else:
            assert kind['kind'] == 'file' and isinstance(kind['executable'], bool), 'unknown source kind'
            required.update(snapshot['files'][path])
    for chunk in required:
        sha(chunk)
    return required


def chunk_path(value):
    return CHUNKS / sha(value)


def join_chunks(chunks, output):
    with output.open('xb') as dst:
        for value in chunks:
            data = chunk_path(value).read_bytes()
            assert digest(data) == value, 'stored chunk corrupt'
            dst.write(data)
        dst.flush()
        os.fsync(dst.fileno())


def source_parents(path, root):
    parent = root
    for part in path.relative_to(root).parts[:-1]:
        parent = parent / part
        parent.mkdir(exist_ok=True)
        assert parent.is_dir() and not parent.is_symlink(), 'source parent is not a real directory'
        # Source directory modes must not inherit the private transport umask:
        # Podman COPY carries these modes into the image's existing directories.
        parent.chmod(0o755)


def snapshot_path(snapshot):
    source = snapshot['source']
    return owner_dir(snapshot) / 'snapshots' / (sha(source['digest']) + '-' + source['revision'] + ('-dirty' if source['dirty'] else ''))


def materialize(snapshot):
    required = validate_snapshot(snapshot)
    assert all(chunk_path(c).is_file() for c in required), 'missing source chunks'
    destination = snapshot_path(snapshot)
    with lock(destination.parent / (destination.name + '.lock')):
        marker = destination.parent / (destination.name + '.json')
        if destination.exists():
            assert read_json(marker) == snapshot, 'snapshot collision/provenance mismatch'
            verify_source(destination, snapshot['source'])
            return destination
        reserve(sum(e['bytes'] for e in snapshot['source']['entries']) * 2)
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.with_name(destination.name + '.' + uuid.uuid4().hex + '.partial')
        temporary.mkdir()
        try:
            # Create files first, links last; no extraction ever follows a link.
            for entry in snapshot['source']['entries']:
                if entry['kind']['kind'] != 'file':
                    continue
                path = temporary / relative(entry['path'])
                source_parents(path, temporary)
                join_chunks(snapshot['files'][entry['path']], path)
                assert digest(path.read_bytes()) == entry['sha256'] and path.stat().st_size == entry['bytes'], 'file checksum mismatch'
                path.chmod(0o755 if entry['kind']['executable'] else 0o644)
            for entry in snapshot['source']['entries']:
                if entry['kind']['kind'] == 'symlink':
                    path = temporary / relative(entry['path'])
                    source_parents(path, temporary)
                    path.symlink_to(entry['kind']['target'])
            bundle = temporary.with_name(temporary.name + '.objects.pack')
            join_chunks(snapshot['git_pack'], bundle)
            checked(['git', 'init', '--quiet', str(temporary)])
            with bundle.open('rb') as objects:
                subprocess.run(['git', '-C', str(temporary), 'index-pack', '--stdin'], stdin=objects,
                               env=ENV, stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True)
            atomic(temporary / '.git/shallow', (snapshot['source']['revision'] + '\n').encode())
            checked(['git', '-C', str(temporary), 'update-ref', 'HEAD', snapshot['source']['revision']])
            checked(['git', '-C', str(temporary), 'read-tree', 'HEAD'])
            bundle.unlink()
            verify_source(temporary, snapshot['source'])
            # Snapshot context and original Git metadata never enter the source tree.
            atomic(marker, encode(snapshot))
            os.rename(temporary, destination)
            return destination
        except BaseException:
            temporary.with_name(temporary.name + '.objects.pack').unlink(missing_ok=True)
            shutil.rmtree(temporary)
            raise


def verify_source(root, source):
    assert root.is_dir() and not root.is_symlink(), 'snapshot root is not a real directory'
    assert (root / '.git').is_dir() and not (root / '.git').is_symlink(), 'snapshot Git metadata was replaced'
    expected = {e['path'] for e in source['entries'] if e['kind']['kind'] != 'deleted'}
    directories = {str(parent) for path in expected for parent in PurePosixPath(path).parents if str(parent) != '.'}
    for directory, children, files in os.walk(root, followlinks=False):
        directory = Path(directory)
        if directory == root:
            children[:] = [name for name in children if name != '.git']
        for name in children:
            path = directory / name
            relative_path = path.relative_to(root).as_posix()
            assert relative_path in (expected if path.is_symlink() else directories), 'unexpected source directory/link'
        for name in files:
            assert (directory / name).relative_to(root).as_posix() in expected, 'unexpected source file'
    for entry in source['entries']:
        path = root / entry['path']
        for parent in path.parents:
            if parent == root:
                break
            assert not parent.is_symlink(), 'source parent became a symlink'
            if parent.exists():
                assert stat.S_IMODE(parent.stat().st_mode) == 0o755, 'source directory mode changed'
        kind = entry['kind']
        if kind['kind'] == 'deleted':
            assert not path.exists() and not path.is_symlink(), 'deleted path exists'
            continue
        if kind['kind'] == 'symlink':
            assert path.is_symlink() and os.readlink(path) == kind['target'], 'link mismatch'
            assert path.resolve().is_relative_to(root.resolve()), 'link chain escapes snapshot'
            data = os.readlink(path).encode()
        else:
            assert path.is_file() and not path.is_symlink(), 'file type mismatch'
            assert bool(path.stat().st_mode & 0o111) == kind['executable'], 'file mode mismatch'
            data = path.read_bytes()
        assert digest(data) == entry['sha256'] and len(data) == entry['bytes'], 'source bytes mismatch'


def reserve(peak):
    assert shutil.disk_usage(BASE).free >= 30 * 1024**3 + peak, 'builder data needs 30 GiB reserve plus operation peak'


def job_path(owner, job):
    return JOBS / ident(owner) / ident(job)


def load_job(request):
    path = job_path(request['owner'], request['job'])
    spec = read_json(path / 'request.json')
    assert spec['snapshot']['identity']['owner'] == request['owner'] and spec['job'] == request['job'], 'job owner mismatch'
    return path, spec


def service(job):
    return 'ilex-job-' + ident(job) + '.service'


def unit_live(spec, *, owned=False):
    unit = service(spec['job'])
    fields = checked(['systemctl', 'show', '-p', 'ActiveState', '-p', 'Description', unit],
                     timeout=2).decode().splitlines()
    info = dict(item.split('=', 1) for item in fields)
    live = info['ActiveState'] in {'active', 'activating', 'deactivating', 'reloading'}
    if live and owned:
        owner = spec['snapshot']['identity']['owner']
        assert info['Description'] == f"ilex-job schema=1 owner={owner} job={spec['job']}", 'job unit ownership changed'
    return live


def interactive(spec):
    command = spec['command']
    return command['operation'] == 'vm' and not command['args']['command']


TRANSITIONS = {
    'submit': {None: {'registered', 'starting'}},
    'heartbeat': {'registered': {'registered'}},
    'attach': {'registered': {'starting'}},
    'worker': {'starting': {'running'}, 'running': {'collecting'},
               'collecting': {'complete', 'cancelled'}},
    'controller': {None: {'cancelled'}, 'registered': {'cancelled', 'interrupted'}, 'starting': {'cancelled', 'interrupted'},
                   'running': {'running'}, 'collecting': {'collecting'}},
    'recovery': {None: {'recovering'}, 'running': {'recovering'}, 'collecting': {'recovering'},
                 'recovering': {'recovering', 'interrupted'}},
}


def record(path, spec):
    file = path / 'state.json'
    current = read_json(file) if file.exists() else None
    if current is None or 'generation' in current:
        return current
    # Jobs made before the lifecycle record was introduced are still resumable.
    legacy = path / 'result.json'
    if legacy.is_file():
        return current | {'result': read_json(legacy)}
    phase = {'queued': 'registered' if interactive(spec) else 'starting',
             'failed': 'starting', 'cancelled': 'recovering', 'complete': 'recovering'}
    return current | {'state': phase.get(current['state'], current['state'])}


def compare_and_swap(path, spec, previous, actor, phase, **fields):
    # Callers hold dispatch.lock across the check and atomic replacement.
    assert record(path, spec) == previous, 'job record changed'
    old = previous['state'] if previous else None
    assert phase in TRANSITIONS[actor].get(old, ()), f'invalid {actor} transition {old} -> {phase}'
    next_record = (previous or {}) | fields | {'state': phase,
                  'generation': (previous or {}).get('generation', 0) + 1}
    atomic(path / 'state.json', encode(next_record))
    return next_record


def dispatch(path):
    return lock(path / 'dispatch.lock', seconds=8)


def lease(seconds):
    return {'boot': Path('/proc/sys/kernel/random/boot_id').read_text().strip(),
            'until': time.monotonic() + seconds}


def expired(current):
    deadline = current.get('lease', {})
    now = lease(0)
    return deadline.get('boot') != now['boot'] or deadline.get('until', 0) <= now['until']


def renew(owner, job):
    path = job_path(owner, job)
    if not (path / 'request.json').is_file():
        return {'lease': 'waiting'}
    path, spec = load_job({'owner': owner, 'job': job})
    with dispatch(path):
        current = record(path, spec)
        if current is None:
            return {'lease': 'waiting'}
        if current['state'] != 'registered':
            return {'lease': 'consumed'}
        if expired(current):
            finish_unclaimed(path, spec, current, 'interrupted', 'terminal registration expired')
            return {'lease': 'expired'}
        compare_and_swap(path, spec, current, 'heartbeat', 'registered', lease=lease(TERMINAL_LEASE_SECONDS))
        return {'lease': 'renewed'}


def launch(spec, terminal=False):
    owner, job = spec['snapshot']['identity']['owner'], spec['job']
    command = ['systemd-run', '--quiet', '--unit=' + service(job),
               '--description=ilex-job schema=1 owner=' + owner + ' job=' + job,
               '--property=Type=exec', '--property=KillMode=control-group',
               f'--property=RuntimeMaxSec={JOB_SECONDS}',
               f'--property=TimeoutStopSec={EVIDENCE_SECONDS + 45}', '--property=SendSIGKILL=yes']
    if terminal:
        command += ['--pty', '--collect']
    command += ['/usr/bin/python3', '-I', '-S', '/usr/local/libexec/ilex-builder-jobs',
                '--run', owner, job]
    if terminal:
        return subprocess.run(command, env=ENV | {'TERM': os.environ.get('TERM', 'xterm')},
                              timeout=JOB_SECONDS + EVIDENCE_SECONDS + 60).returncode
    checked(command, timeout=START_SECONDS)
    return 0


def terminal(owner, job):
    path, spec = load_job({'owner': owner, 'job': job})
    with lock('/run/lock/ilex-builder-jobs.lock', shared=True):
        assert interactive(spec), 'job has no guest terminal'
        with dispatch(path):
            current = record(path, spec)
            assert current and current['state'] == 'registered', 'terminal job is not registered'
            if expired(current):
                finish_unclaimed(path, spec, current, 'interrupted', 'terminal registration expired')
                raise ValueError('terminal registration expired')
            compare_and_swap(path, spec, current, 'attach', 'starting', lease=lease(START_SECONDS))
        def disconnect(_signum, _frame):
            raise InterruptedError('guest terminal disconnected')
        signal.signal(signal.SIGHUP, disconnect)
        signal.signal(signal.SIGTERM, disconnect)
        code = None
        try:
            code = launch(spec, terminal=True)
            return code
        finally:
            if code != 0:
                signal.signal(signal.SIGHUP, signal.SIG_IGN)
                signal.signal(signal.SIGTERM, signal.SIG_IGN)
                reply = cancel_job(path, spec)
                if reply['cancel'] == 'pending':
                    print('terminal disconnected; cancel pending', file=sys.stderr)


def submit(spec):
    assert spec['schema'] == 1 and spec['command']['operation'] in {'build', 'rechunk', 'disk', 'iso', 'tool', 'test', 'vm', 'import_kache', 'export', 'self_check', 'oemdrv'}, 'unsupported job schema/operation'
    assert set(spec['environment']) <= ALLOWED_ENV, 'environment not allowlisted'
    assert all(isinstance(v, str) and len(v) < 16384 and '\x00' not in v for v in spec['environment'].values()), 'invalid environment value'
    snapshot = spec['snapshot']
    root = materialize(snapshot)
    owner = snapshot['identity']['owner']
    path = job_path(owner, spec['job'])
    with dispatch(path):
        job_request = path / 'request.json'
        if job_request.is_file():
            assert read_json(job_request) == spec, 'job UUID reused with another request'
        if record(path, spec) is not None:
            return {'job': spec['job'], 'root': str(root)}
        reserve(16 * 1024**3)
        atomic(job_request, encode(spec))
        atomic(path / 'context.json', encode({'schema': 1, 'root': str(root), 'identity': snapshot['identity'], 'source': snapshot['source']}))
        attached = interactive(spec)
        initial = compare_and_swap(path, spec, None, 'submit', 'registered' if attached else 'starting',
                                   lease=lease(TERMINAL_LEASE_SECONDS if attached else START_SECONDS))
    if not attached:
        try:
            launch(spec)
        except Exception:
            with dispatch(path):
                current = record(path, spec)
                if current == initial:
                    finish_unclaimed(path, spec, current, 'interrupted', 'worker launch failed')
            raise
    return {'job': spec['job'], 'root': str(root)}


def capacity(spec):
    # One full build/test at a time on 16 GiB; smaller tool jobs may share.
    kind = spec['command']['operation']
    weight = {'tool': 4, 'self_check': 4, 'export': 4, 'disk': 10, 'rechunk': 8, 'import_kache': 4, 'oemdrv': 4}.get(kind, 12) * 1024**3
    cpus = min(4, os.cpu_count())
    if kind == 'vm':
        args = spec['command']['args']
        size = re.fullmatch(r'([1-9][0-9]*)([KMGTP]?)', (args['memory'] or '4G').upper())
        assert size, 'invalid ephemeral VM memory'
        weight = int(size[1]) * 1024**{'K': 1, 'M': 2, '': 2, 'G': 3, 'T': 4, 'P': 5}[size[2]] + 2 * 1024**3
        cpus = args['vcpus'] if args['vcpus'] is not None else 4
        assert isinstance(cpus, int) and 0 < cpus <= os.cpu_count(), 'ephemeral vCPUs exceed builder capacity'
    total = int(next(l.split()[1] for l in Path('/proc/meminfo').read_text().splitlines() if l.startswith('MemTotal:'))) * 1024
    assert weight <= total - 2 * 1024**3, 'builder is too small for this operation; use builder up --memory-gib 16 while off'
    cap = min(4, max(1, total // (8 * 1024**3)))
    if 'ILEX_VM_SLOTS' in spec['environment']:
        cap = min(cap, max(1, int(spec['environment']['ILEX_VM_SLOTS'])))
    until = time.monotonic() + 2 * 60 * 60
    while time.monotonic() < until:
        with lock(JOBS / 'schedule.lock', seconds=60):
            active = []
            for path in sorted(resource_jobs()):
                if time.monotonic() >= until:
                    raise TimeoutError('job capacity wait exceeded two hours')
                reservation = resource_reservation(path)
                if reservation is None:
                    continue
                if not reconcile_resources(path, min(until, time.monotonic() + CLEANUP_SECONDS)):
                    active.append(reservation)
            if len(active) < cap and sum(x['bytes'] for x in active) + weight <= total - 2 * 1024**3 and sum(x['cpus'] for x in active) + cpus <= os.cpu_count():
                atomic(job_path(spec['snapshot']['identity']['owner'], spec['job']) / 'lease.json',
                       encode({'bytes': weight, 'cpus': cpus, 'unit': service(spec['job'])}))
                return
        time.sleep(2)
    raise TimeoutError('job capacity wait exceeded two hours')


def artifact_entry(file, name):
    with file.open('rb') as source:
        checksum = hashlib.file_digest(source, 'sha256').hexdigest()
    return {'path': name, 'sha256': 'sha256:' + checksum, 'bytes': file.stat().st_size}


def output_prefixes(operation):
    outputs = {'build': ('jobs',), 'rechunk': ('jobs',), 'disk': ('disks',),
               'iso': ('iso',), 'tool': ('tools/images', 'tools/keys'),
               'export': ('exports',)}
    return outputs.get(operation, ())


def artifact_files(root, prefix):
    if not root.is_dir():
        return
    # Enforce the same filters when a replacement publisher opens frozen files.
    assert root.resolve() == root, 'artifact root escaped its owned directory'
    files = root.iterdir() if prefix == 'job' else root.rglob('*')
    for file in sorted(files):
        tail = file.relative_to(root)
        if not file.is_file() or file.is_symlink() or private(tail):
            continue
        parts = [part.lower() for part in tail.parts]
        if any(part.startswith('.ilex') or part.endswith(('.ilex-owner', '.partial', '.lock')) for part in parts):
            continue
        if prefix == 'job' and file.name not in {'stdout.log', 'stderr.log', 'request.json', 'context.json', 'image.json'}:
            continue
        if prefix == 'iso' and file.suffix not in {'.iso', '.sha256', '.json', '.md'}:
            continue
        if prefix == 'disks' and (len(parts) != 1 or not file.name.endswith(('.qcow2', '.raw', '.disk.json'))):
            continue
        if prefix == 'tools/keys' and (len(parts) != 1 or file.suffix != '.crt'):
            continue
        if prefix == 'tools/images' and (len(parts) != 1 or file.suffix not in {'.raw', '.crt'}):
            continue
        if prefix == 'exports' and (len(parts) != 1 or not file.name.endswith(('.oci.tar', '.sha256'))):
            continue
        if prefix == 'jobs' and (len(parts) != 2 or not re.fullmatch('[0-9a-f]{64}', parts[0]) or file.name not in {'request.json', 'result.json'}):
            continue
        if prefix == 'artifacts/console' and (len(parts) != 1 or file.suffix != '.log'):
            continue
        name = relative(prefix + '/' + tail.as_posix())
        assert file.resolve().is_relative_to(root), 'artifact parent escapes its owned directory'
        yield file, name


def frozen_inventory(path, spec, artifacts):
    receipt = path / 'frozen-manifest.json'
    known = {}
    if receipt.exists():
        value = read_json(receipt)
        assert value['job'] == spec['job'] and value['owner'] == spec['snapshot']['identity']['owner'], 'foreign frozen manifest'
        assert value['source_digest'] == spec['snapshot']['source']['digest'], 'frozen manifest source changed'
        known = {entry['path']: entry for entry in value['artifacts']}
    seen = {entry['path'] for entry in artifacts}
    operation = spec['command']['operation']
    for prefix in (*output_prefixes(operation), 'artifacts'):
        for file, name in artifact_files(path / 'exports' / prefix, prefix):
            if name in seen:
                continue
            if operation == 'oemdrv' and name != 'artifacts/answers.raw':
                continue
            if operation in {'import_kache', 'self_check'}:
                continue
            entry = known.get(name)
            if not entry or entry['bytes'] != file.stat().st_size:
                entry = artifact_entry(file, name)
            else:
                entry = {'path': name, 'bytes': entry['bytes'], 'sha256': 'sha256:' + sha(entry['sha256'])}
            artifacts.append(entry)


def artifact_inventory(path, spec, state, artifacts):
    roots = [(path, 'job')]
    operation = spec['command']['operation']
    if operation == 'oemdrv':
        roots.append((path / 'fixture', 'artifacts'))
    elif operation not in {'import_kache', 'self_check'}:
        roots.append((path / 'artifacts', 'artifacts'))
    if state is not None:
        roots.extend((state / prefix, prefix) for prefix in output_prefixes(operation))
    seen = {entry['path'] for entry in artifacts}
    for root, prefix in roots:
        for file, name in artifact_files(root, prefix):
            if name in seen or name.startswith('artifacts/console/'):
                continue  # guest console evidence is collected separately
            # XFS reflinks retain immutable job outputs cheaply even when the
            # next owner-serialized job rewrites a cache at the same state path.
            if prefix != 'job':
                frozen = path / 'exports' / name
                frozen.parent.mkdir(parents=True, exist_ok=True)
                temporary = frozen.with_name(frozen.name + '.partial')
                checked(['cp', '--reflink=auto', '--sparse=always', '--', str(file), str(temporary)])
                os.replace(temporary, frozen)
                file = frozen
            artifacts.append(artifact_entry(file, name))


def remove_libvirt(identity, name):
    ident(identity)
    inventory = checked(['virsh', 'list', '--all', '--uuid']).decode().split()
    if identity not in inventory:
        return
    assert checked(['virsh', 'domname', identity]).decode().strip() == name, 'domain allocation changed'
    subprocess.run(['virsh', 'destroy', identity], env=ENV, capture_output=True)
    assert checked(['virsh', 'domuuid', name]).decode().strip() == identity, 'domain UUID changed'
    checked(['bcvk', 'libvirt', 'rm', '--force', name])


def prepare_console_logs(owner, job):
    # virtlogd/QEMU must not traverse private source, state or credential trees.
    base = CONSOLE_ROOT
    directory = base / owner / job
    group = grp.getgrnam('qemu').gr_gid
    for path in (base, base / owner, directory):
        path.mkdir(exist_ok=True)
        assert path.is_dir() and not path.is_symlink(), 'console directory replaced'
        os.chown(path, 0, group)
        path.chmod(0o770 if path == directory else 0o750)
        checked(['restorecon', '-F', str(path)])
    return directory


def console_names(path, directory):
    names = {source.name for source in directory.glob('*.log')}
    names.update(source.name for source in (path / 'artifacts/console').glob('*.log'))
    for receipt in (path / 'resources').glob('*.json'):
        entry = read_json(receipt)
        if entry['provider'] in {'libvirt', 'libvirt_nonce'}:
            name = relative(entry['name'])
            assert '/' not in name, 'invalid console allocation name'
            names.update({name + '-console.log', name + '-platform.log'})
    return sorted(names)


def collect_console_log(path, directory, name):
    source = directory / name
    if not source.exists():
        source = path / 'artifacts/console' / name  # a legacy job may have made the copy
    assert source.is_file() and not source.is_symlink(), f'missing or invalid console log {name}'
    assert source.resolve().parent in {directory, path / 'artifacts/console'}, 'console log escaped its job'
    artifact = f'artifacts/console/{name}'
    target = path / 'exports' / artifact
    target.parent.mkdir(parents=True, exist_ok=True)
    partial = target.with_name(target.name + '.' + uuid.uuid4().hex + '.partial')
    checked(['cp', '--reflink=auto', '--sparse=always', '--', str(source), str(partial)])
    os.replace(partial, target)
    return artifact_entry(target, artifact)


def cleanup_resources(path, spec):
    for record in sorted((path / 'resources').glob('*.json')):
        resource = read_json(record)
        assert resource['schema'] == 1 and resource['owner'] == spec['snapshot']['identity']['owner']
        assert resource['source_digest'] == spec['snapshot']['source']['digest']
        identity, name = resource['id'], resource['name']
        if resource['provider'] == 'podman':
            assert re.fullmatch('[0-9a-f]{64}', identity)
            inventory = checked(['podman', 'ps', '--all', '--no-trunc', '--format', '{{.ID}}']).decode().splitlines()
            if identity not in inventory:
                continue
            container = json.loads(checked(['podman', 'container', 'inspect', identity]))[0]
            assert container['Id'] == identity and container['Name'].lstrip('/') == name, 'container allocation changed'
            checked(['podman', 'rm', '--force', '--time', '5', identity])
        elif resource['provider'] == 'podman_nonce':
            assert re.fullmatch('[0-9a-f]{64}', identity), 'invalid container allocation nonce'
            inventory = checked(['podman', 'ps', '--all', '--no-trunc', '--filter',
                                 'label=ilex.remote-allocation=' + identity, '--format', '{{.ID}}']).decode().splitlines()
            assert len(inventory) <= 1, 'ambiguous container allocation'
            for cid in inventory:
                assert re.fullmatch('[0-9a-f]{64}', cid), 'invalid container ID'
                container = json.loads(checked(['podman', 'container', 'inspect', cid]))[0]
                labels = container['Config']['Labels']
                assert container['Id'] == cid and container['Name'].lstrip('/') == name, 'container allocation changed'
                assert labels['ilex.remote-allocation'] == identity and labels['ilex.remote-job'] == spec['job'] and labels['ilex.checkout-owner'] == spec['snapshot']['identity']['owner'], 'foreign partial container allocation'
                checked(['podman', 'rm', '--force', '--time', '5', cid])
        elif resource['provider'] == 'libvirt':
            remove_libvirt(identity, name)
        elif resource['provider'] == 'libvirt_nonce':
            assert re.fullmatch('ilex-job-[0-9a-f]{64}', identity), 'invalid libvirt allocation nonce'
            inventory = checked(['virsh', 'list', '--all', '--name']).decode().splitlines()
            if name not in inventory:
                continue
            domain = ET.fromstring(checked(['virsh', 'dumpxml', name]))
            labels = domain.findtext('.//{https://github.com/containers/bootc}label', '').split(',')
            assert identity in labels, 'foreign partial libvirt allocation'
            remove_libvirt(domain.findtext('uuid'), name)
        elif resource['provider'] == 'image_alias':
            assert re.fullmatch('localhost/ilex-job:[0-9a-f]{64}', identity), 'invalid temporary image allocation'
            inventory = checked(['podman', 'images', '--all', '--format', '{{.Repository}}:{{.Tag}}']).decode().splitlines()
            if identity not in inventory:
                continue
            image = json.loads(checked(['podman', 'image', 'inspect', identity]))[0]
            assert image['Labels']['ilex.checkout-owner'] == spec['snapshot']['identity']['owner'], 'temporary image owner changed'
            checked(['podman', 'untag', identity, identity])
        else:
            raise ValueError('unknown resource provider')
    for cid in sorted((path / 'resources').glob('*.cid')):
        identity = cid.read_text().strip()
        assert re.fullmatch('[0-9a-f]{64}', identity), 'invalid Podman-created CID'
        inventory = checked(['podman', 'ps', '--all', '--no-trunc', '--format', '{{.ID}}']).decode().splitlines()
        if identity not in inventory:
            continue
        container = json.loads(checked(['podman', 'container', 'inspect', identity]))[0]
        labels = container['Config']['Labels']
        assert container['Id'] == identity and labels['ilex.remote-job'] == spec['job'] and labels['ilex.checkout-owner'] == spec['snapshot']['identity']['owner'], 'foreign job container'
        checked(['podman', 'rm', '--force', '--time', '5', identity])


def kache_version(root):
    return next(line.split('=', 1)[1] for line in (root / 'Containerfile').read_text().splitlines()
                if line.startswith('ARG KACHE_VERSION='))


def import_kache(root, spec):
    args = spec['command']['args']
    assert args['version'] == kache_version(root), 'cache version must match Containerfile'
    key = sha(args['sha256'])
    chunks = spec['inputs']['kache']
    assert set(spec['inputs']) == {'kache'} and chunks, 'missing kache archive'
    owner = owner_dir(spec['snapshot'])
    parent = owner / KACHE_IMPORT_LAYOUT
    parent.mkdir(parents=True, exist_ok=True)
    destination = parent / key
    receipt = {'schema': 1, 'sha256': args['sha256'], 'version': args['version'],
               'target': 'x86_64-linux', 'owner': spec['snapshot']['identity']['owner']}
    if destination.exists():
        assert read_json(destination / 'receipt.json') == receipt, 'cache import identity mismatch'
        assert (destination / 'store/index.db').is_file(), 'imported cache index missing'
        return receipt
    reserve(2 * sum(chunk_path(chunk).stat().st_size for chunk in chunks))
    with tempfile.TemporaryDirectory(prefix='.import-', dir=parent) as tmp:
        archive = Path(tmp) / 'cache.tar'
        candidate = Path(tmp) / 'candidate'
        store = candidate / 'store'
        store.mkdir(parents=True)
        join_chunks(chunks, archive)
        with archive.open('rb') as stream:
            assert hashlib.file_digest(stream, 'sha256').hexdigest() == key, 'cache archive changed in transfer'
        with tarfile.open(archive, 'r:') as entries:
            for member in entries:
                name = member.name.removeprefix('./')
                if member.isdir() and name in {'', '.'}:
                    continue
                name = relative(name)
                assert not private(name), 'private data is not a cache input'
                output = store / name
                if member.isdir():
                    output.mkdir(parents=True, exist_ok=True)
                else:
                    assert member.isfile() and not member.issparse(), 'cache archive must contain regular files and directories'
                    output.parent.mkdir(parents=True, exist_ok=True)
                    with entries.extractfile(member) as src, output.open('xb') as out:
                        shutil.copyfileobj(src, out)
                        out.flush()
                        os.fsync(out.fileno())
        index = store / 'index.db'
        assert index.is_file() and index.stat().st_size, 'archive is missing the cache index'
        # A cleanly stopped daemon can leave a WAL-mode index without its
        # sidecars. SQLite cannot create those files on the read-only mount.
        with contextlib.closing(sqlite3.connect(index)) as database:
            busy, _, _ = database.execute('PRAGMA wal_checkpoint(TRUNCATE)').fetchone()
            assert busy == 0, 'could not checkpoint the copied cache index'
            mode, = database.execute('PRAGMA journal_mode=DELETE').fetchone()
            assert mode == 'delete', 'could not prepare the copied index for a read-only mount'
            assert database.execute('PRAGMA quick_check(1)').fetchall() == [('ok',)], 'cache index is corrupt'
            database.execute('SELECT 1 FROM entries LIMIT 1').fetchone()
        atomic(candidate / 'receipt.json', encode(receipt))
        candidate.rename(destination)
        fd = os.open(parent, os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)
    return receipt


def create_oemdrv(path, spec):
    args = spec['command']['args']
    assert set(args) == {'kickstart', 'public_key'} and not spec['inputs'], 'unexpected OEMDRV input'
    output = path / 'fixture'
    output.mkdir()
    raw = output / 'answers.raw.partial'
    with tempfile.TemporaryDirectory(dir=path) as source:
        for arg, name in [('kickstart', 'ks.cfg'), ('public_key', 'recovery.pub')]:
            data = args[arg].encode()
            assert 0 < len(data) <= 65536, 'invalid OEMDRV source size'
            (Path(source) / name).write_bytes(data)
        with raw.open('xb') as file:
            file.truncate(OEMDRV_BYTES)
        checked(['mke2fs', '-q', '-t', 'ext4', '-F', '-L', 'OEMDRV', '-d', source, str(raw)], timeout=30)
    raw.rename(output / 'answers.raw')


def read_oemdrv(frame, payload):
    assert len(payload) == OEMDRV_BYTES and digest(payload) == frame['sha256'], 'invalid OEMDRV readback image'

    # The guest-written disk and encrypted envelope stay out of persistent job
    # files, chunk caches and logs. The e2e harness decrypts only on Windows.
    with os.fdopen(os.memfd_create('ilex-oemdrv'), 'wb') as file:
        file.write(payload)
        file.flush()
        fd = file.fileno()

        def extract(entry, size):
            with subprocess.Popen(
                ['timeout', '10s', 'debugfs', '-R', 'cat ' + entry, f'/proc/self/fd/{fd}'],
                pass_fds=(fd,), stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=ENV,
            ) as cmd:
                data = cmd.stdout.read(size + 1)
                if len(data) > size:
                    cmd.kill()
                assert cmd.wait() == 0 and len(data) == size, 'invalid OEMDRV witness'
                return data

        boot = extract('/public/installer-boot', 37).decode().strip()
        fingerprint = extract('/public/recovery-sha256', 65).decode().strip()
        envelope = extract('/recovery.oaep', 256)
    return {'boot': ident(boot), 'fingerprint': sha('sha256:' + fingerprint),
            'sha256': digest(envelope)}, envelope


def execute(owner, job):
    path, spec = load_job({'owner': owner, 'job': job})
    state = owner_dir(spec['snapshot']) / 'state' / spec['snapshot']['identity']['slug']
    started = time.monotonic()
    free_before = shutil.disk_usage(BASE).free
    compile_seconds = None
    operation_seconds = None
    cancelled = False

    def cancel(_signum, _frame):
        nonlocal cancelled
        cancelled = True
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        raise InterruptedError('job cancelled')

    signal.signal(signal.SIGTERM, cancel)
    code = 1
    guards = contextlib.ExitStack()
    claimed = False
    admitted = False
    try:
        with dispatch(path):
            current = record(path, spec)
            if current['state'] in TERMINAL_STATES:
                return
            assert current['state'] == 'starting', 'job is not starting'
            if expired(current):
                return
            compare_and_swap(path, spec, current, 'worker', 'running', lease=None)
            claimed = True
        guards.enter_context(lock('/run/lock/ilex-builder-jobs.lock', shared=True))
        guards.enter_context(lock(state.parent.parent / 'job.lock', seconds=2 * 60 * 60))
        capacity(spec)
        admitted = True
        if admitted:
            root = snapshot_path(spec['snapshot'])
            verify_source(root, spec['snapshot']['source'])
            console_logs = prepare_console_logs(owner, job)
            env = ENV | spec['environment'] | {
                'ILEX_ROOT': str(root), 'ILEX_STATE_DIR': str(state.parent),
                'ILEX_REMOTE_CONTEXT': str(path / 'context.json'),
                'ILEX_ARTIFACTS': str(path / 'artifacts'),
                'ILEX_VM_LOG_DIR': str(console_logs),
                'ILEX_JOB_RESOURCE_DIR': str(path / 'resources'),
                # Cargo freshness uses mtimes and shared binary paths. Returning
                # to an older immutable tree must not execute a newer worker.
                'CARGO_TARGET_DIR': str(BASE / 'cargo/targets' / owner / ('x86_64-linux-' + hashlib.sha256(checked(['rustc', '-vV'])).hexdigest()[:16]) / root.name),
                'CARGO_BUILD_JOBS': str(min(4, os.cpu_count())), 'ILEX_VM_SLOTS': '1',
            }
            operation = spec['command']['operation']
            for name, chunks in ({} if operation == 'import_kache' else spec['inputs']).items():
                assert name in {'iso', 'catalogue'}, 'unsupported input artifact'
                target = path / 'inputs' / ('ilex-installer.iso' if name == 'iso' else 'compositions.json')
                target.parent.mkdir(exist_ok=True)
                join_chunks(chunks, target)
                if name == 'iso':
                    env['ILEX_ISO'] = str(target)
            Path(env['XDG_RUNTIME_DIR']).mkdir(mode=0o700, exist_ok=True)
            if operation == 'oemdrv':
                operation_started = time.monotonic()
                create_oemdrv(path, spec)
                operation_seconds = time.monotonic() - operation_started
                code = 0
                return
            if operation == 'import_kache':
                operation_started = time.monotonic()
                atomic(path / 'stdout.log', encode(import_kache(root, spec)) + b'\n')
                operation_seconds = time.monotonic() - operation_started
                code = 0
                return
            if operation == 'build' and 'ILEX_KACHE_IMPORT' in env:
                cache = owner_dir(spec['snapshot']) / KACHE_IMPORT_LAYOUT / sha(env['ILEX_KACHE_IMPORT'])
                receipt = read_json(cache / 'receipt.json')
                assert receipt['sha256'] == env['ILEX_KACHE_IMPORT'] and receipt['owner'] == owner, 'cache import identity mismatch'
                assert receipt['version'] == kache_version(root) and receipt['target'] == 'x86_64-linux', 'cache import profile mismatch'
                assert (cache / 'store/index.db').is_file(), 'imported cache index missing'
                env['KACHE_CACHE_DIR'] = str(cache / 'store')
            with (path / 'stdout.log').open('ab', buffering=0) as out, (path / 'stderr.log').open('ab', buffering=0) as err:
                attached = interactive(spec)
                compile_started = time.monotonic()
                compile_result = subprocess.run(['cargo', 'build', '--locked', '-p', 'xtask'], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=None if attached else err, stderr=None if attached else err)
                compile_seconds = time.monotonic() - compile_started
                if compile_result.returncode:
                    code = compile_result.returncode
                else:
                    binary = Path(env['CARGO_TARGET_DIR']) / 'debug/xtask'
                    operation_started = time.monotonic()
                    code = subprocess.run([str(binary), 'remote-worker', '--request', str(path / 'request.json')],
                                          cwd=root, env=env | ({'TERM': os.environ.get('TERM', 'xterm')} if attached else {}),
                                          stdin=None if attached else subprocess.DEVNULL,
                                          stdout=None if attached else out, stderr=None if attached else err).returncode
                    operation_seconds = time.monotonic() - operation_started
    except BaseException as error:
        if not claimed:
            raise
        with (path / 'stderr.log').open('ab') as err:
            err.write((str(error) + '\n').encode())
        code = 130 if cancelled else 1
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        try:
            if claimed:
                finish_worker(path, spec, state if admitted else None, code, cancelled,
                              {'compile_seconds': compile_seconds, 'operation_seconds': operation_seconds,
                               'data_free_before': free_before}, started)
        finally:
            guards.close()


@contextlib.contextmanager
def deadline(until, label):
    seconds = until - time.monotonic()
    if seconds <= 0:
        raise TimeoutError(f'{label} deadline exceeded')

    def timeout(_signum, _frame):
        raise TimeoutError(f'{label} deadline exceeded')

    previous = signal.signal(signal.SIGALRM, timeout)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, previous)


def cleanup_bounded(spec, until):
    # Keep the publisher out of virsh/Podman. Even a stuck child must not own
    # the job record or its deadline.
    owner = spec['snapshot']['identity']['owner']
    with tempfile.TemporaryFile() as output:
        worker = subprocess.Popen([sys.executable, '-I', '-S', __file__, '--cleanup', owner, spec['job'], str(until)],
                                  env=ENV, stdin=subprocess.DEVNULL, stdout=output,
                                  stderr=output, start_new_session=True)
        try:
            if worker.wait(timeout=max(0, until - time.monotonic())) == 0:
                return
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(worker.pid, signal.SIGKILL)
            with contextlib.suppress(subprocess.TimeoutExpired):
                worker.wait(timeout=1)
            raise TimeoutError('resource cleanup deadline exceeded') from None
        if worker.returncode == -signal.SIGKILL:
            raise TimeoutError('resource cleanup deadline exceeded')
        output.seek(0)
        raise RuntimeError(output.read(4096).decode(errors='replace').strip() or 'resource cleanup failed')


def resource_jobs():
    return {file.parent for name in ('lease.json', 'reap.json') for file in JOBS.glob(f'*/*/{name}')}


def resource_reservation(path):
    for name in ('lease.json', 'reap.json'):
        try:
            value = read_json(path / name)
            return value if name == 'lease.json' else value['reservation']
        except FileNotFoundError:
            continue
    return None


def resource_names(path):
    names = set()
    for file in (path / 'resources').glob('*.json'):
        entry = read_json(file)
        names.add(entry['name'])
    for file in (path / 'resources').glob('*.cid'):
        names.add(file.read_text().strip())
    return sorted(names)


def release_resources(path):
    (path / 'lease.json').unlink(missing_ok=True)
    (path / 'reap.json').unlink(missing_ok=True)


def defer_cleanup(path):
    # Commit the reaping receipt before releasing the active lease. A crash
    # between these operations leaves a retryable receipt and a stale lease.
    names = resource_names(path)
    atomic(path / 'reap.json', encode({'names': names, 'reservation': resource_reservation(path)}))
    (path / 'lease.json').unlink(missing_ok=True)
    return names


def reconcile_resources(path, until):
    try:
        with lock(path / 'publisher.lock', wait=False):
            _, spec = load_job({'owner': path.parent.name, 'job': path.name})
            if unit_live(spec, owned=True):
                return False
            try:
                cleanup_bounded(spec, until)
            except (OSError, RuntimeError, subprocess.SubprocessError):
                defer_cleanup(path)
                return False
            release_resources(path)
            return True
    except BlockingIOError:
        return False


def maintenance():
    # The shell holds the exclusive lifecycle lock. A live lease cannot be
    # renewed or consumed between this inventory and the idle decision.
    until = time.monotonic() + MAINTENANCE_SECONDS
    with deadline(until, 'maintenance job inventory'):
        for file in sorted(JOBS.glob('*/*/state.json')):
            path, spec = load_job({'owner': file.parent.parent.name, 'job': file.parent.name})
            current = record(path, spec)
            if current and current['state'] in {'registered', 'starting'} and not expired(current):
                return f"busy: job {spec['job']}"
        resources = sorted(resource_jobs())
    for path in resources:
        if time.monotonic() >= until:
            break
        if not reconcile_resources(path, min(until, time.monotonic() + CLEANUP_SECONDS)):
            print(f"job {path.name} cleanup still pending: {', '.join(resource_names(path))}", file=sys.stderr)
    return ''


def collect_final(path, spec, state, until, cleanup):
    artifacts, errors = [], []
    cleanup_error = None
    pending_resources = []
    if cleanup:
        try:
            cleanup_bounded(spec, min(until, time.monotonic() + CLEANUP_SECONDS))
            release_resources(path)
        except Exception as failure:
            cleanup_error = str(failure)
            pending_resources = defer_cleanup(path)
    owner = spec['snapshot']['identity']['owner']
    directory = CONSOLE_ROOT / owner / spec['job']
    try:
        with deadline(until, 'frozen evidence recovery'):
            frozen_inventory(path, spec, artifacts)
    except Exception as failure:
        errors.append(str(failure))
    frozen_names = {entry['path'] for entry in artifacts}
    try:
        with deadline(until, 'console inventory'):
            names = console_names(path, directory)
    except Exception as failure:
        names = []
        errors.append(f'console inventory: {failure}')
    for name in names:
        if f'artifacts/console/{name}' in frozen_names:
            continue
        try:
            with deadline(until, 'console collection'):
                artifacts.append(collect_console_log(path, directory, name))
        except Exception as failure:
            errors.append(f'{name}: {failure}')
    try:
        with deadline(until, 'evidence collection'):
            artifact_inventory(path, spec, state, artifacts)
            atomic(path / 'frozen-manifest.json', encode({
                'job': spec['job'], 'owner': owner,
                'source_digest': spec['snapshot']['source']['digest'],
                'artifacts': [entry for entry in artifacts if not entry['path'].startswith('job/')],
            }))
    except Exception as failure:
        errors.append(str(failure))
    return {'artifacts': artifacts, 'evidence_error': '; '.join(errors) or None,
            'resource_cleanup_error': cleanup_error,
            'resources_pending_cleanup': pending_resources,
            'resource_lease_retained': (path / 'lease.json').exists()}


def state_view(current):
    return {'state': current['state'], 'exit_code': current.get('exit_code')}


def publish(path, spec, current, actor, outcome, code, **evidence):
    assert outcome in TERMINAL_STATES and 'result' not in (current or {}), 'job result already published'
    state_bytes = encode(state_view({'state': outcome, 'exit_code': code}))
    artifacts = evidence.pop('artifacts')
    artifacts.append({'path': 'job/state.json', 'bytes': len(state_bytes), 'sha256': digest(state_bytes)})
    result = {'schema': 1, 'job': spec['job'], 'owner': spec['snapshot']['identity']['owner'],
              'source_digest': spec['snapshot']['source']['digest'], 'exit_code': code,
              'artifacts': artifacts, **evidence}
    return compare_and_swap(path, spec, current, actor, outcome, exit_code=code, result=result)


def finish_unclaimed(path, spec, current, outcome, reason=None):
    artifacts = [artifact_entry(path / name, 'job/' + name)
                 for name in ('request.json', 'context.json') if (path / name).is_file()]
    return publish(path, spec, current, 'controller', outcome, 130, artifacts=artifacts,
                   interrupted=outcome == 'interrupted', terminal_error=reason)


def finish_worker(path, spec, state, code, cancelled, metrics, started):
    with dispatch(path):
        current = record(path, spec)
        compare_and_swap(path, spec, current, 'worker', 'collecting')
    until = time.monotonic() + EVIDENCE_SECONDS
    evidence = collect_final(path, spec, state, until, cleanup=(path / 'lease.json').exists())
    versions = {}
    for tool in ('rustc', 'cargo', 'podman', 'bcvk', 'qemu-system-x86_64'):
        try:
            versions[tool] = checked([tool, '--version'], timeout=min(5, max(0, until - time.monotonic()))).decode().splitlines()[0]
        except Exception:
            versions[tool] = 'unavailable'
    details = {**metrics, 'elapsed_seconds': time.monotonic() - started, 'data_free_after': shutil.disk_usage(BASE).free,
               'executor_id': 'hyperv-builder:' + read_json('/var/lib/ilex-builder/provision.json')['data_uuid'],
               'versions': versions}
    if (path / 'image.json').is_file():
        details['image'] = read_json(path / 'image.json')
    with dispatch(path):
        current = record(path, spec)
        cancelled = cancelled or 'cancel_at' in current
        code = 130 if cancelled else (code or int(bool(evidence['resource_cleanup_error'] or evidence['evidence_error'])))
        publish(path, spec, current, 'worker', 'cancelled' if cancelled else 'complete',
                code, **details, **evidence)


def recover(path, spec):
    # A crashed recovery releases this lock in the kernel. Its successor
    # reclaims the recovering record without racing a live publisher.
    try:
        with lock(path / 'publisher.lock', wait=False):
            with dispatch(path):
                current = record(path, spec)
                if current and current['state'] not in {'running', 'collecting', 'recovering'}:
                    return current
                if current and unit_live(spec, owned=True):
                    return current
                if current is None:
                    # The submitter died after writing the request but before committing registration.
                    # The absence is fenced by dispatch.lock and no launcher can claim it.
                    current = compare_and_swap(path, spec, None, 'recovery', 'recovering')
                else:
                    current = compare_and_swap(path, spec, current, 'recovery', 'recovering')
            # Only job-owned outputs are safe to recover after losing the checkout execution lock.
            evidence = collect_final(path, spec, None, time.monotonic() + RECOVERY_SECONDS, cleanup=True)
            with dispatch(path):
                return publish(path, spec, current, 'recovery', 'interrupted', 130,
                               interrupted=True, **evidence)
    except BlockingIOError:
        with dispatch(path):
            return record(path, spec)


def cancel_step(path, spec):
    with dispatch(path):
        current = record(path, spec)
        if current is None:
            finish_unclaimed(path, spec, current, 'cancelled')
            return 'confirmed', None
        phase = current['state']
        if phase in TERMINAL_STATES:
            return ('finished' if phase == 'complete' else 'confirmed'), None
        if phase in {'registered', 'starting'}:
            finish_unclaimed(path, spec, current, 'cancelled')
            return 'confirmed', None
        if phase == 'recovering':
            return 'gone', None
        if 'cancel_at' not in current:
            current = compare_and_swap(path, spec, current, 'controller', phase, cancel_at=time.monotonic())
        if phase == 'collecting':
            return 'confirmed', None
        try:
            if not unit_live(spec, owned=True):
                return 'gone', None
            now = time.monotonic()
            which = 'SIGKILL' if now - current['cancel_at'] >= CANCEL_GRACE_SECONDS else 'SIGTERM'
            retry_kill = which == 'SIGKILL' and now - current.get('delivered_at', 0) >= KILL_RETRY_SECONDS
            if current.get('delivered') != which or retry_kill:
                checked(['systemctl', 'kill', '--signal=' + which, '--kill-whom=all', service(spec['job'])], timeout=2)
                compare_and_swap(path, spec, current, 'controller', phase,
                                 delivered=which, delivered_at=time.monotonic())
        except (OSError, subprocess.SubprocessError, AssertionError) as failure:
            return 'pending', str(failure)
        return 'pending', 'waiting for the worker to stop'


def cancel_job(path, spec):
    until = time.monotonic() + CANCEL_WAIT_SECONDS
    while True:
        try:
            status, detail = cancel_step(path, spec)
        except BlockingIOError:
            status, detail = 'pending', 'job transition in progress'
        if status != 'pending':
            # A status poll will recover evidence if a unit has exited.
            return {'cancel': 'confirmed' if status == 'gone' else status}
        if time.monotonic() >= until:
            return {'cancel': 'pending', 'detail': detail}
        time.sleep(.5)


def job_status(path, spec, resume_terminal=False):
    with dispatch(path):
        current = record(path, spec)
        if current and current['state'] in {'registered', 'starting'}:
            if expired(current) or (resume_terminal and current['state'] == 'registered'):
                reason = ('the original client did not open its terminal' if resume_terminal and current['state'] == 'registered'
                          else 'terminal registration expired' if current['state'] == 'registered'
                          else 'worker start expired')
                current = finish_unclaimed(path, spec, current, 'interrupted', reason)
    if current and current['state'] == 'running' and 'cancel_at' in current:
        cancel_step(path, spec)
    if current is None or current['state'] in {'running', 'collecting', 'recovering'}:
        # A live worker keeps its publication right. Recovery never runs under the dispatch lock.
        current = recover(path, spec)
    sizes = {n: (path / (n + '.log')).stat().st_size if (path / (n + '.log')).exists() else 0 for n in ('stdout', 'stderr')}
    return {'state': state_view(current or {'state': 'recovering'}),
            'result': (current or {}).get('result'), 'logs': sizes}


def request(frame, payload):
    assert frame['schema'] == 1, 'unsupported job protocol version'
    op = frame['operation']
    if op == 'read_oemdrv':
        return read_oemdrv(frame, payload)
    if op == 'missing':
        chunks = frame['chunks']
        assert len(chunks) <= 100000
        return {'missing': [c for c in chunks if not chunk_path(c).exists()]}, b''
    if op == 'put':
        assert len(payload) <= CHUNK and digest(payload) == frame['sha256'], 'chunk checksum mismatch'
        reserve(len(payload))
        path = chunk_path(frame['sha256'])
        if not path.exists():
            atomic(path, payload)
        return {}, b''
    if op == 'put_many':
        offset = 0
        for chunk in frame['chunks']:
            count = chunk['bytes']
            assert isinstance(count, int) and 0 < count <= CHUNK
            data = payload[offset:offset + count]
            assert len(data) == count and digest(data) == chunk['sha256'], 'batch chunk checksum mismatch'
            path = chunk_path(chunk['sha256'])
            reserve(count)
            if not path.exists():
                atomic(path, data)
            offset += count
        assert offset == len(payload), 'trailing chunk data'
        return {}, b''
    if op == 'submit':
        return submit(frame['request']), b''
    if op == 'stage':
        return {'root': str(materialize(frame['snapshot']))}, b''
    if op == 'renew_terminal':
        return renew(frame['owner'], frame['job']), b''
    if op in {'status', 'resume_terminal', 'cancel', 'fetch'}:
        path, spec = load_job(frame)
        if op == 'cancel':
            return cancel_job(path, spec), b''
        if op in {'status', 'resume_terminal'}:
            assert op == 'status' or interactive(spec), 'job has no guest terminal'
            return job_status(path, spec, resume_terminal=op == 'resume_terminal'), b''
        name = relative(frame['path'])
        prefix, _, tail = name.partition('/')
        if prefix == 'job':
            assert tail in {'stdout.log', 'stderr.log', 'state.json', 'request.json', 'context.json', 'result.json', 'image.json'}, 'unsupported job artifact'
            target = path / tail
            if tail in {'state.json', 'result.json'}:
                current = record(path, spec)
                if current and 'generation' in current:
                    value = state_view(current) if tail == 'state.json' else current.get('result')
                    assert value is not None, 'job result not published'
                    offset = frame.get('offset', 0)
                    assert isinstance(offset, int) and offset >= 0
                    data = encode(value)[offset:offset + 64 * 1024 * 1024]
                    return {'bytes': len(data), 'sha256': digest(data)}, data
        else:
            assert prefix in {'artifacts', 'iso', 'disks', 'tools', 'jobs', 'exports'} and not private(tail), 'unsupported output artifact'
            result = record(path, spec).get('result')
            assert result is not None, 'job result not published'
            assert result['job'] == frame['job'] and result['owner'] == frame['owner'], 'artifact result identity mismatch'
            assert any(a['path'] == name for a in result['artifacts']), 'artifact is not in the immutable job result'
            target = path / 'exports' / name
        assert target.resolve().is_relative_to(path.resolve()) and not target.is_symlink(), 'artifact escape'
        offset = frame.get('offset', 0)
        assert isinstance(offset, int) and offset >= 0
        with target.open('rb') as f:
            f.seek(offset)
            data = f.read(64 * 1024 * 1024)
        return {'bytes': len(data), 'sha256': digest(data)}, data
    raise ValueError('unsupported job operation')


def main():
    os.umask(0o077)
    if sys.argv[1:] == ['--maintenance']:
        print(maintenance())
        return
    if len(sys.argv) == 4 and sys.argv[1] == '--terminal':
        return terminal(ident(sys.argv[2]), ident(sys.argv[3]))
    if len(sys.argv) == 4 and sys.argv[1] == '--run':
        execute(ident(sys.argv[2]), ident(sys.argv[3]))
        return
    if len(sys.argv) == 5 and sys.argv[1] == '--cleanup':
        assert os.getpid() == os.getpgrp(), 'cleanup needs its own process group'
        until = float(sys.argv[4])
        # The deadline and resource lock still apply if the publisher dies.
        def stop_cleanup(_signum, _frame):
            os.killpg(os.getpgrp(), signal.SIGKILL)
        signal.signal(signal.SIGALRM, stop_cleanup)
        signal.setitimer(signal.ITIMER_REAL, max(.001, until - time.monotonic()))
        path, spec = load_job({'owner': ident(sys.argv[2]), 'job': ident(sys.argv[3])})
        with lock(path / 'resource-cleanup.lock', seconds=max(0, until - time.monotonic())):
            cleanup_resources(path, spec)
        return
    assert len(sys.argv) == 1, 'arguments are internal only'
    # read(1) in the shell controller does not prefetch this binary stdin frame.
    line = sys.stdin.buffer.readline(MAX_FRAME + 1)
    assert len(line) <= MAX_FRAME and line.endswith(b'\n'), 'oversized/truncated request'
    frame = json.loads(line)
    assert frame['schema'] == 1, 'unsupported job protocol version'
    count = frame.get('payload_bytes', 0)
    limit = OEMDRV_BYTES if frame['operation'] == 'read_oemdrv' else CHUNK
    assert isinstance(count, int) and 0 <= count <= limit
    payload = sys.stdin.buffer.read(count)
    assert len(payload) == count, 'truncated chunk'
    with lock('/run/lock/ilex-builder-jobs.lock', shared=True):
        value, data = request(frame, payload)
    sys.stdout.buffer.write(encode({'schema': 1, 'ok': True, 'value': value, 'payload_bytes': len(data)}) + b'\n' + data)


if __name__ == '__main__':
    try:
        status = main()
    except BaseException as error:
        if len(sys.argv) == 1:
            sys.stdout.buffer.write(encode({'schema': 1, 'ok': False, 'error': str(error), 'payload_bytes': 0}) + b'\n')
        else:
            traceback.print_exc(file=sys.stderr)
        sys.exit(1)
    sys.exit(status)
