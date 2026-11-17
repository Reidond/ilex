#!/usr/bin/python3
"""Finite SSH job protocol. No shell, user paths, environment or credentials.

The maintenance key already has guest-root authority through recipe updates.
The framing and path checks prevent accidental interpolation and cross-job reuse;
they do not sandbox executable source supplied by that key holder.
"""
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import signal
import stat
import subprocess
import sys
import time
import traceback
import uuid

BASE = Path('/var/lib/ilex-data')
CHUNKS = BASE / 'chunks'
JOBS = BASE / 'jobs'
MAX_FRAME = 32 * 1024 * 1024
CHUNK = 4 * 1024 * 1024
ENV = {'PATH': '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin',
       'HOME': '/root', 'LANG': 'C.UTF-8', 'XDG_RUNTIME_DIR': '/run/ilex-builder',
       'CARGO_HOME': str(BASE / 'cargo/home'), 'KACHE_CACHE_DIR': str(BASE / 'kache')}
ALLOWED_ENV = {'ILEX_IMAGE', 'ILEX_UPGRADE_FROM', 'ILEX_SHELLS', 'ILEX_MACHINE',
               'ILEX_UPGRADE_PREPARE', 'ILEX_SIGPOLICY_LEGACY', 'ILEX_TOOL_REBUILD',
               'ILEX_ISO_MACHINE', 'ILEX_ISO_EXPECT', 'ILEX_ISO_TARGET',
               'ILEX_ISO_DIGEST', 'ILEX_ISO_INSTALL_TIMEOUT', 'ILEX_VM_SLOTS'}


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
    return any(p in {'.git', '.ssh', '.gnupg', '.env', 'ilex-secrets', 'credentials'}
               or p.endswith(('.key', '.pem', '.p12', '.pfx')) or p.startswith('.env.')
               for p in Path(path).parts)


@contextlib.contextmanager
def lock(path, shared=False, wait=True):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open('a+b') as f:
        fcntl.flock(f, (fcntl.LOCK_SH if shared else fcntl.LOCK_EX) | (0 if wait else fcntl.LOCK_NB))
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


def running(unit):
    state = subprocess.run(['systemctl', 'show', '-p', 'ActiveState', '--value', unit],
                           env=ENV, capture_output=True, text=True, check=True).stdout.strip()
    return state in {'active', 'activating', 'deactivating', 'reloading'}


def submit(spec):
    assert spec['schema'] == 1 and spec['command']['operation'] in {'build', 'rechunk', 'disk', 'iso', 'tool', 'test', 'self_check'}, 'unsupported job schema/operation'
    assert set(spec['environment']) <= ALLOWED_ENV, 'environment not allowlisted'
    assert all(isinstance(v, str) and len(v) < 16384 and '\x00' not in v for v in spec['environment'].values()), 'invalid environment value'
    snapshot = spec['snapshot']
    root = materialize(snapshot)
    owner = snapshot['identity']['owner']
    path = job_path(owner, spec['job'])
    with lock(JOBS / 'register.lock'):
        if path.exists():
            assert read_json(path / 'request.json') == spec, 'job UUID reused with another request'
            return {'job': spec['job'], 'root': str(root)}
        reserve(16 * 1024**3)
        path.mkdir(parents=True)
        atomic(path / 'request.json', encode(spec))
        atomic(path / 'context.json', encode({'schema': 1, 'root': str(root), 'identity': snapshot['identity'], 'source': snapshot['source']}))
        atomic(path / 'state.json', encode({'state': 'queued', 'exit_code': None}))
        try:
            checked(['systemd-run', '--quiet', '--unit=' + service(spec['job']),
                     '--property=Type=exec', '--property=KillMode=control-group',
                     '--property=TimeoutStopSec=45', '--property=SendSIGKILL=yes',
                     '/usr/bin/python3', '-I', '-S', '/usr/local/libexec/ilex-builder-jobs',
                     '--run', owner, spec['job']])
        except BaseException:
            atomic(path / 'state.json', encode({'state': 'failed', 'exit_code': 1}))
            raise
    return {'job': spec['job'], 'root': str(root)}


