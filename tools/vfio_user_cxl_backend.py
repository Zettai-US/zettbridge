#!/usr/bin/env python3
"""Zettbridge vfio-user CXL transport backend for the two-QEMU functional model.

BAR0 uses standard vfio-user region messages. Private CXL.mem commands are
accepted only after a version handshake advertising zettbridge_cxl version 1.
The provider QEMU remains the PCI requester and ATS owner; this process relays
bounded operations to its existing local SEQPACKET endpoint.
"""

import argparse
import errno
import os
import pathlib
import socket
import struct
import sys

HEADER = struct.Struct("<HHIII")
VERSION = struct.Struct("<HH")
REGION = struct.Struct("<QII")
DEVICE_INFO = struct.Struct("<IIII")
REGION_INFO = struct.Struct("<IIIIQQ")
PROVIDER = struct.Struct("<IIIIQQ64s")
CAPABILITIES = b'{"capabilities":{"zettbridge_cxl":1}}\0'

VFIO_USER_VERSION = 1
VFIO_USER_DEVICE_GET_INFO = 4
VFIO_USER_DEVICE_GET_REGION_INFO = 5
VFIO_USER_REGION_READ = 9
VFIO_USER_REGION_WRITE = 10
ZB_VFIO_CXL_READ = 0x8001
ZB_VFIO_CXL_WRITE = 0x8002
VFIO_USER_REPLY = 1
VFIO_USER_ERROR = 0x20
ZB_MODEL_V1 = 0x5A424D01
ZB_BAR_SIZE = 0x4000
ZB_OK = 0
ZB_DENIED = 8


class ProtocolError(Exception):
    pass


def receive_exact(sock, length):
    data = bytearray()
    while len(data) < length:
        part = sock.recv(length - len(data))
        if not part:
            raise EOFError("vfio-user peer closed")
        data.extend(part)
    return bytes(data)


def send_reply(sock, msg_id, command, payload=b"", error=0):
    flags = VFIO_USER_REPLY | (VFIO_USER_ERROR if error else 0)
    sock.sendall(HEADER.pack(msg_id, command, HEADER.size + len(payload), flags, error) + payload)


def provider_request(sock, op, count, addr, value=0, data=b""):
    if len(data) > 64:
        raise ProtocolError("oversized provider request")
    request = PROVIDER.pack(ZB_MODEL_V1, op, count, 0, addr, value, data.ljust(64, b"\0"))
    if sock.send(request) != len(request):
        raise EOFError("provider request was not sent atomically")
    reply = sock.recv(PROVIDER.size)
    if len(reply) != PROVIDER.size:
        raise EOFError("provider closed or truncated reply")
    magic, reply_op, reply_count, status, reply_addr, reply_value, reply_data = PROVIDER.unpack(reply)
    if (magic, reply_op, reply_count, reply_addr) != (ZB_MODEL_V1, op, count, addr):
        raise ProtocolError("provider reply does not match request")
    if status != ZB_OK:
        return (errno.EACCES if status == ZB_DENIED else errno.EIO), b""
    if op == 0:
        return 0, struct.pack("<I", reply_value)
    if op == 2:
        return 0, reply_data[:count]
    return 0, b""


def device_config():
    config = bytearray(256)
    struct.pack_into("<HH", config, 0, 0x1B36, 0x10F1)
    config[8:12] = bytes((2, 0, 0x80, 0x05))
    return bytes(config)


