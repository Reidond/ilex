"""Run the synthetic serial redaction and fingerprint regression matrix."""
import io
import pathlib
import tempfile

from diagnostics import LOG_BYTES, emit, extract, redacted_tail, tail
from privacy import PATTERNS, fingerprint, stream_matches, verify_export


def check():
    wraps = (b'', b'\n', b'\r\n\t ', b'\0', b'\x1b[31m',
             b'\r\n \x1b[0m\t', b'\x1b]8;;file://here\x1b\\')
    checked = 0
    for secret, letter in ((b'z' * 48, b'z'), (b'-'.join([b'u' * 8] * 8), b'u')):
        length = len(secret)
        expected = {size: set() for size in PATTERNS}
        expected[length].add(fingerprint(secret)['sha256'])
        alphabet = {letter[0]} | ({ord('-')} if length == 71 else set())

        def zero(data):
            nonlocal checked
            assert not alphabet.intersection(redacted_tail(data)), (length, checked)
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
    print(f'{checked} redaction cases, 0 credential bytes retained; fingerprint and export gates passed')


if __name__ == '__main__':
    check()
