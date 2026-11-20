"""Normalize terminal captures while retaining original byte offsets."""
from bisect import bisect_right
import re

PASSPHRASE = b'abcdefghijklmnopqrstuvwxyz'
RECOVERY = b'cbdefghijklnrtuv'
CREDENTIALS = (
    (PASSPHRASE,) * 48,
    ((RECOVERY,) * 8 + (b'-',)) * 7 + (RECOVERY,) * 8,
)
CONTEXT = max(map(len, CREDENTIALS)) - 1
FULL = re.compile(rb'[a-z]{48,}')
RECOVERY_FULL = re.compile(rb'(?i:(?=([cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7})))')
CONTROL = re.compile(rb'[\r\n\x1b\0]')
NOT_INDENT = re.compile(rb'[^\r\n \t\0]')
NOT_PARAMETER = re.compile(rb'[^ -?]')
NOT_INTERMEDIATE = re.compile(rb'[^ -/]')
STRING_END = re.compile(rb'[\x07\x1b]')
CSI_SUFFIX = re.compile(rb'\[?[0-?]*[ -/]*[@-~]')
STRING_SUFFIX = re.compile(rb'\x07|\x1b\\')


class Normalizer:
    """Carry wrapping and ANSI parser state across arbitrary reads."""

    def __init__(self, offset=0):
        self.offset = offset
        self.mode = 'wrap'
        self.escape = None
        self.after_escape = 'text'

    def feed(self, data, escapes=None):
        parts, offsets, starts = [], [], []
        length = 0
        index = 0
        while index < len(data):
            if self.mode == 'text':
                control = CONTROL.search(data, index)
                end = control.start() if control else len(data)
                if end > index:
                    starts.append(length)
                    offsets.append(self.offset + index)
                    parts.append(data[index:end])
                    length += end - index
                index = end
                if control:
                    if data[index] == 27:
                        self.escape = self.offset + index
                        self.mode = 'escape'
                        self.after_escape = 'text'
                    elif data[index] != 0:
                        self.mode = 'wrap'
                    index += 1
            elif self.mode == 'wrap':
                ordinary = NOT_INDENT.search(data, index)
                index = ordinary.start() if ordinary else len(data)
                if ordinary:
                    if data[index] == 27:
                        self.escape = self.offset + index
                        self.after_escape = 'wrap'
                        self.mode = 'escape'
                        index += 1
                    else:
                        self.mode = 'text'
            elif self.mode == 'escape':
                byte = data[index]
                if byte == ord('['):
                    self.mode = 'csi'
                elif byte in b']P^_X':
                    self.mode = 'string'
                elif 32 <= byte <= 47:
                    self.mode = 'intermediate'
                else:
                    self.mode = 'text'
                if 32 <= byte <= 126:
                    index += 1
            elif self.mode in ('csi', 'intermediate'):
                pattern = NOT_PARAMETER if self.mode == 'csi' else NOT_INTERMEDIATE
                end = pattern.search(data, index)
                index = end.start() if end else len(data)
                if end:
                    if 48 <= data[index] <= 126:
                        index += 1
                    self.mode = 'text'
            elif self.mode == 'string':
                end = STRING_END.search(data, index)
                index = end.start() if end else len(data)
                if end:
                    self.mode = 'string-escape' if data[index] == 27 else 'text'
                    index += 1
            else:  # ESC in a control string, possibly the string terminator.
                if data[index] == ord('\\'):
                    self.mode = 'text'
                    index += 1
                else:
                    self.mode = 'string'
            if self.escape is not None and self.mode == 'text':
                if escapes is not None:
                    escapes.append((self.escape, self.offset + index))
                self.escape = None
                self.mode = self.after_escape
        self.offset += len(data)
        return b''.join(parts), (starts, offsets)


def original_offset(mapping, index):
    starts, offsets = mapping
    run = bisect_right(starts, index) - 1
    return offsets[run] + index - starts[run]


def edge_length(data, left):
    """Longest fragment consistent with a credential crossing this edge."""
    limit = min(len(data), CONTEXT)
    edge = data[:limit] if left else data[-limit:]
    edge = edge.lower()
    longest = 0
    for alphabet in CREDENTIALS:
        for length in range(min(limit, len(alphabet) - 1), longest, -1):
            fragment = edge[:length] if left else edge[-length:]
            allowed = alphabet[-length:] if left else alphabet[:length]
            if all(byte in choices for byte, choices in zip(fragment, allowed)):
                longest = length
                break
    return longest


def redact(data):
    normalizer = Normalizer()
    escapes = []
    normalized, mapping = normalizer.feed(data, escapes)
    if normalizer.escape is not None:
        escapes.append((normalizer.escape, len(data)))
    spans = [m.span() for m in FULL.finditer(normalized)]
    spans.extend(m.span(1) for m in RECOVERY_FULL.finditer(normalized))
    if count := edge_length(normalized, left=True):
        spans.append((0, count))
    # A snapshot can start in the middle of an escape. Try a leading CSI
    # suffix as well as the first orphaned control-string terminator.
    csi = CSI_SUFFIX.match(normalized)
    string = None
    for match in STRING_SUFFIX.finditer(data):
        previous = bisect_right(escapes, (match.start() - 1, len(data))) - 1
        if previous < 0 or escapes[previous][1] <= match.start():
            string = match
            break
    if csi or normalized.startswith(b'\\'):
        start = csi.end() if csi else 1
        edge = normalized[start:].lstrip(b' \t')
        if count := edge_length(edge, left=True):
            start = len(normalized) - len(edge)
            spans.append((start, start + count))
    if string:
        remainder = Normalizer(string.end()).feed(data[string.end():])
        if count := edge_length(remainder[0], left=True):
            # Use the second mapping because the split starts in the middle
            # of the original capture and may be followed by wrap indentation.
            end = original_offset(remainder[1], count - 1) + 1
            escapes.append((0, end))
            escapes.sort()
    if count := edge_length(normalized, left=False):
        spans.append((len(normalized) - count, len(normalized)))
    # Mask before slicing any output. Preserve line breaks in the original
    # evidence and strip terminal control strings, including their payloads.
    masked = bytearray(data)
    for start, end in spans:
        first = original_offset(mapping, start)
        last = original_offset(mapping, end - 1) + 1
        masked[first:last] = re.sub(rb'[^\r\n]', b'*', masked[first:last])
    for first, last in escapes:
        masked[first:last] = b'\0' * (last - first)
    return bytes(masked).replace(b'\0', b'')
