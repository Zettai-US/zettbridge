#!/usr/bin/env python3
"""Connect to a live guest's local serial console (Ctrl-] disconnects)."""
import argparse
import os
import pathlib
import select
import socket
import sys
import termios
import time
import tty
import uuid


def command(output, role, text, timeout=30):
    marker = "__ZB_DONE_" + uuid.uuid4().hex
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.settimeout(timeout)
        conn.connect(str(pathlib.Path(output) / (role + ".console")))
        # Stdin is the interactive guest shell. Only the generated marker is
        # shell-quoted here; text is an explicitly requested guest command.
        conn.sendall(("\n" + text + "; echo '" + marker + "'\n").encode())
        data = b""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            chunk = conn.recv(65536)
            if not chunk:
                raise RuntimeError("guest console closed")
            data += chunk
            lines = data.replace(b"\r", b"").split(b"\n")
            if marker.encode() in lines:
                return data.decode(errors="replace")
        raise TimeoutError("guest command did not complete")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=pathlib.Path)
    parser.add_argument("role", choices=["provider", "consumer"])
    parser.add_argument("--command")
    parser.add_argument("--timeout", type=int, default=30)
    args = parser.parse_args()
    if args.command is not None:
        print(command(args.output, args.role, args.command, args.timeout), end="")
        return
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as conn:
        conn.connect(str(args.output / (args.role + ".console")))
        saved = termios.tcgetattr(0) if sys.stdin.isatty() else None
        try:
            if saved:
                tty.setraw(0)
            conn.sendall(b"\n")
            while True:
                readable, _, _ = select.select([conn, sys.stdin], [], [])
                if conn in readable:
                    data = conn.recv(65536)
                    if not data:
                        break
                    os.write(1, data)
                if sys.stdin in readable:
                    data = os.read(0, 4096)
                    if not data or b"\x1d" in data:
                        break
                    conn.sendall(data)
        finally:
            if saved:
                termios.tcsetattr(0, termios.TCSADRAIN, saved)


if __name__ == "__main__":
    main()
