"""Private credential matcher shared by guest, host fixture and CI export gate.

Only high-entropy, randomly generated test credentials may be fingerprinted.
Failures identify the scan region and credential kind without reporting matching
bytes, exception text, or unredacted names. Images are excluded;
screenshots have a separate key-free handshake, and no OCR claim is made.
"""
import hashlib
import bz2
import ctypes
import gzip
import io
import json
import lzma
import os
import pathlib
import re
import resource
import stat
import subprocess
import sys
import zipfile
from contextlib import contextmanager
if 'Normalizer' not in globals():
    from log_stream import CONTEXT, Normalizer, original_offset, redact

resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
assert ctypes.CDLL(None).prctl(4, 0, 0, 0, 0) == 0  # PR_SET_DUMPABLE
try:
    from compression import zstd
except ImportError:
    zstd = None

READ_ERRORS = (OSError, EOFError, lzma.LZMAError, zipfile.BadZipFile)
if zstd:
    READ_ERRORS += (zstd.ZstdError,)

PATTERNS = {
    48: re.compile(rb'(?=([a-z]{48}))'),
    52: re.compile(rb'(?=(Ix9-[0-9a-f]{48}))'),
    71: re.compile(rb'(?=([cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}))'),
}
EDGE_BYTES = 8
EDGE_PATTERN = re.compile(rb'(?=(Ix9-[0-9a-f]{4}|[a-z]{8}|[0-9a-f]{8}))')
IMAGE_SUFFIXES = {'.png', '.jpg', '.jpeg', '.webp', '.gif', '.bmp', '.ppm'}
SECRET_KINDS = {48: 'disk-passphrase', 52: 'home-password', 71: 'recovery-key'}


def fingerprint(value):
    assert len(value) in PATTERNS
    return {'length': len(value), 'sha256': hashlib.sha256(value).hexdigest()}

def private_fingerprint(value):
    return dict(fingerprint(value), edges=[
        hashlib.sha256(part).hexdigest() for part in (value[:EDGE_BYTES], value[-EDGE_BYTES:])
    ])


def matches(data, expected, found):
    for length, pattern in PATTERNS.items():
        if length in found or not expected[length]:
            continue
        for match in pattern.finditer(data):
            if hashlib.sha256(match.group(1)).hexdigest() in expected[length]:
                yield length, match.start(1)
                break


def stream_matches(source, expected, read_size=1024 * 1024, edges=None):
    normalizer = Normalizer()
    raw_tail = b''
    normalized_tail = b''
    prior_offsets = []
    found = set()
    offset = 0

    def hits(data):
        yield from matches(data, expected, found)
        if edges and len(found) < len(PATTERNS):
            # Every exposed prefix/suffix of length >= 8 includes its exact
            # eight-byte endpoint. Scan overlapping candidates throughout the
            # stream so a clipped capture or embedded terminal transition
            # cannot disguise where a segment begins or ends.
            for match in EDGE_PATTERN.finditer(data):
                digest = hashlib.sha256(match.group(1)).hexdigest()
                for length in edges.get(digest, ()):
                    if length not in found:
                        yield length, match.start(1)

    while chunk := source.read(read_size):
        raw = raw_tail + chunk
        # Preserve literal matching for binary evidence and ANSI payloads.
        for length, start in hits(raw):
            found.add(length)
            yield length, offset - len(raw_tail) + start
        normalized, mapping = normalizer.feed(chunk)
        data = normalized_tail + normalized

        def absolute(index):
            if index < len(normalized_tail):
                return prior_offsets[index]
            return original_offset(mapping, index - len(normalized_tail))

        for length, start in hits(data):
            found.add(length)
            yield length, absolute(start)
        prior_offsets = [absolute(index) for index in range(max(0, len(data) - CONTEXT), len(data))]
        normalized_tail = data[-CONTEXT:]
        raw_tail = raw[-CONTEXT:]
        offset += len(chunk)


