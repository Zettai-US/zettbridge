#!/usr/bin/env python3
"""Release the live lease, retire the provider export, then stop both QEMUs."""
import argparse
import json
import pathlib
import socket
import time
from guest_console import command


def quit_qemu(path):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(10)
        sock.connect(str(path))
        stream = sock.makefile("rwb", buffering=0)
        json.loads(stream.readline())  # QMP greeting
        for operation in ["qmp_capabilities", "quit"]:
            stream.write((json.dumps({"execute": operation}) + "\n").encode())
            while True:
                line = stream.readline()
                if not line:
                    if operation == "quit":
                        return
                    raise RuntimeError("QMP closed")
                reply = json.loads(line)
                if "error" in reply:
                    raise RuntimeError(reply["error"])
                if "return" in reply:
                    break


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=pathlib.Path)
    args = parser.parse_args()
    manifest_path = args.output / "run.json"
    manifest = json.loads(manifest_path.read_text())
    if manifest["status"] != "running" or not manifest["keep_running"]:
        parser.error("output is not a running live pair")
    print(command(args.output, "consumer", "touch /run/zettbridge-stop; "
                  "while [ ! -e /run/zettbridge-consumer-drained ]; do sleep 1; done", 120), end="")
    print(command(args.output, "provider", "touch /run/zettbridge-stop"), end="")
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        if all((role.upper() + " STOPPED") in (args.output / (role + ".log")).read_text(errors="replace")
               for role in ["consumer", "provider"]):
            break
        time.sleep(0.25)
    else:
        raise TimeoutError("retirement did not complete; guests were left running for diagnosis")
    for role in ["consumer", "provider"]:
        quit_qemu(args.output / (role + ".qmp"))
    manifest["status"] = "stopped"
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    print("PAIR STOPPED after lease release, drain, retire and reclaim", args.output)


if __name__ == "__main__":
    main()
