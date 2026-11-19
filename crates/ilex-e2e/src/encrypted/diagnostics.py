"""Bounded diagnostic text for the disposable encrypted install fixture."""
import base64
import json
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
WRAP = rb'(?:(?:(?>\r\n?|\n)[ \t]*|\x1b\[[0-?]*[ -/]*[@-~]))*'
PASSPHRASE = re.compile(rb'[a-z](?:' + WRAP + rb'[a-z]){47,}')
RECOVERY_GROUP = rb'[cbdefghijklnrtuv](?:' + WRAP + rb'[cbdefghijklnrtuv]){7}'
RECOVERY = re.compile(RECOVERY_GROUP + (rb'(?:' + WRAP + rb'-' + WRAP + RECOVERY_GROUP + rb'){7}'),
                      re.IGNORECASE)
PARTIAL = re.compile(rb'[A-Za-z-](?:' + WRAP + rb'[A-Za-z-]){0,70}' + WRAP + rb'[ \t]*\Z')


def redact(data):
    # Redact the generated credential formats even if completion failed
    # before the recovery fingerprint reached the host.
    data = RECOVERY.sub(b'[redacted recovery key]', data)
    data = PASSPHRASE.sub(b'[redacted lowercase token]', data)
    return data.decode('utf-8', 'replace').encode()


def redacted_tail(data, drop_incomplete=False):
    complete = data.endswith(b'\n')
    data = redact(data)
    if drop_incomplete and not complete:
        end = data.rfind(b'\n')
        if end < 0:
            return b''
        # The unfinished line may continue a partial token from its predecessor.
        data = PARTIAL.sub(b'[redacted incomplete token]', data[:end]) + b'\n'
    return data[-LOG_BYTES:]


def tail(path):
    try:
        with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), 'rb') as stream:
            info = os.fstat(stream.fileno())
            if not stat.S_ISREG(info.st_mode):
                return b''
            data = stream.read(info.st_size)
            # Fixture JSON reports are written as one value without a newline.
            # Keep complete values, and handle truncated JSON as unfinished text.
            if path.suffix == '.json':
                try:
                    json.loads(data)
                    return redacted_tail(data)
                except ValueError:
                    pass
            return redacted_tail(data, drop_incomplete=True)
    except OSError:
        return b''


def emit(stream, name, data):
    assert name in INSTALLER_LOGS
    if data:
        data = redacted_tail(data)
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
                    continue
                if not line.endswith(b'\n'):
                    continue
                name, _, payload = line[len(PREFIX):].rstrip().partition(b' ')
                if name.decode('ascii', 'replace') not in INSTALLER_LOGS or len(payload) > 192 * 1024:
                    continue
                try:
                    records[name.decode()] = redacted_tail(base64.b64decode(payload, validate=True))
                except ValueError:
                    continue
    for name, data in records.items():
        (output / ('installer-' + name)).write_bytes(data)
    (output / serial.name).write_bytes(redacted_tail(console, drop_incomplete=True))


if __name__ == '__main__':
    output = pathlib.Path(sys.argv[1])
    output.mkdir(exist_ok=True)
    for argument in sys.argv[2:]:
        path = pathlib.Path(argument)
        if path.is_file():
            if path.name == 'installer-serial.log':
                extract(path, output)
            else:
                (output / path.name).write_bytes(tail(path))