def public_path(path, labels=None):
    # A path itself can contain the secret. Redact before truncating it. JSON
    # escapes control characters so a name cannot inject a serial log record.
    path = os.fsdecode(path)
    for alias, original in (labels or {}).items():
        if path == alias or path.startswith(alias + '/'):
            path = original + path[len(alias):]
            break
    # Delimit a complete path so edge-fragment masking preserves its labels.
    return redact(b'<' + os.fsencode(path) + b'>')[1:-1].decode('utf-8', 'replace')[:160]


def scan(roots, fingerprints, journal=False, sync_journal=False, path_labels=None,
         require_credentials=True, private_edges=None):
    assert (fingerprints or not require_credentials) and all(f['length'] in PATTERNS and
                               re.fullmatch('[0-9a-f]{64}', f['sha256'])
                               for f in fingerprints)
    result = {'schema': 2, 'files_scanned': 0, 'images_excluded': 0,
              'read_errors': 0, 'journal_scanned': False, 'secret_found': False,
              'credentials_checked': len(fingerprints), 'findings': [],
              'findings_omitted': 0}
    expected = {length: {f['sha256'] for f in fingerprints if f['length'] == length}
                for length in PATTERNS}
    seen = set()

    def finding(rule, path, region, **details):
        if len(result['findings']) == 24:
            result['findings_omitted'] += 1
            return
        result['findings'].append({'rule': rule, 'path': public_path(path, path_labels),
                                   'region': region, **details})

    def unreadable(path, region, error=None, **details):
        result['read_errors'] += 1
        if error is not None:
            details['error'] = type(error).__name__
            details['errno'] = getattr(error, 'errno', None)
        finding('scan-read-error', path, region, **details)

    def check_stream(source, path, region):
        for length, start in stream_matches(source, expected, edges=private_edges):
            result['secret_found'] = True
            finding('credential-fingerprint-match', path, region,
                    secret_kind=SECRET_KINDS[length], offset=start)

    def walk_error(error):
        unreadable(error.filename or 'scan-root', 'directory-walk', error)

    def journalctl(region, *args):
        try:
            return subprocess.run(['journalctl', *args], capture_output=True, check=True,
                                  timeout=60)
        except subprocess.CalledProcessError as error:
            unreadable('system-journal', region, returncode=error.returncode)
        except subprocess.TimeoutExpired:
            unreadable('system-journal', region, error_code='timeout')
        except OSError as error:
            unreadable('system-journal', region, error)
        return None

    if sync_journal:
        journalctl('journal-sync', '--sync')
    for root in roots:
        # Several target locations are optional (e.g. /var/lib/anaconda on
        # bootc). An existing directory that cannot be walked still fails.
        try:
            pathlib.Path(root).stat()
        except FileNotFoundError:
            continue
        except OSError as error:
            unreadable(root, 'scan-root', error)
            continue
        root = pathlib.Path(root)
        check_stream(io.BytesIO(os.fsencode(root.name)), root, 'directory-name')
        for directory, subdirs, files in os.walk(root, followlinks=False, onerror=walk_error):
            directory = pathlib.Path(directory)
            for name in subdirs:
                check_stream(io.BytesIO(os.fsencode(name)), directory / name, 'directory-name')
            for name in files:
                path = directory / name
                check_stream(io.BytesIO(os.fsencode(name)), path, 'file-name')
                region = 'file-metadata'
                try:
                    info = path.lstat()
                    if not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino) in seen:
                        continue
                    seen.add((info.st_dev, info.st_ino))
                    region = 'file-header'
                    with path.open('rb') as source:
                        header = source.read(12)
                    if path.suffix.lower() in IMAGE_SUFFIXES and (
                            header.startswith((b'\x89PNG\r\n\x1a\n', b'\xff\xd8\xff', b'GIF8', b'BM', b'P6\n')) or
                            (header.startswith(b'RIFF') and header[8:12] == b'WEBP')):
                        result['images_excluded'] += 1
                        continue

                    region = 'file-bytes'
                    with path.open('rb') as source:
                        check_stream(source, path, region)
                    opener = (gzip.open if header.startswith(b'\x1f\x8b') else
                              bz2.open if header.startswith(b'BZh') else
                              lzma.open if header.startswith(b'\xfd7zXZ\x00') else
                              zstd.open if zstd and header.startswith(b'\x28\xb5\x2f\xfd') else None)
                    if header.startswith(b'\x28\xb5\x2f\xfd') and zstd is None:
                        unreadable(path, 'zstd', error_code='decompressor-unavailable')
                    if opener:
                        region = opener.__module__
                        with opener(path, 'rb') as source:
                            check_stream(source, path, region)
                    elif header.startswith(b'PK\x03\x04'):
                        region = 'zip-directory'
                        with zipfile.ZipFile(path) as archive:
                            for index, member in enumerate(archive.infolist()):
                                if not member.is_dir():
                                    region = f'zip-member-{index}'
                                    with archive.open(member) as source:
                                        check_stream(source, path, region)
                    result['files_scanned'] += 1
                except READ_ERRORS as error:
                    unreadable(path, region, error)
    if journal:
        output = journalctl('journal-export', '--no-pager', '-o', 'export')
        if output is not None:
            check_stream(io.BytesIO(output.stdout), 'system-journal', 'journal-export')
            result['journal_scanned'] = True
    return result


