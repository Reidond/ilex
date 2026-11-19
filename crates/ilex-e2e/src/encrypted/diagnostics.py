"""Bounded diagnostic text for the disposable encrypted install fixture."""
import base64
import os
import pathlib
import re
import stat
import sys

LOG_BYTES = 32 * 1024
PREFIX = b'ILEX-ENCRYPTED-LOG '
INSTALLER_LOGS = (
    'anaconda.log', 'program.log', 'storage.log', 'packaging.log',
    'post.log', 'preload.log', 'driver.log', 'luksDump.txt',
    'leak-scan.json', 'finaliser-leak-scan.json',
)


def redact(data):
    # Redact the generated credential formats even if completion failed
    # before the recovery fingerprint reached the host.
    data = re.sub(rb'[a-z]{48,}', b'[redacted lowercase token]', data)
    data = re.sub(rb'(?i)[cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7}',
                  b'[redacted recovery key]', data)
    return data.decode('utf-8', 'replace').encode()


def tail(path):
    try:
        with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode):
                return b''
            if info.st_size > LOG_BYTES:
                stream.seek(-LOG_BYTES, os.SEEK_END)
                stream.readline(LOG_BYTES)  # Skip a possibly partial credential.
            return stream.read(LOG_BYTES)
    except OSError:
        return b''


def emit(stream, name, data):
    assert name in INSTALLER_LOGS
    if data:
        data = redact(data)[-LOG_BYTES:]
        stream.write(PREFIX + name.encode() + b' ' + base64.b64encode(data) + b'\n')


def extract(serial, output):
    # Keep the latest complete record per file. A forced VM stop can truncate
    # the final write. Names from the guest cannot select paths.
    records = {}
    console = bytearray()
    if serial.is_file():
        with serial.open('rb') as source:
            for line in source:
                if not line.startswith(PREFIX):
                    console.extend(line)
                    del console[:-LOG_BYTES]
                    continue
                if not line.endswith(b'\n'):
                    continue
                name, _, payload = line[len(PREFIX):].rstrip().partition(b' ')
                if name.decode('ascii', 'replace') not in INSTALLER_LOGS or len(payload) > 192 * 1024:
                    continue
                try:
                    records[name.decode()] = redact(base64.b64decode(payload, validate=True))
                except ValueError:
                    continue
    for name, data in records.items():
        (output / ('installer-' + name)).write_bytes(data)
    (output / serial.name).write_bytes(redact(console))


if __name__ == '__main__':
    output = pathlib.Path(sys.argv[1])
    output.mkdir(exist_ok=True)
    for argument in sys.argv[2:]:
        path = pathlib.Path(argument)
        if path.is_file():
            if path.name == 'installer-serial.log':
                extract(path, output)
            else:
                (output / path.name).write_bytes(redact(tail(path)))
