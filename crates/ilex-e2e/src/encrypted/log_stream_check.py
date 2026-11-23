"""Run the synthetic serial redaction and fingerprint regression matrix."""
import io
import json
import pathlib
import re
import subprocess
import sys
import tempfile

from diagnostics import LOG_BYTES, emit, extract, redacted_tail, tail
from log_stream import CANCEL, CSI, ESC, OSC, ST, STRINGS, Normalizer
from privacy import (ENCRYPTED_TIERS, LIFECYCLE_TIER, PATTERNS, EvidenceError, check_seal, fingerprint,
                     private_fingerprint, seal_export, stream_matches, verify_export)


def terminal_sequences():
    def encodings(c1):
        return bytes((ESC, c1 - 0x40)), bytes((c1,)), bytes((0xC2, c1))

    for code in range(32):
        if code != ESC:
            yield bytes((code,))
    yield b'\x7f'
    for code in range(0x80, 0xA0):
        if code not in STRINGS | {CSI}:
            yield from encodings(code)
    for intermediates in (b'', b'(', b'# '):
        yield bytes((ESC,)) + intermediates + b'0'
    for intro in encodings(CSI):
        for parameters in (b'', b'?25', b'38:2;0;1;2'):
            for intermediates in (b'', b' ', b'$ '):
                yield intro + parameters + intermediates + b'm'
        for cancel in CANCEL:
            yield intro + b'31' + bytes((cancel,))
    for code in STRINGS:
        for intro in encodings(code):
            for terminator in (*encodings(ST), b'\x07' if code == OSC else b'\x18'):
                yield intro + b'8;;file://here' + terminator


def check_artifact_seal(root):
    security = root / 'security'
    security.mkdir()
    marker = root / 'seal'
    private = root / 'private'
    private.mkdir()
    secret = b'z' * 48
    recovery = b'-'.join([b'u' * 8] * 8)
    reissued = b'-'.join([b'v' * 8] * 8)
    home = b'Ix9-' + b'0a' * 24
    for tier in (*ENCRYPTED_TIERS, LIFECYCLE_TIER):
        path = root / tier
        path.mkdir()
        (path / 'result.json').write_text('{"qualification":"passed"}')
        values = [secret, recovery, reissued] if tier == 'encrypted-interrupted' else [secret, recovery]
        (path / 'installer-credentials.json').write_text(
            json.dumps([fingerprint(value) for value in values]))
        if tier == LIFECYCLE_TIER:
            (path / 'home-credentials.json').write_text(json.dumps([fingerprint(home)]))
            values.append(home)
        (private / (tier + '.json')).write_text(
            json.dumps([private_fingerprint(value) for value in values]))
    log = security / 'fixture-serial.redacted.log'

    def refuses_upload():
        try:
            check_seal(root, marker, 'true')
        except (OSError, ValueError):
            return
        raise AssertionError('upload accepted unscanned evidence')

    log.write_bytes(b'boot complete')
    refuses_upload()
    fallback = root / 'encrypted-existing-space' / 'repair-fallback-credentials.json'
    fallback.write_text('{"passphrase_slot":0,"recovery_slot":1,"authenticated":true}')

    def rejected_export(path, region):
        process = subprocess.run([
            sys.executable, '-B', str(pathlib.Path(__file__).with_name('privacy.py')),
            str(root), '--seal-export', str(marker), 'true', str(private),
        ], capture_output=True, text=True)
        assert process.returncode == 1 and not process.stderr
        report = json.loads(process.stdout)
        assert report['verification_failed'] and report['read_errors'] == 1
        assert report['findings'][0]['path'].endswith('/' + path.name)
        assert report['findings'][0]['region'] == region
        refuses_upload()

    rejected_export(fallback, 'public-fingerprints')
    fallback.rename(fallback.with_name('repair-fallback-authentication.json'))
    log.write_bytes(secret[:4] + b'\x9b3\0;1m\r\n' + secret[4:8])
    assert seal_export(root, marker, 'true', private)['secret_found']
    refuses_upload()
    log.write_bytes(home[:4] + b'\x1b[1m' + home[4:8])
    assert seal_export(root, marker, 'true', private)['secret_found']
    refuses_upload()
    log.write_bytes(b'boot complete')
    assert not seal_export(root, marker, 'true', private)['secret_found']
    check_seal(root, marker, 'true')
    log.write_bytes(b'changed after successful scan')
    refuses_upload()
    receipt = private / (ENCRYPTED_TIERS[0] + '.json')
    receipt.unlink()
    rejected_export(receipt, 'private-fingerprints')
    secret_path = security / (secret.decode() + '.json')
    secret_path.symlink_to(receipt)
    try:
        verify_export(root, ['security'])
    except EvidenceError as error:
        assert error.finding['path'].endswith('/security/' + '*' * len(secret) + '.json')
    else:
        raise AssertionError('artifact symlink accepted for export')