class EvidenceError(ValueError):
    def __init__(self, path, region, error=None):
        super().__init__(region)
        self.finding = {'rule': 'scan-read-error', 'path': public_path(path), 'region': region}
        if error is not None:
            self.finding.update(error=type(error).__name__, errno=getattr(error, 'errno', None))


@contextmanager
def evidence_io(path, region):
    try:
        yield
    except (*READ_ERRORS, UnicodeError, json.JSONDecodeError) as error:
        raise EvidenceError(path, region, error) from None


def evidence_json(path, region):
    with evidence_io(path, region):
        return json.loads(path.read_text())


def fingerprint_records(path, private=False):
    region = 'private-fingerprints' if private else 'public-fingerprints'
    records = evidence_json(path, region)
    if not isinstance(records, list) or not all(
            isinstance(record, dict) and type(record.get('length')) is int
            and record['length'] in PATTERNS and isinstance(record.get('sha256'), str)
            and re.fullmatch('[0-9a-f]{64}', record['sha256']) for record in records):
        raise EvidenceError(path, region)
    return records


def export_files(directory):
    def walk_error(error):
        raise EvidenceError(error.filename or directory, 'artifact-directory-walk', error) from None

    for parent, subdirs, files in os.walk(directory, onerror=walk_error):
        for name in subdirs:
            path = pathlib.Path(parent) / name
            with evidence_io(path, 'artifact-directory'):
                if name == 'raw-logs' or not stat.S_ISDIR(path.lstat().st_mode):
                    raise EvidenceError(path, 'artifact-directory')
        for name in files:
            path = pathlib.Path(parent) / name
            with evidence_io(path, 'artifact-file'):
                lower = name.lower()
                if (not stat.S_ISREG(path.lstat().st_mode)
                        or ('serial' in lower and not lower.endswith('-serial.redacted.log'))):
                    raise EvidenceError(path, 'artifact-file')
            yield path


def verify_export(root, tiers, required=False):

    for tier in tiers:
        directory = root / tier
        with evidence_io(directory, 'artifact-root'):
            try:
                mode = directory.lstat().st_mode
            except FileNotFoundError:
                if required:
                    raise
                continue
            if not stat.S_ISDIR(mode):
                raise EvidenceError(directory, 'artifact-root')
        for _ in export_files(directory):
            pass


ENCRYPTED_TIERS = ('encrypted-install', 'encrypted-existing-space', 'encrypted-interrupted')
LIFECYCLE_TIER = 'encrypted-upgrade-rollback'
FINGERPRINT_LENGTHS = {
    'encrypted-install': [48, 71],
    'encrypted-existing-space': [48, 71],
    'encrypted-interrupted': [48, 71, 71],
    LIFECYCLE_TIER: [48, 48, 52, 71],
}
EXPORT_REPORT = 'final-export-privacy.json'


def encrypted_tiers(mode):
    return {'false': (), 'true': (*ENCRYPTED_TIERS, LIFECYCLE_TIER),
            'fresh': (ENCRYPTED_TIERS[0],), 'installer': ENCRYPTED_TIERS,
            'lifecycle': (LIFECYCLE_TIER,), 'release': (ENCRYPTED_TIERS[0], LIFECYCLE_TIER)}[mode]


