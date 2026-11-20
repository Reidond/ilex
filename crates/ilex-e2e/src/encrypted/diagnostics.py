"""Bounded diagnostic text for the disposable encrypted install fixture."""
import base64
import os
import pathlib
import stat
import sys

if 'Normalizer' not in globals():
    from log_stream import CONTEXT, redact

LOG_BYTES = 32 * 1024
READ_BYTES = 8192
WINDOW_BYTES = LOG_BYTES * 2 + CONTEXT
PREFIX = b'ILEX-ENCRYPTED-LOG '
INSTALLER_LOGS = (
    'anaconda.log', 'program.log', 'storage.log', 'packaging.log',
    'post.log', 'preload.log', 'driver.log', 'luksDump.txt',
    'leak-scan.json', 'finaliser-leak-scan.json',
)
FRAME_BYTES = len(PREFIX) + max(map(len, INSTALLER_LOGS)) + 4 * ((LOG_BYTES + 2) // 3) + 3


def redacted_tail(data):
    text = redact(data)[-LOG_BYTES:].decode('utf-8', 'replace').encode()
    return text[-LOG_BYTES:].decode('utf-8', 'ignore').encode()


def open_capture(path):
    return os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), 'rb')


def backward(stream):
    offset = stream.seek(0, os.SEEK_END)
    while offset:
        size = min(offset, READ_BYTES)
        offset -= size
        stream.seek(offset)
        yield stream.read(size)


def tail(path):
    with open_capture(path) as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError('diagnostic source is not a regular file')
        blocks = []
        length = 0
        # Read a bounded margin before the requested tail. When a long wrap
        # consumes the margin, edge-fragment masking protects the open edge.
        for block in backward(stream):
            blocks.append(block)
            length += len(block)
            if length >= WINDOW_BYTES:
                break
        return redacted_tail(b''.join(reversed(blocks)))


def emit(stream, name, data):
    assert name in INSTALLER_LOGS
    if data:
        data = redacted_tail(data)
        stream.write(PREFIX + name.encode() + b' ' + base64.b64encode(data) + b'\n')


def reverse_lines(stream):
    pending = b''
    oversized = False
    complete = False
    for block in backward(stream):
        lines = block.split(b'\n')
        for index in range(len(lines) - 1, 0, -1):
            line = lines[index] + pending
            if oversized or len(line) > FRAME_BYTES:
                yield None
            elif line or complete:
                yield line + (b'\n' if complete else b'')
            pending = b''
            oversized = False
            complete = True
        if not oversized:
            pending = lines[0] + pending
            oversized = len(pending) > FRAME_BYTES
            if oversized:
                pending = b''
    if oversized:
        yield None
    elif pending or complete:
        yield pending + (b'\n' if complete else b'')


def extract(serial, output):
    # Work backward and retain at most one complete record per known name.
    # Discard overlong lines and frame data with an unknown name or bad base64.
    records = {}
    console = []
    length = 0
    with open_capture(serial) as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError('serial source is not a regular file')
        for line in reverse_lines(stream):
            if line is None:
                # No trustworthy context on the far side of an oversized line.
                length = WINDOW_BYTES
                continue
            _, marker, frame = line.partition(PREFIX)
            if marker:
                if frame.endswith(b'\n'):
                    name, _, payload = frame.rstrip(b'\r\n').partition(b' ')
                    name = name.decode('ascii', 'replace')
                    if name in INSTALLER_LOGS and name not in records:
                        try:
                            records[name] = redacted_tail(base64.b64decode(payload, validate=True))
                        except ValueError:
                            pass
            elif length < WINDOW_BYTES:
                console.append(line)
                length += len(line)
            if length >= WINDOW_BYTES and len(records) == len(INSTALLER_LOGS):
                break
    for name, data in records.items():
        (output / ('installer-' + name)).write_bytes(data)
    return redacted_tail(b''.join(reversed(console)))


if __name__ == '__main__':
    output = pathlib.Path(sys.argv[1])
    output.mkdir(exist_ok=True)
    for argument in sys.argv[2:]:
        path = pathlib.Path(argument)
        if path.is_file():
            if path.name.endswith('-serial.log'):
                data = extract(path, output) if path.name == 'installer-serial.log' else tail(path)
                name = path.stem + '.redacted.log'
            else:
                data = tail(path)
                name = path.name
            (output / name).write_bytes(data)