def check():
    wraps = (b'', b'\n', b'\r\n\t ', b'\0', b'\x1b[31m',
             b'\r\n \x1b[0m\t', b'\x1b]8;;file://here\x1b\\')
    checked = 0
    for secret, letter in ((b'z' * 48, b'z'), (b'-'.join([b'u' * 8] * 8), b'u')):
        length = len(secret)
        expected = {size: set() for size in PATTERNS}
        expected[length].add(fingerprint(secret)['sha256'])
        alphabet = {letter[0]} | ({ord('-')} if length == 71 else set())

        def zero(data, context=None):
            nonlocal checked
            assert not alphabet.intersection(redacted_tail(data)), (length, checked, context)
            checked += 1

        for split in range(1, length):
            for wrap in wraps:
                full = b'<>' + secret[:split] + wrap + secret[split:] + b'<>'
                zero(full)
                for size in (1, 5, split + 2, len(full) - 1):
                    assert list(stream_matches(io.BytesIO(full), expected, size)) == [(length, 2)]
                for boundary in range(len(wrap) + 1):
                    zero(b'<>' + secret[:split] + wrap[:boundary])
                    zero(wrap[boundary:] + secret[split:] + b'<>')
                    zero(secret[:split] + wrap[:boundary])
                    zero(wrap[boundary:] + secret[split:])

        for seq in terminal_sequences():
            # Pad every state transition, including escape introducers,
            # parameters, intermediates, string payloads and terminators.
            units = re.findall(rb'\xc2[\x80-\x9f]|.', seq, flags=re.S)
            for terminal in (seq, b'\0'.join(units)):
                for split in (1, length // 2, length - 1):
                    full = b'<>' + secret[:split] + terminal + secret[split:] + b'<>'
                    zero(full)
                    assert list(stream_matches(io.BytesIO(full), expected, 1)) == [(length, 2)], terminal
                    for boundary in range(len(terminal) + 1):
                        zero(b'<>' + secret[:split] + terminal[:boundary], ('end', split, terminal, boundary))
                        zero(terminal[boundary:] + secret[split:] + b'<>', ('start', split, terminal, boundary))

        endpoints = {digest: {length} for digest in private_fingerprint(secret)['edges']}
        # Start after the opener in a clipped DCS/OSC. A UTF-8 continuation
        # that resembles raw ST, a DCS BEL, and line breaks within the string
        # all precede the actual resynchronization point.
        for fake_end in (b'clip:\xe2\x80\x9c', b'clip:\x07', b'clip:\xe2\x80\x9c\r\n\x07',
                         b'\x9c', b'\x80\x9c'):
            for real_end in (b'\x1b\\', b'\x1b\0\\', b'\x9c', b'\xc2\x9c', b'\x18', b'\x07'):
                before = fake_end + b'\r\nmore:' + real_end
                for count in range(8, length):
                    for fragment in (secret[-count:], secret[:count]):
                        clipped = before + fragment + b'<>'
                        zero(clipped, ('clipped-control-string', fake_end, real_end, count))
                        for size in (1, 23):
                            assert list(stream_matches(io.BytesIO(clipped), expected, size, edges=endpoints))
        assert not list(stream_matches(io.BytesIO(b'<>' + secret[:7] + b'<>'),
                                       expected, 1, edges=endpoints))

        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            path = root / 'captured.log'
            # Exercise both the output cutoff and an unbounded input file.
            with path.open('wb') as stream:
                stream.seek(64 * 1024 * 1024)
                stream.write(b'<>\n' + secret[:1] + b'\r\n\x1b[31m' + secret[1:]
                             + b'\n' + b'#' * (LOG_BYTES - length))
            assert len(tail(path)) == LOG_BYTES
            assert not alphabet.intersection(tail(path))
            checked += 1
            # Overlong wrapping consumes the whole lookbehind margin.
            path.write_bytes(b'<>' + secret[:1] + b'\r\n' * (LOG_BYTES * 2)
                             + secret[1:] + b'\n' + b'#' * (LOG_BYTES - length))
            assert not alphabet.intersection(tail(path))
            checked += 1

            output = root / 'evidence'
            output.mkdir()
            path = root / 'installer-serial.log'
            with path.open('wb') as stream:
                stream.write(b'<>' + secret[:-1] + b'\r\n')
                emit(stream, 'program.log', b'<>' + secret[:-1] + b'\r\n')
                emit(stream, 'program.log', secret[1:] + b'<>')
            assert not alphabet.intersection(extract(path, output))
            framed = (output / 'installer-program.log').read_bytes()
            assert framed.endswith(b'<>') and not alphabet.intersection(framed)
            checked += 2
            raw = output / 'installer-serial.log'
            raw.write_bytes(b'no credentials required to reject a raw filename')
            try:
                verify_export(root, ['evidence'])
            except ValueError:
                pass
            else:
                raise AssertionError('raw serial file accepted for export')
            raw.unlink()
            verify_export(root, ['evidence'])
    assert Normalizer().feed('a\u2019b'.encode(), final=True)[0] == 'a\u2019b'.encode()
    with tempfile.TemporaryDirectory() as directory:
        check_artifact_seal(pathlib.Path(directory))
    print(f'{checked} redaction cases, 0 credential bytes retained; fingerprint and export gates passed')


if __name__ == '__main__':
    check()
