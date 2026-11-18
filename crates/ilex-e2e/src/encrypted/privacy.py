"""Private credential matcher shared by guest, host fixture and CI export gate.

Only high-entropy, randomly generated test credentials may be fingerprinted.
Neither a matching path, line nor credential is returned. Images are excluded;
screenshots have a separate key-free handshake, and no OCR claim is made.
"""
import hashlib
import bz2
import gzip
import json
import lzma
import os
import pathlib
import re
import stat
import subprocess
import sys
import zipfile

PATTERNS = {
    48: re.compile(rb'(?=([a-z]{48}))'),
    71: re.compile(rb'(?=([cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}))'),
}
IMAGE_SUFFIXES = {'.png', '.jpg', '.jpeg', '.webp', '.gif', '.bmp', '.ppm'}


def fingerprint(value):
    assert len(value) in PATTERNS
    return {'length': len(value), 'sha256': hashlib.sha256(value).hexdigest()}


def contains(data, fingerprints):
    for length, pattern in PATTERNS.items():
        expected = {f['sha256'] for f in fingerprints if f['length'] == length}
        if expected and any(hashlib.sha256(m.group(1)).hexdigest() in expected
                            for m in pattern.finditer(data)):
            return True
    return False


def scan(roots, fingerprints, journal=False):
    assert fingerprints and all(f['length'] in PATTERNS and
                               re.fullmatch('[0-9a-f]{64}', f['sha256'])
                               for f in fingerprints)
    result = {'schema': 2, 'files_scanned': 0, 'images_excluded': 0,
              'read_errors': 0, 'journal_scanned': False, 'secret_found': False,
              'credentials_checked': len(fingerprints)}
    seen = set()
    def unreadable(_error):
        result['read_errors'] += 1
    for root in roots:
        # Several target locations are optional (e.g. /var/lib/anaconda on
        # bootc). An existing directory that cannot be walked still fails.
        try:
            pathlib.Path(root).stat()
        except FileNotFoundError:
            continue
        except OSError:
            result['read_errors'] += 1
            continue
        for directory, _, files in os.walk(root, followlinks=False, onerror=unreadable):
            for name in files:
                path = pathlib.Path(directory) / name
                try:
                    info = path.lstat()
                    if not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino) in seen:
                        continue
                    seen.add((info.st_dev, info.st_ino))
                    with path.open('rb') as source:
                        header = source.read(12)
                    if path.suffix.lower() in IMAGE_SUFFIXES and (
                            header.startswith((b'\x89PNG\r\n\x1a\n', b'\xff\xd8\xff', b'GIF8', b'BM', b'P6\n')) or
                            (header.startswith(b'RIFF') and header[8:12] == b'WEBP')):
                        result['images_excluded'] += 1
                        continue

                    def check_stream(source):
                        previous = b''
                        while chunk := source.read(1024 * 1024):
                            data = previous + chunk
                            result['secret_found'] |= contains(data, fingerprints)
                            previous = data[-70:]
                    with path.open('rb') as source:
                        check_stream(source)
                    opener = (gzip.open if header.startswith(b'\x1f\x8b') else
                              bz2.open if header.startswith(b'BZh') else
                              lzma.open if header.startswith(b'\xfd7zXZ\x00') else None)
                    if opener:
                        with opener(path, 'rb') as source:
                            check_stream(source)
                    elif header.startswith(b'PK\x03\x04'):
                        with zipfile.ZipFile(path) as archive:
                            for member in archive.infolist():
                                if not member.is_dir():
                                    with archive.open(member) as source:
                                        check_stream(source)
                    result['files_scanned'] += 1
                except (OSError, EOFError, lzma.LZMAError, zipfile.BadZipFile):
                    result['read_errors'] += 1
    if journal:
        output = subprocess.run(['journalctl', '--no-pager', '-o', 'export'],
                                capture_output=True, check=True)
        result['secret_found'] |= contains(output.stdout, fingerprints)
        result['journal_scanned'] = True
    return result


if __name__ == '__main__':
    root = pathlib.Path(sys.argv[1])
    fingerprints = []
    for path in root.rglob('*credentials.json'):
        fingerprints.extend(json.loads(path.read_text()))
    result = scan([root], fingerprints)
    print(json.dumps(result))
    raise SystemExit(1 if result['secret_found'] or result['read_errors'] else 0)
