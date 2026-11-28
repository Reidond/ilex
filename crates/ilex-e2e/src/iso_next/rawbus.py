# A minimal D-Bus client for what busctl cannot do: take the file descriptor a
# method returns (TakeRecoveryKey). Standard library only; one connection to
# the system bus as the calling user (EXTERNAL authentication), method calls
# with string arguments, and replies that are strings, nothing, or a file
# descriptor. The trials use it as the installer's interface does.
import array
import os
import socket
import struct

BUS_SOCKET = "/run/dbus/system_bus_socket"


def _align(buffer, boundary, offset=0):
    while (offset + len(buffer)) % boundary:
        buffer.append(0)


class Bus:
    def __init__(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(BUS_SOCKET)
        self.sock.sendall(b"\0AUTH EXTERNAL " + str(os.getuid()).encode().hex().encode() + b"\r\n")
        if not self._line().startswith(b"OK"):
            raise RuntimeError("the bus refused the authentication")
        self.sock.sendall(b"NEGOTIATE_UNIX_FD\r\n")
        if not self._line().startswith(b"AGREE_UNIX_FD"):
            raise RuntimeError("the bus does not pass file descriptors")
        self.sock.sendall(b"BEGIN\r\n")
        self.serial = 0
        self.fds = []
        self.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "Hello")

    def _line(self):
        data = b""
        while not data.endswith(b"\r\n"):
            data += self.sock.recv(1)
        return data

    def _read(self, count):
        data = b""
        while len(data) < count:
            chunk, fds, _flags, _addr = socket.recv_fds(self.sock, count - len(data), 8)
            if not chunk:
                raise RuntimeError("the bus closed the connection")
            data += chunk
            self.fds.extend(fds)
        return data

    @staticmethod
    def _string(value):
        return struct.pack("<I", len(value.encode())) + value.encode() + b"\0"

    def call(self, destination, path, interface, member, *args):
        self.serial += 1
        fields = bytearray()
        # The array of header fields starts at byte 16 of the message.
        def field(code, signature, value):
            _align(fields, 8, 16)
            fields.append(code)
            fields.extend(bytes([len(signature)]) + signature.encode() + b"\0")
            if signature in ("s", "o"):
                _align(fields, 4, 16)
                fields.extend(self._string(value))
            else:
                fields.extend(bytes([len(value)]) + value.encode() + b"\0")
        field(1, "o", path)
        field(2, "s", interface)
        field(3, "s", member)
        field(6, "s", destination)
        if args:
            field(8, "g", "s" * len(args))
        body = bytearray()
        for arg in args:
            _align(body, 4)
            body.extend(self._string(arg))
        message = bytearray(b"l" + bytes([1, 0, 1]) + struct.pack("<II", len(body), self.serial))
        message.extend(struct.pack("<I", len(fields)))
        message.extend(fields)
        _align(message, 8)
        message.extend(body)
        self.sock.sendall(bytes(message))
        while True:
            reply = self._message()
            if reply["type"] in (2, 3) and reply["reply_serial"] == self.serial:
                if reply["type"] == 3:
                    raise RuntimeError(f"{reply['error']}: {reply['text']}")
                return reply

    def _message(self):
        head = self._read(16)
        if head[0:1] != b"l":
            raise RuntimeError("a big-endian message")
        kind = head[1]
        body_length, _serial, fields_length = struct.unpack("<III", head[4:16])
        fields_padded = (fields_length + 7) // 8 * 8
        rest = self._read(fields_padded + body_length)
        fields, body = rest[:fields_length], rest[fields_padded:]
        found = {}
        at = 0
        while at < fields_length:
            at = (at + 7) // 8 * 8
            if at >= fields_length:
                break
            code = fields[at]
            sig_length = fields[at + 1]
            signature = fields[at + 2:at + 2 + sig_length].decode()
            at += 3 + sig_length
            if signature in ("s", "o"):
                at = (at + 3) // 4 * 4
                (length,) = struct.unpack("<I", fields[at:at + 4])
                found[code] = fields[at + 4:at + 4 + length].decode()
                at += 5 + length
            elif signature == "g":
                length = fields[at]
                found[code] = fields[at + 1:at + 1 + length].decode()
                at += 2 + length
            elif signature == "u":
                at = (at + 3) // 4 * 4
                (found[code],) = struct.unpack("<I", fields[at:at + 4])
                at += 4
            else:
                raise RuntimeError(f"a header field of type {signature}")
        reply = {"type": kind, "reply_serial": found.get(5), "signature": found.get(8, ""),
                 "error": found.get(4), "text": "", "value": None}
        if kind == 3 and reply["signature"].startswith("s"):
            (length,) = struct.unpack("<I", body[:4])
            reply["text"] = body[4:4 + length].decode(errors="replace")
        elif kind == 2 and reply["signature"] == "s":
            (length,) = struct.unpack("<I", body[:4])
            reply["value"] = body[4:4 + length].decode()
        elif kind == 2 and reply["signature"] == "h":
            (index,) = struct.unpack("<I", body[:4])
            reply["value"] = self.fds[index]
            self.fds = []
        return reply