def artifact_roots(root, mode):
    tiers = ('security', *encrypted_tiers(mode))
    verify_export(root, tiers, required=True)
    return [root / tier for tier in tiers]


def artifact_snapshot(root, paths):
    contents = {}
    for path in sorted(file for directory in paths for file in export_files(directory)):
        with evidence_io(path, 'artifact-digest'):
            with path.open('rb') as source:
                contents[path.relative_to(root).as_posix()] = hashlib.file_digest(source, 'sha256').hexdigest()
    return contents


def seal_export(root, marker, mode, private_directory=None):
    with evidence_io(marker, 'seal-reset'):
        marker.unlink(missing_ok=True)
    paths = artifact_roots(root, mode)
    initial = artifact_snapshot(root, paths)
    credentials = [entry for name in initial if name.endswith('credentials.json')
                   for entry in fingerprint_records(root / name)]
    edges = {}
    tiers = encrypted_tiers(mode)
    if tiers:
        for tier in tiers:
            definition = root / tier / 'result.json'
            outcome = evidence_json(definition, 'qualification-result')
            if not isinstance(outcome, dict) or outcome.get('qualification') != 'passed':
                raise EvidenceError(definition, 'qualification-result')
            fingerprints = definition.with_name('installer-credentials.json')
            public = fingerprint_records(fingerprints)
            if tier == LIFECYCLE_TIER:
                public += fingerprint_records(definition.with_name('home-credentials.json'))
            if sorted(entry['length'] for entry in public) != FINGERPRINT_LENGTHS[tier]:
                raise EvidenceError(fingerprints, 'public-fingerprints')
            private_path = private_directory / (tier + '.json')
            private = fingerprint_records(private_path, private=True)
            if [{k: item[k] for k in ('length', 'sha256')} for item in private] != public:
                raise EvidenceError(private_path, 'private-fingerprints')
            for entry in private:
                if (not isinstance(entry.get('edges'), list) or len(entry['edges']) != 2
                        or not all(isinstance(item, str) and re.fullmatch('[0-9a-f]{64}', item)
                                   for item in entry['edges'])):
                    raise EvidenceError(private_path, 'private-fingerprints')
                for digest in entry['edges']:
                    edges.setdefault(digest, set()).add(entry['length'])
    report = scan(paths, credentials, require_credentials=bool(tiers), private_edges=edges)
    if report['secret_found'] or report['read_errors']:
        return report
    require_same_snapshot(root, initial, artifact_snapshot(root, artifact_roots(root, mode)))
    receipt = json.dumps(report) + '\n'
    if tiers:
        path = root / EXPORT_REPORT
        with evidence_io(path, 'privacy-report-write'):
            with path.open('x') as output:
                output.write(receipt)
        initial[EXPORT_REPORT] = hashlib.sha256(receipt.encode()).hexdigest()
    with evidence_io(marker, 'seal-write'):
        with marker.open('x') as output:
            json.dump(initial, output, sort_keys=True)
    return report


def require_same_snapshot(root, expected, current):
    for name in sorted(expected.keys() | current.keys()):
        if expected.get(name) != current.get(name):
            raise EvidenceError(root / name, 'artifact-changed')


def check_seal(root, marker, mode):
    current = artifact_snapshot(root, artifact_roots(root, mode))
    if encrypted_tiers(mode):
        report = root / EXPORT_REPORT
        with evidence_io(report, 'privacy-report-read'):
            if not stat.S_ISREG(report.lstat().st_mode):
                raise EvidenceError(report, 'privacy-report-read')
            current[EXPORT_REPORT] = hashlib.sha256(report.read_bytes()).hexdigest()
    expected = evidence_json(marker, 'seal-read')
    if not isinstance(expected, dict) or not all(
            isinstance(digest, str) and re.fullmatch('[0-9a-f]{64}', digest)
            for digest in expected.values()):
        raise EvidenceError(marker, 'seal-read')
    require_same_snapshot(root, expected, current)


