#!/usr/bin/env python3
"""Exercise the vfio-user framing, bounds and provider relay without QEMU."""

import pathlib
import socket
import struct
import sys
import threading
import unittest

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "tools"))
import vfio_user_cxl_backend as backend


class BackendProtocolTest(unittest.TestCase):
    def test_reject_without_capability(self):
        client, server = socket.socketpair()
        relay, provider = socket.socketpair(type=socket.SOCK_SEQPACKET)
        client.settimeout(2)
        server.settimeout(2)
        failure = []

        def run_server():
            try:
                backend.serve(server, relay, 256 << 20)
            except backend.ProtocolError as exc:
                failure.append(str(exc))

        thread = threading.Thread(target=run_server)
        thread.start()
        try:
            body = backend.VERSION.pack(0, 0) + b'{"capabilities":{}}\0'
            client.sendall(backend.HEADER.pack(1, 1, backend.HEADER.size + len(body), 0, 0) + body)
            thread.join(2)
            self.assertFalse(thread.is_alive())
            self.assertIn("missing zettbridge_cxl", failure[0])
        finally:
            client.close()
            server.close()
            relay.close()
            provider.close()

    def test_region_and_cxl_operations(self):
        client, server = socket.socketpair()
        relay, provider = socket.socketpair(type=socket.SOCK_SEQPACKET)
        for sock in (client, server, relay, provider):
            sock.settimeout(2)
        seen = []

        def provider_worker():
            try:
                while request := provider.recv(backend.PROVIDER.size):
                    magic, op, count, _, addr, value, data = backend.PROVIDER.unpack(request)
                    seen.append((op, count, addr, value, data[:count]))
                    answer = b"ABCD" if op == 2 else b""
                    provider.send(backend.PROVIDER.pack(
                        magic, op, count, 0, addr, 0x12345678 if op == 0 else 0,
                        answer.ljust(64, b"\0")))
            except OSError:
                pass

        worker = threading.Thread(target=provider_worker)
        server_thread = threading.Thread(target=backend.serve, args=(server, relay, 256 << 20))
        worker.start()
        server_thread.start()

        def exchange(msg_id, command, payload):
            client.sendall(backend.HEADER.pack(msg_id, command,
                                               backend.HEADER.size + len(payload), 0, 0) + payload)
            response = backend.receive_exact(client, backend.HEADER.size)
            rid, rcmd, size, flags, error = backend.HEADER.unpack(response)
            self.assertEqual((rid, rcmd), (msg_id, command))
            return flags, error, backend.receive_exact(client, size - backend.HEADER.size)

        try:
            self.assertEqual(exchange(1, 1, backend.VERSION.pack(0, 0) + backend.CAPABILITIES),
                             (1, 0, backend.VERSION.pack(0, 0) + backend.CAPABILITIES))
            flags, error, info = exchange(2, 4, backend.DEVICE_INFO.pack(16, 0, 0, 0))
            self.assertEqual((flags, error, backend.DEVICE_INFO.unpack(info)), (1, 0, (16, 2, 9, 0)))
            flags, error, info = exchange(3, 5, backend.REGION_INFO.pack(32, 0, 0, 0, 0, 0))
            self.assertEqual((flags, error, backend.REGION_INFO.unpack(info)[:3]), (1, 0, (32, 3, 0)))
            flags, error, data = exchange(4, 9, backend.REGION.pack(0, 0, 4))
            self.assertEqual((flags, error, data),
                             (1, 0, backend.REGION.pack(0, 0, 4) + struct.pack("<I", 0x12345678)))
            flags, error, data = exchange(5, 10, backend.REGION.pack(4, 0, 4) + b"WXYZ")
            self.assertEqual((flags, error, data), (1, 0, backend.REGION.pack(4, 0, 4)))
            flags, error, data = exchange(6, 0x8001, backend.REGION.pack(8, 0, 4))
            self.assertEqual((flags, error, data), (1, 0, backend.REGION.pack(8, 0, 4) + b"ABCD"))
            flags, error, data = exchange(7, 0x8002, backend.REGION.pack(12, 0, 4) + b"data")
            self.assertEqual((flags, error, data), (1, 0, backend.REGION.pack(12, 0, 4)))
            flags, error, data = exchange(8, 0x8001, backend.REGION.pack(256 << 20, 0, 4))
            self.assertEqual((flags, error, data), (0x21, 22, b""))
            self.assertEqual([item[0] for item in seen], [0, 1, 2, 3])
        finally:
            client.close()
            server_thread.join(2)
            server.close()
            relay.close()
            provider.close()
            worker.join(2)
            self.assertFalse(server_thread.is_alive())
            self.assertFalse(worker.is_alive())


if __name__ == "__main__":
    unittest.main()