def serve(client, provider, capacity):
    negotiated = False
    config = device_config()
    while True:
        try:
            header = receive_exact(client, HEADER.size)
        except EOFError:
            return
        msg_id, command, size, flags, error = HEADER.unpack(header)
        if size < HEADER.size or size > 4096 or flags != 0 or error:
            raise ProtocolError("malformed vfio-user header")
        body = receive_exact(client, size - HEADER.size)
        if not negotiated:
            if (command != VFIO_USER_VERSION or
                    body != VERSION.pack(0, 0) + CAPABILITIES):
                raise ProtocolError("missing zettbridge_cxl version 1 capability")
            send_reply(client, msg_id, command, body)
            negotiated = True
            continue

        if command == VFIO_USER_DEVICE_GET_INFO:
            if len(body) != DEVICE_INFO.size:
                send_reply(client, msg_id, command, error=errno.EINVAL)
                continue
            send_reply(client, msg_id, command,
                       DEVICE_INFO.pack(DEVICE_INFO.size, 2, 9, 0))
            continue
        if command == VFIO_USER_DEVICE_GET_REGION_INFO:
            if len(body) != REGION_INFO.size:
                send_reply(client, msg_id, command, error=errno.EINVAL)
                continue
            argsz, _, index, _, _, _ = REGION_INFO.unpack(body)
            if argsz < REGION_INFO.size or index >= 9:
                send_reply(client, msg_id, command, error=errno.EINVAL)
                continue
            flags = 3 if index == 0 else 1 if index == 7 else 0
            region_size = ZB_BAR_SIZE if index == 0 else len(config) if index == 7 else 0
            send_reply(client, msg_id, command,
                       REGION_INFO.pack(REGION_INFO.size, flags, index, 0, region_size, 0))
            continue
        if command not in (VFIO_USER_REGION_READ, VFIO_USER_REGION_WRITE,
                           ZB_VFIO_CXL_READ, ZB_VFIO_CXL_WRITE) or len(body) < REGION.size:
            send_reply(client, msg_id, command, error=errno.ENOSYS)
            continue

        offset, region, count = REGION.unpack_from(body)
        write = command in (VFIO_USER_REGION_WRITE, ZB_VFIO_CXL_WRITE)
        payload = body[REGION.size:]
        if len(payload) != (count if write else 0):
            send_reply(client, msg_id, command, error=errno.EINVAL)
            continue
        if command in (VFIO_USER_REGION_READ, VFIO_USER_REGION_WRITE):
            if region == 7 and not write and count and offset <= len(config) - count:
                send_reply(client, msg_id, command,
                           REGION.pack(offset, region, count) + config[offset:offset + count])
                continue
            if region != 0 or count != 4 or offset & 3 or offset >= ZB_BAR_SIZE:
                send_reply(client, msg_id, command, error=errno.EINVAL)
                continue
            op = 1 if write else 0
            value = struct.unpack("<I", payload)[0] if write else 0
            error_code, data = provider_request(provider, op, count, offset, value)
        else:
            if region or not 0 < count <= 64 or offset >= capacity or count > capacity - offset:
                send_reply(client, msg_id, command, error=errno.EINVAL)
                continue
            op = 3 if write else 2
            error_code, data = provider_request(provider, op, count, offset, data=payload)
        if error_code:
            send_reply(client, msg_id, command, error=error_code)
        else:
            send_reply(client, msg_id, command, REGION.pack(offset, region, count) + data)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket-path", type=pathlib.Path, required=True)
    parser.add_argument("--provider-socket", type=pathlib.Path, required=True)
    parser.add_argument("--capacity", type=int, required=True)
    parser.add_argument("--timeout-ms", type=int, default=2000)
    args = parser.parse_args()
    if sys.byteorder != "little":
        parser.error("this vfio-user CXL profile requires a little-endian host")
    if args.capacity < 256 << 20 or args.capacity > 64 << 30 or args.capacity & (args.capacity - 1):
        parser.error("capacity must be a power of two from 256M through 64G")
    if not 1 <= args.timeout_ms <= 30000:
        parser.error("timeout must be 1..30000 ms")
    if args.socket_path.exists():
        parser.error("vfio-user socket path already exists")
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as listener:
        listener.bind(str(args.socket_path))
        os.chmod(args.socket_path, 0o600)
        inode = args.socket_path.stat().st_ino
        listener.listen(1)
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET) as provider:
                provider.settimeout(args.timeout_ms / 1000)
                provider.connect(str(args.provider_socket))
                with listener.accept()[0] as client:
                    client.settimeout(args.timeout_ms / 1000)
                    serve(client, provider, args.capacity)
        finally:
            if args.socket_path.exists() and args.socket_path.stat().st_ino == inode:
                args.socket_path.unlink()


if __name__ == "__main__":
    main()
