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
    71: re.compile(rb'(?=([cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}))'),
}
EDGE_BYTES = 8
EDGE_PATTERN = re.compile(rb'(?=([a-z]{8}))')
IMAGE_SUFFIXES = {'.png', '.jpg', '.jpeg', '.webp', '.gif', '.bmp', '.ppm'}
SECRET_KINDS = {48: 'disk-passphrase', 71: 'recovery-key'}


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
        for directory, _, files in os.walk(root, followlinks=False, onerror=walk_error):
            for name in files:
                path = pathlib.Path(directory) / name
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


def verify_export(root, tiers):
    def walk_error(error):
        raise error

    for tier in tiers:
        directory = root / tier
        if not directory.exists() and not directory.is_symlink():
            continue
        if not stat.S_ISDIR(directory.lstat().st_mode):
            raise ValueError('invalid artifact root')
        for parent, subdirs, files in os.walk(directory, onerror=walk_error):
            for name in subdirs:
                if name == 'raw-logs' or not stat.S_ISDIR((pathlib.Path(parent) / name).lstat().st_mode):
                    raise ValueError('raw log directory in artifact tree')
            for name in files:
                kind = (pathlib.Path(parent) / name).lstat().st_mode
                name = name.lower()
                if (not stat.S_ISREG(kind)
                        or ('serial' in name and not name.endswith('-serial.redacted.log'))):
                    raise ValueError('raw serial log or unsupported file in artifact tree')


ENCRYPTED_TIERS = ('encrypted-install', 'encrypted-existing-space', 'encrypted-interrupted')
EXPORT_REPORT = 'final-export-privacy.json'


def artifact_roots(root, encrypted):
    tiers = ('security', *ENCRYPTED_TIERS) if encrypted else ('security',)
    verify_export(root, tiers)
    paths = [root / tier for tier in tiers]
    if not all(path.is_dir() for path in paths):
        raise ValueError('qualification evidence missing')
    return paths


def artifact_snapshot(root, paths):
    contents = {}
    for path in sorted(file for directory in paths for file in directory.rglob('*')):
        if path.is_dir():
            continue
        with path.open('rb') as source:
            contents[path.relative_to(root).as_posix()] = hashlib.file_digest(source, 'sha256').hexdigest()
    return contents


def seal_export(root, marker, encrypted, private_directory=None):
    marker.unlink(missing_ok=True)
    paths = artifact_roots(root, encrypted)
    initial = artifact_snapshot(root, paths)
    credentials = [entry for directory in paths for path in directory.rglob('*credentials.json')
                   for entry in json.loads(path.read_text())]
    edges = {}
    if encrypted:
        for tier in ENCRYPTED_TIERS:
            definition = root / tier / 'result.json'
            if json.loads(definition.read_text())['qualification'] != 'passed':
                raise ValueError('encrypted qualification incomplete')
            fingerprints = definition.with_name('installer-credentials.json')
            required = [48, 71, 71] if tier == 'encrypted-interrupted' else [48, 71]
            public = json.loads(fingerprints.read_text())
            if sorted(entry['length'] for entry in public) != required:
                raise ValueError('encrypted qualification fingerprints incomplete')
            private = json.loads((private_directory / (tier + '.json')).read_text())
            if [{k: item[k] for k in ('length', 'sha256')} for item in private] != public:
                raise ValueError('private fingerprint receipt does not cover the public set')
            for entry in private:
                if (len(entry['edges']) != 2
                        or not all(re.fullmatch('[0-9a-f]{64}', item) for item in entry['edges'])):
                    raise ValueError('invalid private endpoint fingerprints')
                for digest in entry['edges']:
                    edges.setdefault(digest, set()).add(entry['length'])
    report = scan(paths, credentials, require_credentials=encrypted, private_edges=edges)
    if report['secret_found'] or report['read_errors']:
        return report
    if artifact_snapshot(root, artifact_roots(root, encrypted)) != initial:
        raise ValueError('qualification evidence changed during scan')
    receipt = json.dumps(report) + '\n'
    if encrypted:
        path = root / EXPORT_REPORT
        with path.open('x') as output:
            output.write(receipt)
        initial[EXPORT_REPORT] = hashlib.sha256(receipt.encode()).hexdigest()
    with marker.open('x') as output:
        json.dump(initial, output, sort_keys=True)
    return report


def check_seal(root, marker, encrypted):
    current = artifact_snapshot(root, artifact_roots(root, encrypted))
    if encrypted:
        report = root / EXPORT_REPORT
        if not stat.S_ISREG(report.lstat().st_mode):
            raise ValueError('invalid privacy report')
        current[EXPORT_REPORT] = hashlib.sha256(report.read_bytes()).hexdigest()
    if json.loads(marker.read_text()) != current:
        raise ValueError('qualification evidence changed after scan')


if __name__ == '__main__':
    try:
        root = pathlib.Path(sys.argv[1])
        if sys.argv[2:3] == ['--verify-export']:
            verify_export(root, sys.argv[3:])
            raise SystemExit(0)
        if sys.argv[2:3] == ['--check-seal']:
            encrypted = sys.argv[4] == 'true'
            check_seal(root, pathlib.Path(sys.argv[3]), encrypted)
            name = sys.argv[5]
            if name == 'security-evidence':
                paths = ['security']
            elif name == 'encrypted-installer-evidence' and encrypted:
                paths = [*ENCRYPTED_TIERS, EXPORT_REPORT]
            else:
                raise ValueError('unknown artifact scope')
            print('paths<<VERIFIED_QUALIFICATION_PATHS')
            for path in paths:
                print(root / path)
            print('VERIFIED_QUALIFICATION_PATHS')
            raise SystemExit(0)
        if sys.argv[2:3] == ['--seal-export']:
            result = seal_export(root, pathlib.Path(sys.argv[3]), sys.argv[4] == 'true',
                                 pathlib.Path(sys.argv[5]) if len(sys.argv) > 5 else None)
        else:
            fingerprints = []
            for path in root.rglob('*credentials.json'):
                fingerprints.extend(json.loads(path.read_text()))
            result = scan([root], fingerprints)
    except Exception:
        # A malformed archive or filename may contain credential material.
        # CI must fail without rendering exception text, paths or source data.
        result = {'schema': 2, 'secret_found': False, 'read_errors': 1,
                  'verification_failed': True}
    print(json.dumps(result), file=sys.stderr if sys.argv[2:3] == ['--check-seal'] else sys.stdout)
    raise SystemExit(1 if result['secret_found'] or result['read_errors'] else 0)