def seal_profile_export(root, marker, private_directory):
    tier = 'installer-profile-setup'
    verify_export(root, (tier,))
    paths = [root / tier]
    initial = artifact_snapshot(root, paths)
    result = json.loads((paths[0] / 'result.json').read_text())
    if result['stage'] != 'complete':
        raise ValueError('profile setup qualification incomplete')
    public = [entry for path in paths[0].rglob('*credentials.json')
              for entry in json.loads(path.read_text())]
    private = json.loads((private_directory / (tier + '.json')).read_text())
    if sorted(entry['length'] for entry in private) != [48, 48, 48, 71] or not all(p in private_projection(private) for p in public):
        raise ValueError('profile setup fingerprints incomplete')
    edges = {}
    for entry in private:
        if len(entry['edges']) != 2 or not all(re.fullmatch('[0-9a-f]{64}', digest) for digest in entry['edges']):
            raise ValueError('invalid private endpoint fingerprints')
        for digest in entry['edges']:
            edges.setdefault(digest, set()).add(entry['length'])
    report = scan(paths, private_projection(private), require_credentials=True, private_edges=edges)
    if report['secret_found'] or report['read_errors']:
        return report
    if artifact_snapshot(root, paths) != initial:
        raise ValueError('profile setup evidence changed during scan')
    with marker.open('x') as output:
        json.dump(initial, output, sort_keys=True)
    return report


def private_projection(private):
    return [{k: item[k] for k in ('length', 'sha256')} for item in private]


if __name__ == '__main__':
    try:
        root = pathlib.Path(sys.argv[1])
        if sys.argv[2:3] == ['--check-profile-seal']:
            paths = [root / 'installer-profile-setup']
            verify_export(root, ('installer-profile-setup',))
            if json.loads(pathlib.Path(sys.argv[3]).read_text()) != artifact_snapshot(root, paths):
                raise ValueError('profile setup evidence changed after scan')
            raise SystemExit(0)
        if sys.argv[2:3] == ['--verify-export']:
            verify_export(root, sys.argv[3:])
            raise SystemExit(0)
        if sys.argv[2:3] == ['--check-seal']:
            mode = sys.argv[4]
            check_seal(root, pathlib.Path(sys.argv[3]), mode)
            name = sys.argv[5]
            if name == 'security-evidence' or (name == 'release-security-evidence' and mode == 'false'):
                paths = ['security']
            elif name == 'encrypted-installer-evidence' and mode in ('true', 'installer'):
                paths = [*encrypted_tiers(mode), EXPORT_REPORT]
            elif ((name == 'encrypted-lifecycle-evidence' and mode in ('lifecycle', 'fresh'))
                  or (name == 'encrypted-release-lifecycle-evidence' and mode in ('release', 'fresh'))):
                paths = ['security', *encrypted_tiers(mode), EXPORT_REPORT]
            else:
                raise ValueError('unknown artifact scope')
            print('paths<<VERIFIED_QUALIFICATION_PATHS')
            for path in paths:
                print(root / path)
            print('VERIFIED_QUALIFICATION_PATHS')
            raise SystemExit(0)
        if sys.argv[2:3] == ['--seal-profile-export']:
            result = seal_profile_export(root, pathlib.Path(sys.argv[3]), pathlib.Path(sys.argv[4]))
        elif sys.argv[2:3] == ['--seal-export']:
            result = seal_export(root, pathlib.Path(sys.argv[3]), sys.argv[4],
                                 pathlib.Path(sys.argv[5]) if len(sys.argv) > 5 else None)
        else:
            fingerprints = []
            for path in root.rglob('*credentials.json'):
                fingerprints.extend(fingerprint_records(path))
            result = scan([root], fingerprints)
    except EvidenceError as error:
        result = {'schema': 2, 'secret_found': False, 'read_errors': 1,
                  'verification_failed': True, 'findings': [error.finding]}
    except Exception:
        # A malformed archive or filename may contain credential material.
        # CI must fail without rendering exception text, paths or source data.
        result = {'schema': 2, 'secret_found': False, 'read_errors': 1,
                  'verification_failed': True}
    print(json.dumps(result), file=sys.stderr if sys.argv[2:3] == ['--check-seal'] else sys.stdout)
    raise SystemExit(1 if result['secret_found'] or result['read_errors'] else 0)