def capacity(spec):
    # One full build/test at a time on 16 GiB; smaller tool jobs may share.
    kind = spec['command']['operation']
    weight = {'tool': 4, 'self_check': 1, 'disk': 10, 'rechunk': 8}.get(kind, 12) * 1024**3
    total = int(next(l.split()[1] for l in Path('/proc/meminfo').read_text().splitlines() if l.startswith('MemTotal:'))) * 1024
    assert weight <= total - 2 * 1024**3, 'builder is too small for this operation; use builder up --memory-gib 16 while off'
    cap = min(4, max(1, total // (8 * 1024**3)))
    if 'ILEX_VM_SLOTS' in spec['environment']:
        cap = min(cap, max(1, int(spec['environment']['ILEX_VM_SLOTS'])))
    while True:
        with lock(JOBS / 'schedule.lock'):
            active = []
            for lease in JOBS.glob('*/*/lease.json'):
                record = read_json(lease)
                if running(service(lease.parent.name)):
                    active.append(record)
            if len(active) < cap and sum(x['bytes'] for x in active) + weight <= total - 2 * 1024**3 and sum(x['cpus'] for x in active) + min(4, os.cpu_count()) <= os.cpu_count():
                atomic(job_path(spec['snapshot']['identity']['owner'], spec['job']) / 'lease.json',
                       encode({'bytes': weight, 'cpus': min(4, os.cpu_count()), 'unit': service(spec['job'])}))
                return
        time.sleep(2)


def artifact_inventory(path, state, include_state=True):
    artifacts = []
    roots = [(path, 'job'), (state / 'artifacts', 'artifacts'), (state / 'iso', 'iso'),
             (state / 'disks', 'disks'), (state / 'tools/images', 'tools/images'),
             (state / 'tools/keys', 'tools/keys'), (state / 'jobs', 'jobs')]
    for root, prefix in roots:
        if not include_state and prefix != 'job':
            continue
        if not root.is_dir():
            continue
        for file in sorted(root.rglob('*')):
            if not file.is_file() or file.is_symlink() or private(file.relative_to(root)) or file.name.endswith(('.partial', '.lock')):
                continue
            if prefix == 'job' and (file.parent != path or file.name not in {'stdout.log', 'stderr.log', 'state.json', 'request.json', 'context.json', 'image.json'}):
                continue
            if prefix == 'iso' and file.suffix not in {'.iso', '.sha256', '.json', '.md'}:
                continue
            if prefix == 'tools/keys' and file.suffix != '.crt':
                continue
            assert file.resolve().is_relative_to(root.resolve()), 'artifact parent escapes its owned directory'
            name = prefix + '/' + file.relative_to(root).as_posix()
            # XFS reflinks retain immutable job outputs cheaply even when the
            # next owner-serialized job rewrites a cache at the same state path.
            if prefix != 'job':
                frozen = path / 'exports' / name
                frozen.parent.mkdir(parents=True, exist_ok=True)
                temporary = frozen.with_name(frozen.name + '.partial')
                checked(['cp', '--reflink=auto', '--sparse=always', '--', str(file), str(temporary)])
                os.replace(temporary, frozen)
                file = frozen
            hasher = hashlib.sha256()
            with file.open('rb') as f:
                while data := f.read(CHUNK):
                    hasher.update(data)
            artifacts.append({'path': name,
                              'sha256': 'sha256:' + hasher.hexdigest(), 'bytes': file.stat().st_size})
    return artifacts


def cleanup_resources(path, spec):
    for record in sorted((path / 'resources').glob('*.json')):
        resource = read_json(record)
        assert resource['schema'] == 1 and resource['owner'] == spec['snapshot']['identity']['owner']
        assert resource['source_digest'] == spec['snapshot']['source']['digest']
        identity, name = resource['id'], resource['name']
        if resource['provider'] == 'podman':
            assert re.fullmatch('[0-9a-f]{64}', identity)
            result = subprocess.run(['podman', 'container', 'inspect', identity], env=ENV, capture_output=True)
            if result.returncode:
                continue
            container = json.loads(result.stdout)[0]
            assert container['Id'] == identity and container['Name'].lstrip('/') == name, 'container allocation changed'
            checked(['podman', 'rm', '--force', '--time', '5', identity])
        elif resource['provider'] == 'libvirt':
            ident(identity)
            result = subprocess.run(['virsh', 'domname', identity], env=ENV, capture_output=True, text=True)
            if result.returncode:
                continue
            assert result.stdout.strip() == name, 'domain allocation changed'
            subprocess.run(['virsh', 'destroy', identity], env=ENV, capture_output=True)
            assert checked(['virsh', 'domuuid', name]).decode().strip() == identity, 'domain UUID changed'
            checked(['bcvk', 'libvirt', 'rm', '--force', name])
        elif resource['provider'] == 'image_alias':
            assert re.fullmatch('localhost/ilex-job:[0-9a-f]{64}', identity), 'invalid temporary image allocation'
            result = subprocess.run(['podman', 'image', 'inspect', identity], env=ENV, capture_output=True)
            if result.returncode:
                continue
            image = json.loads(result.stdout)[0]
            assert image['Labels']['ilex.checkout-owner'] == spec['snapshot']['identity']['owner'], 'temporary image owner changed'
            checked(['podman', 'untag', identity, identity])
        else:
            raise ValueError('unknown resource provider')
    for cid in sorted((path / 'resources').glob('*.cid')):
        identity = cid.read_text().strip()
        assert re.fullmatch('[0-9a-f]{64}', identity), 'invalid Podman-created CID'
        result = subprocess.run(['podman', 'container', 'inspect', identity], env=ENV, capture_output=True)
        if result.returncode:
            continue
        container = json.loads(result.stdout)[0]
        labels = container['Config']['Labels']
        assert container['Id'] == identity and labels['ilex.remote-job'] == spec['job'] and labels['ilex.checkout-owner'] == spec['snapshot']['identity']['owner'], 'foreign job container'
        checked(['podman', 'rm', '--force', '--time', '5', identity])


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
        raise InterruptedError('job cancelled')

    signal.signal(signal.SIGTERM, cancel)
    code = 1
    guards = contextlib.ExitStack()
    admitted = False
    try:
        guards.enter_context(lock('/run/lock/ilex-builder-jobs.lock', shared=True))
        guards.enter_context(lock(owner_dir(spec['snapshot']) / 'job.lock'))
        admitted = True
        if admitted:
            capacity(spec)
            root = snapshot_path(spec['snapshot'])
            verify_source(root, spec['snapshot']['source'])
            env = ENV | spec['environment'] | {
                'ILEX_ROOT': str(root), 'ILEX_STATE_DIR': str(state.parent),
                'ILEX_REMOTE_CONTEXT': str(path / 'context.json'),
                'ILEX_ARTIFACTS': str(state / 'artifacts'),
                'ILEX_VM_LOG_DIR': str(state / 'artifacts/console'),
                'ILEX_JOB_RESOURCE_DIR': str(path / 'resources'),
                'CARGO_TARGET_DIR': str(BASE / 'cargo/targets' / owner / ('x86_64-linux-' + hashlib.sha256(checked(['rustc', '-vV'])).hexdigest()[:16])),
                'CARGO_BUILD_JOBS': str(min(4, os.cpu_count())), 'ILEX_VM_SLOTS': '1',
            }
            for name, chunks in spec.get('inputs', {}).items():
                assert name in {'iso', 'catalogue'}, 'unsupported input artifact'
                target = path / 'inputs' / ('ilex-installer.iso' if name == 'iso' else 'compositions.json')
                target.parent.mkdir(exist_ok=True)
                join_chunks(chunks, target)
                if name == 'iso':
                    env['ILEX_ISO'] = str(target)
            Path(env['XDG_RUNTIME_DIR']).mkdir(mode=0o700, exist_ok=True)
            atomic(path / 'state.json', encode({'state': 'running', 'exit_code': None}))
            with (path / 'stdout.log').open('ab', buffering=0) as out, (path / 'stderr.log').open('ab', buffering=0) as err:
                compile_started = time.monotonic()
                compile_result = subprocess.run(['cargo', 'build', '--locked', '-p', 'xtask'], cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=err, stderr=err)
                compile_seconds = time.monotonic() - compile_started
                if compile_result.returncode:
                    code = compile_result.returncode
                else:
                    binary = Path(env['CARGO_TARGET_DIR']) / 'debug/xtask'
                    operation_started = time.monotonic()
                    code = subprocess.run([str(binary), 'remote-worker', '--request', str(path / 'request.json')],
                                          cwd=root, env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=err).returncode
                    operation_seconds = time.monotonic() - operation_started
    except BaseException as error:
        with (path / 'stderr.log').open('ab') as err:
            err.write((str(error) + '\n').encode())
        code = 130 if cancelled else 1
    finally:
        # SIGTERM targets this service's cgroup, including cargo/QEMU/podman helpers.
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        if admitted:
            try:
                cleanup_resources(path, spec)
            except Exception as error:
                with (path / 'stderr.log').open('ab') as err:
                    err.write(('owned resource cleanup failed: ' + str(error) + '\n').encode())
                code = code or 1
        atomic(path / 'state.json', encode({'state': 'cancelled' if cancelled else 'complete', 'exit_code': code}))
        artifacts = artifact_inventory(path, state, include_state=admitted)
        versions = {}
        for tool in ('rustc', 'cargo', 'podman', 'bcvk', 'qemu-system-x86_64'):
            try:
                versions[tool] = checked([tool, '--version']).decode().splitlines()[0]
            except Exception:
                versions[tool] = 'unavailable'
        result = {'schema': 1, 'job': job, 'owner': owner, 'source_digest': spec['snapshot']['source']['digest'],
                  'exit_code': code, 'elapsed_seconds': time.monotonic() - started,
                  'compile_seconds': compile_seconds, 'operation_seconds': operation_seconds,
                  'data_free_before': free_before, 'data_free_after': shutil.disk_usage(BASE).free,
                  'executor_id': 'hyperv-builder:' + read_json('/var/lib/ilex-builder/provision.json')['data_uuid'],
                  'versions': versions, 'artifacts': artifacts}
        if (path / 'image.json').is_file():
            result['image'] = read_json(path / 'image.json')
        atomic(path / 'result.json', encode(result))
        (path / 'lease.json').unlink(missing_ok=True)
        guards.close()


def request(frame, payload):
    assert frame['schema'] == 1, 'unsupported job protocol version'
    op = frame['operation']
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
    if op in {'status', 'cancel', 'fetch'}:
        path, spec = load_job(frame)
        if op == 'cancel':
            if running(service(frame['job'])):
                # Signal only this unit. The wrapper freezes outputs and writes
                # its result before exiting; no maintenance/VM-stop operation.
                checked(['systemctl', 'kill', '--signal=SIGTERM', '--kill-whom=all', service(frame['job'])])
            return {}, b''
        if op == 'status':
            result = read_json(path / 'result.json') if (path / 'result.json').exists() else None
            if not result and not running(service(frame['job'])):
                # A power loss/cgroup kill remains observable and still exposes logs.
                state = owner_dir(spec['snapshot']) / 'state' / spec['snapshot']['identity']['slug']
                result = {'schema': 1, 'job': frame['job'], 'owner': frame['owner'],
                          'source_digest': spec['snapshot']['source']['digest'], 'exit_code': 130,
                          'artifacts': artifact_inventory(path, state), 'interrupted': True}
                atomic(path / 'result.json', encode(result))
            sizes = {n: (path / (n + '.log')).stat().st_size if (path / (n + '.log')).exists() else 0 for n in ('stdout', 'stderr')}
            return {'state': read_json(path / 'state.json'), 'result': result, 'logs': sizes}, b''
        name = relative(frame['path'])
        prefix, _, tail = name.partition('/')
        if prefix == 'job':
            assert tail in {'stdout.log', 'stderr.log', 'state.json', 'request.json', 'context.json', 'result.json', 'image.json'}, 'unsupported job artifact'
            target = path / tail
        else:
            assert prefix in {'artifacts', 'iso', 'disks', 'tools', 'jobs'} and not private(tail), 'unsupported output artifact'
            result = read_json(path / 'result.json')
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
    if len(sys.argv) == 4 and sys.argv[1] == '--run':
        execute(ident(sys.argv[2]), ident(sys.argv[3]))
        return
    assert len(sys.argv) == 1, 'arguments are internal only'
    # read(1) in the shell controller does not prefetch this binary stdin frame.
    line = sys.stdin.buffer.readline(MAX_FRAME + 1)
    assert len(line) <= MAX_FRAME and line.endswith(b'\n'), 'oversized/truncated request'
    frame = json.loads(line)
    assert frame['schema'] == 1, 'unsupported job protocol version'
    count = frame.get('payload_bytes', 0)
    assert isinstance(count, int) and 0 <= count <= CHUNK
    payload = sys.stdin.buffer.read(count)
    assert len(payload) == count, 'truncated chunk'
    with lock('/run/lock/ilex-builder-jobs.lock', shared=True):
        value, data = request(frame, payload)
    sys.stdout.buffer.write(encode({'schema': 1, 'ok': True, 'value': value, 'payload_bytes': len(data)}) + b'\n' + data)


if __name__ == '__main__':
    try:
        main()
    except BaseException as error:
        if len(sys.argv) == 1:
            sys.stdout.buffer.write(encode({'schema': 1, 'ok': False, 'error': str(error), 'payload_bytes': 0}) + b'\n')
        else:
            traceback.print_exc(file=sys.stderr)
        sys.exit(1)
