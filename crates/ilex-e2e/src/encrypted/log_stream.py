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
HOMED_FULL = re.compile(rb'Ix9-[0-9a-f]{48}')
RECOVERY_FULL = re.compile(rb'(?i:(?=([cbdefghijklnrtuv]{8}(?:-[cbdefghijklnrtuv]{8}){7})))')
ESC = 0x1B
CSI, ST, OSC = 0x9B, 0x9C, 0x9D
STRINGS = frozenset((0x90, 0x98, OSC, 0x9E, 0x9F))  # DCS, SOS, OSC, PM, APC
CANCEL = frozenset((0x18, 0x1A))  # CAN, SUB
SPECIAL = re.compile(rb'[\x00-\x1f\x7f-\x9f\xc2-\xf4]')
UTF8 = re.compile(rb'[\xc2-\xdf][\x80-\xbf]|[\xe0-\xef][\x80-\xbf]{2}|[\xf0-\xf4][\x80-\xbf]{3}')
CSI_SUFFIX = re.compile(rb'\[?[0-?]*[ -/]*[@-~]')
ESC_SUFFIX = re.compile(rb'[ -/]*[0-~]')
RESYNC = re.compile(UTF8.pattern + rb'|\x1b[\x00-\x17\x19\x1c-\x1f]*\\|[\x07\x18\x1a\x9c]')


class Normalizer:
    """Carry wrapping and ANSI parser state across arbitrary reads."""

    def __init__(self, offset=0):
        self.offset = offset
        self.mode = 'text'
        self.indent = True
        self.escape = None
        self.bell_ends_string = False
        self.pending = b''

    def feed(self, data, escapes=None, final=False):
        data = self.pending + data
        self.pending = b''
        parts, offsets, starts = [], [], []
        length = 0
        index = 0

        def keep(first, last):
            nonlocal length
            if self.indent:
                while first < last and data[first] == 32:
                    first += 1
                if first == last:
                    return
                self.indent = False
            starts.append(length)
            offsets.append(self.offset + first)
            parts.append(data[first:last])
            length += last - first

        def cut(first, last):
            if escapes is None:
                return
            if escapes and escapes[-1][1] == first:
                first = escapes.pop()[0]
            escapes.append((first, last))

        def begin(kind, start):
            self.escape = start if self.escape is None else self.escape
            if kind == ESC:
                self.mode = 'escape'
            elif kind == CSI:
                self.mode = 'parameters'
            else:
                self.mode = 'string'
                self.bell_ends_string = kind == OSC

        def end(stop):
            if self.escape is not None:
                cut(self.escape, stop)
            self.escape = None
            self.mode = 'text'

        while index < len(data):
            if self.mode == 'text':
                control = SPECIAL.search(data, index)
                stop = control.start() if control else len(data)
                if stop > index:
                    keep(index, stop)
                index = stop
                if not control:
                    break
            first = index
            byte = data[index]
            index += 1
            if 0xC2 <= byte <= 0xF4:
                width = 2 if byte <= 0xDF else 3 if byte <= 0xEF else 4
                glyph = UTF8.match(data, first)
                if glyph:
                    index = glyph.end()
                    if byte == 0xC2 and 0x80 <= data[first + 1] <= 0x9F:
                        byte = data[first + 1]
                    else:
                        byte = 0xFF  # International text is a graphic, not a raw C1 byte.
                elif len(data) - first < width and all(
                        0x80 <= part <= 0xBF for part in data[index:]):
                    if final:
                        cut(self.escape if self.escape is not None else self.offset + first,
                            self.offset + len(data))
                        self.escape = None
                        index = len(data)
                    else:
                        self.pending = data[first:]
                        index = first
                    break
            start, stop = self.offset + first, self.offset + index
            if self.mode == 'text':
                if byte == ESC or byte == CSI or byte in STRINGS:
                    begin(byte, start)
                elif byte in (10, 13):
                    self.indent = True
                elif byte != 9 and (byte < 32 or 0x7F <= byte <= 0x9F):
                    cut(start, stop)
                elif byte != 9:
                    keep(first, index)
            elif self.mode in ('string', 'string-escape'):
                if (byte == ST or byte in CANCEL or byte == 7 and self.bell_ends_string
                        or self.mode == 'string-escape' and byte == ord('\\')):
                    end(stop)
                elif byte == ESC:
                    self.mode = 'string-escape'
                elif byte >= 32 and not 0x7F <= byte <= 0x9F:
                    if self.mode == 'string-escape':
                        self.mode = 'escape'
                        index = first
                # Padding (including NUL between ESC and backslash) preserves state.
            elif byte in CANCEL or byte == ST:
                end(stop)
            elif byte == ESC or byte == CSI or byte in STRINGS:
                begin(byte, start)
            elif byte < 32 or 0x7F <= byte <= 0x9F:
                continue  # Execute/ignore standalone controls without ending a sequence.
            elif self.mode == 'escape' and 0x40 <= byte <= 0x5F and byte + 0x40 in (*STRINGS, CSI):
                begin(byte + 0x40, start)
            elif 0x20 <= byte <= 0x2F:
                self.mode = 'esc-intermediate' if self.mode.startswith('esc') else 'intermediate'
            elif self.mode == 'parameters' and 0x30 <= byte <= 0x3F:
                continue
            elif (0x30 if self.mode.startswith('esc') else 0x40) <= byte <= 0x7E:
                end(stop)
            else:
                end(start)
                keep(first, index)
        self.offset += index
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
            if any(all(byte in choices for byte, choices in zip(fragment, allowed))
                   for allowed in (alphabet[:length], alphabet[-length:])):
                longest = length
                break
    return longest


def leading_end(data):
    """Protect the entire leading region if it may be a clipped string."""
    start = 0
    for match in RESYNC.finditer(data):
        control = match.group()
        bell = control == b'\x07'
        universal = (control in (b'\x18', b'\x1a', b'\x9c', b'\xc2\x9c')
                     or control.startswith(b'\x1b'))
        if not bell and not universal:
            continue  # A complete UTF-8 graphic can contain a C1 byte.
        start = match.end()
        # BEL can end OSC, but the same byte inside DCS is payload. Keep
        # looking before choosing how much of the unknown leading region to
        # suppress. ST and CAN/SUB resynchronize all control-string states.
        clipped_utf8 = (control == b'\x9c' and match.start() < 3
                        and all(0x80 <= part <= 0xBF for part in data[:match.start()]))
        if universal and not clipped_utf8:
            break
    if start:
        normalized, mapping = Normalizer(start).feed(data[start:], final=True)
        if count := edge_length(normalized, left=True):
            return original_offset(mapping, count - 1) + 1
    return start


def redact(data):
    normalizer = Normalizer()
    escapes = []
    normalized, mapping = normalizer.feed(data, escapes, final=True)
    if normalizer.escape is not None:
        escapes.append((normalizer.escape, len(data)))
    spans = [m.span() for m in FULL.finditer(normalized)]
    spans.extend(m.span(1) for m in RECOVERY_FULL.finditer(normalized))
    spans.extend(m.span() for m in HOMED_FULL.finditer(normalized))
    if count := edge_length(normalized, left=True):
        spans.append((0, count))
    # A snapshot can start inside an escape or inside a control string.
    for suffix in (CSI_SUFFIX, ESC_SUFFIX):
        fragment = suffix.match(normalized)
        if not fragment:
            continue
        start = fragment.end()
        edge = normalized[start:].lstrip(b' \t')
        if count := edge_length(edge, left=True):
            start = len(normalized) - len(edge)
            spans.append((start, start + count))
    if end := leading_end(data):
        escapes.append((0, end))
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
