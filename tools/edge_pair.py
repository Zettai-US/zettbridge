#!/usr/bin/env python3
"""Run mapped lease, usage expiry/recovery and peer-loss tests on NEW VM pairs.

Never accepts an existing run.json. Fault tests deliberately lose one guest;
the surviving guest must quarantine rather than reclaim. Artifacts are retained.
"""
import argparse
import json
import pathlib
import re
import socket
import subprocess
import time

from guest_console import command


def qmp(out, role, operation):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(10)
        sock.connect(str(out / (role + ".qmp")))
        with sock.makefile("rwb", buffering=0) as stream:
            json.loads(stream.readline())
            for op in ["qmp_capabilities", operation]:
                stream.write((json.dumps({"execute": op}) + "\n").encode())
                while True:
                    line = stream.readline()
                    if not line and op == "quit":
                        return
                    reply = json.loads(line)
                    if "error" in reply:
                        raise RuntimeError(reply)
                    if "return" in reply:
                        break
            return reply["return"]


def snapshot(out, role):
    node = "rpu0" if role == "provider" else "consumer0"
    fields = ["state", "usage_valid", "usage_age_ns", "ats_status", "ats_enabled",
              "ats_transitions", "ats_faults", "ats_requests", "ats_hits",
              "ats_invalidations", "ats_cached_entries", "dram/allocated_bytes",
              "dram/quarantined_bytes", "dram/quarantined_backing_bytes"]
    script = (f"cd /sys/class/rpu/{node}; for f in " + " ".join(fields) +
              '; do v=$(cat "$f"); printf "%s=%s\\n" "$f" "$v"; done; '
              'v=$(zbctl ' + role + '-stats); printf "__ZB_STATS__%s\\n" "$v"')
    result = command(out, role, script).replace("\r", "")
    values = dict(re.findall(r"^([a-z_/]+)=([^\n]+)$", result, re.M))
    for line in result.splitlines():
        if line.startswith("__ZB_STATS__state="):
            values["stats"] = dict(item.split("=") for item in line.removeprefix("__ZB_STATS__").split())
    values["state"] = re.search(r"^state=([a-z]+)$", result, re.M)[1]
    assert set(fields) <= values.keys(), result
    assert all(re.fullmatch(r"[0-9]+|unknown", values[field]) for field in fields if field != "state"), result
    return values


def stop_service(out, role):
    # Pause only the userspace heartbeat: the kernel worker must keep running.
    command(out, role, 'for f in /proc/[0-9]*/comm; do read name < "$f"; '
            'if [ "$name" = zbctl ]; then p=${f%/comm}; kill -STOP ${p##*/}; fi; done')


def run_case(repo, root, name, args):
    out = root / name
    out.mkdir(mode=0o700)
    # Mark the directory as owned before starting. No external VM paths accepted.
    records = {}
    passed = False
    try:
        subprocess.run(["python3", str(repo / "tools/run_pair.py"), "--keep-running",
                        "--qemu", args.qemu, "--kernel", args.kernel,
                        "--output", str(out)], check=True)
        records["before"] = snapshot(out, "provider")
        assert records["before"]["ats_status"] == "7"
        if name == "recovery":
            result = command(out, "consumer", "zbctl consumer-edge-test", timeout=120)
            (out / "mapped-stress.log").write_text(result)
            assert "MAPPED_REVOKE_EDGE_PASS cycles=128 threads=4" in result, result
            qmp(out, "consumer", "stop")
            time.sleep(3)
            records["expired"] = snapshot(out, "provider")
            stale = records["expired"]
            assert stale["state"] == "active" and stale["usage_valid"] == "0", stale
            assert stale["dram/allocated_bytes"] == "unknown", stale
            assert int(stale["stats"]["flags"], 0) & 8, stale
            assert stale["ats_enabled"] == "1" and stale["ats_transitions"] == "1", stale
            qmp(out, "consumer", "cont")
            for _ in range(30):
                fresh = snapshot(out, "provider")
                if fresh["usage_valid"] == "1":
                    break
                time.sleep(0.2)
            records["recovered"] = fresh
            assert fresh["usage_valid"] == "1" and fresh["ats_status"] == "7", fresh
            assert int(fresh["stats"]["report"]) > int(stale["stats"]["report"]), fresh
            subprocess.run(["python3", str(repo / "tools/stop_pair.py"), str(out)], check=True)
        else:
            survivor = "provider" if name == "consumer-loss" else "consumer"
            lost = "consumer" if survivor == "provider" else "provider"
            stop_service(out, survivor)
            qmp(out, lost, "quit")
            for _ in range(30):
                faulted = snapshot(out, survivor)
                if faulted["state"] == "faulted":
                    break
                time.sleep(0.2)
            records["faulted"] = faulted
            assert faulted["state"] == "faulted" and faulted["usage_valid"] == "0", faulted
            if survivor == "provider":
                assert int(faulted["dram/quarantined_backing_bytes"]) == 256 << 20, faulted
                assert faulted["ats_status"] == "7" and faulted["ats_transitions"] == "1", faulted
            else:
                assert int(faulted["stats"]["quarantined"]) == 128 << 20, faulted
                assert faulted["ats_enabled"] == "unknown", faulted
        passed = True
        print("EDGE PAIR PASS", name, flush=True)
    finally:
        (out / "edge-results.json").write_text(json.dumps(records, indent=2) + "\n")
        # Only sockets beneath the directory created by this invocation.
        for role in ["consumer", "provider"]:
            try:
                qmp(out, role, "quit")
            except (OSError, ValueError):
                pass
        manifest_path = out / "run.json"
        if manifest_path.exists():
            manifest = json.loads(manifest_path.read_text())
            manifest["status"] = "edge-passed-stopped" if passed else "edge-failed-stopped"
            manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=pathlib.Path)
    parser.add_argument("--qemu", default="/root/CXLMemSim/lib/qemu/build/qemu-system-x86_64")
    parser.add_argument("--kernel", default="/root/linux")
    parser.add_argument("--case", choices=["recovery", "consumer-loss", "provider-loss"])
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(mode=0o700)  # must be fresh, including the top-level directory
    repo = pathlib.Path(__file__).resolve().parents[1]
    for name in [args.case] if args.case else ["recovery", "consumer-loss", "provider-loss"]:
        run_case(repo, args.output, name, args)


if __name__ == "__main__":
    main()
