#!/usr/bin/env python3
"""Boot two isolated Linux guests; test and exit, or leave an active CXL pool."""
import argparse
import hashlib
import json
import os
import pathlib
import re
import subprocess
import tempfile
import time

GIB = 1 << 30
MIB = 1 << 20


def memory_size(value):
    match = re.fullmatch(r"([1-9][0-9]*)([MG]?)", value.upper())
    if not match:
        raise argparse.ArgumentTypeError("use bytes or an integer M/G size, for example 8G")
    return int(match[1]) * {"": 1, "M": MIB, "G": GIB}[match[2]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--qemu", default="/root/CXLMemSim/lib/qemu/build/qemu-system-x86_64")
    parser.add_argument("--kernel", default="/root/linux")
    parser.add_argument("--output")
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--accel", choices=["kvm", "tcg"], default="kvm")
    parser.add_argument("--ats", choices=["on", "off"], default="on",
                        help="keep provider ATS enabled (default), or run the original baseline")
    parser.add_argument("--capacity", type=memory_size, default=256 * MIB)
    parser.add_argument("--provider-memory", type=memory_size)
    parser.add_argument("--consumer-memory", type=memory_size, default=2 * GIB)
    parser.add_argument("--keep-running", action="store_true",
                        help="leave the pool active, with serial console and QMP sockets")
    parser.add_argument("--consumer-disk", type=pathlib.Path,
                        help="attach a read-only raw workload disk to the consumer")
    parser.add_argument("--drop-commit-ack", action="store_true")
    parser.add_argument("--drop-ats-test-ack", action="store_true",
                        help="exercise completion loss for the temporary ATS test buffer")
    parser.add_argument("--query-unknown", action="store_true")
    parser.add_argument("--lifetime", action="store_true")
    args = parser.parse_args()
    if args.consumer_disk:
        args.consumer_disk = args.consumer_disk.resolve()
        if not args.consumer_disk.is_file() or "," in str(args.consumer_disk):
            parser.error("--consumer-disk must name a regular file without commas")
    if not 256 * MIB <= args.capacity <= 64 * GIB or args.capacity & (args.capacity - 1):
        parser.error("--capacity must be a power of two from 256M through 64G")
    if args.drop_commit_ack and args.drop_ats_test_ack:
        parser.error("choose only one completion-loss injection")
    if args.drop_ats_test_ack and args.ats != "on":
        parser.error("--drop-ats-test-ack requires --ats on")
    if args.lifetime and (args.drop_commit_ack or args.drop_ats_test_ack):
        parser.error("--lifetime cannot inject commit faults")
    if args.query_unknown and not (args.drop_commit_ack or args.drop_ats_test_ack):
        parser.error("--query-unknown requires a completion-loss injection")
    if args.keep_running and (args.lifetime or args.query_unknown):
        parser.error("--keep-running requires a working data path")
    provider_ram = args.provider_memory or max(2 * GIB, (args.capacity * 3 // 2 + GIB - 1) // GIB * GIB)
    if provider_ram < args.capacity + GIB:
        parser.error("provider RAM must exceed pool capacity by at least 1G")
    if min(provider_ram, args.consumer_memory) % MIB or args.consumer_memory < GIB:
        parser.error("guest RAM must be MiB aligned; consumer needs at least 1G")
    repo = pathlib.Path(__file__).resolve().parents[1]
    out = pathlib.Path(args.output or tempfile.mkdtemp(prefix="zettbridge-pair-")).resolve()
    out.mkdir(parents=True, exist_ok=True, mode=0o700)
    if (out / "run.json").exists() or (out / "bridge.sock").exists():
        parser.error("use a fresh output directory")
    if len(os.fsencode(out / "consumer.console")) >= 108:
        parser.error("output path is too long for Unix sockets")
    os.umask(0o077)
    initrd = out / "initramfs.cpio.gz"
    subprocess.run(["python3", str(repo / "tools/build_initramfs.py"), "--kernel", args.kernel,
                    "--output", str(initrd)], check=True)
    common = [args.qemu, "-machine", "q35,cxl=on,kernel-irqchip=split", "-accel", args.accel,
              "-cpu", "host,hypervisor=off" if args.accel == "kvm" else "max,hypervisor=off",
              "-smp", "2", "-nodefaults", "-nographic", "-no-reboot", "-monitor", "none",
              "-kernel", str(pathlib.Path(args.kernel) / "arch/x86/boot/bzImage"),
              "-initrd", str(initrd), "-device",
              f"intel-iommu,intremap=on,caching-mode=on,aw-bits=48,device-iotlb={args.ats}",
              "-device", "pcie-root-port,id=mgmt,bus=pcie.0,chassis=1,slot=1"]
    manifest = {
        "commands": {}, "pids": {}, "capacity_bytes": args.capacity,
        "provider_ram_bytes": provider_ram, "consumer_ram_bytes": args.consumer_memory,
        "keep_running": args.keep_running, "status": "starting",
        "ats": args.ats,
        "consumer_disk": str(args.consumer_disk) if args.consumer_disk else None,
        "kernel_sha": subprocess.check_output(["git", "-C", args.kernel, "rev-parse", "HEAD"], text=True).strip(),
        "kernel_release": (pathlib.Path(args.kernel) / "include/config/kernel.release").read_text().strip(),
        "driver_implementation": "native Rust OOT with stateless kernel helpers",
    }
    for key, path in [("module", repo / "zettbridge.ko"), ("qemu", pathlib.Path(args.qemu)),
                      ("kernel_image", pathlib.Path(args.kernel) / "arch/x86/boot/bzImage")]:
        with path.open("rb") as binary:
            manifest[key + "_sha256"] = hashlib.file_digest(binary, "sha256").hexdigest()

    def save_manifest():
        (out / "run.json").write_text(json.dumps(manifest, indent=2) + "\n")

    processes, files = [], []
    leave_running = False
    try:
        for role in ["provider", "consumer"]:
            memory = provider_ram if role == "provider" else args.consumer_memory
            # Capacity and zero/test commands execute synchronously in this model.
            # Large-range DMA can block the peer's management RPC during startup.
            ipc_timeout = 30000 if args.capacity > 256 * MIB else 2000
            options = (f"zettbridge,id=rpu,bus=mgmt,provider={'on' if role == 'provider' else 'off'},"
                       f"socket={out / 'bridge.sock'},capacity={args.capacity},timeout-ms={ipc_timeout}")
            if role == "provider" and args.drop_commit_ack:
                options += ",x-drop-ack-op=512"
            if role == "provider" and args.drop_ats_test_ack:
                options += ",x-drop-ack-op=261"
            if role == "provider":
                options += f",ats={args.ats}"
            if role == "provider" and args.query_unknown:
                options += ",x-query-unknown=on"
            cmd = common + [
                "-m", f"{memory // MIB}M,maxmem={(memory + max(2 * GIB, args.capacity)) // MIB}M,slots=4",
                "-device", options,
                "-append", ("console=ttyS0 earlyprintk=serial panic=-1 nokaslr intel_iommu=on "
                            "iommu.strict=1 module.sig_enforce=0 "
                            f"zb.role={role} zb.capacity={args.capacity} "
                            f"zb.quarantine={int(args.query_unknown and role == 'provider')} "
                            f"zb.lifetime={int(args.lifetime)} zb.live={int(args.keep_running)} "
                            f"zb.ats_quarantine={int(args.query_unknown and args.drop_ats_test_ack and role == 'provider')} "
                            f"zb.ats={args.ats}"),
                "-chardev", (f"socket,id=serial,path={out / (role + '.console')},server=on,wait=off,"
                             f"logfile={out / (role + '.log')}"), "-serial", "chardev:serial",
                "-qmp", f"unix:{out / (role + '.qmp')},server=on,wait=off",
                "-pidfile", str(out / (role + ".pid")),
            ]
            if role == "consumer":
                if args.consumer_disk:
                    cmd += ["-drive", f"file={args.consumer_disk},if=none,format=raw,readonly=on,id=workload",
                            "-device", "virtio-blk-pci,drive=workload"]
                cmd += ["-device", "pxb-cxl,id=cxl.1,bus=pcie.0,bus_nr=52",
                        "-device", "cxl-rp,id=cxl_rp,bus=cxl.1,port=0,chassis=2,slot=0",
                        "-object", f"memory-backend-ram,id=cxl-advertised,size={args.capacity}",
                        "-device", "cxl-type3,id=cxl0,bus=cxl_rp,volatile-memdev=cxl-advertised,zettbridge=rpu,memsim-atomics=off,sn=1",
                        "-M", f"cxl-fmw.0.targets.0=cxl.1,cxl-fmw.0.size={args.capacity}"]
            manifest["commands"][role] = cmd
            log = open(out / f"{role}.qemu.log", "w")
            files.append(log)
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("CXL_MEMSIM", "CXL_TRANSPORT", "CXL_HOST"))}
            proc = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=log,
                                    stderr=subprocess.STDOUT, env=env, start_new_session=True)
            processes.append(proc)
            manifest["pids"][role] = proc.pid
            save_manifest()
            if role == "provider":
                for _ in range(200):
                    if (out / "bridge.sock").exists():
                        break
                    if proc.poll() is not None:
                        raise RuntimeError("provider failed to start; see provider.qemu.log")
                    time.sleep(0.05)
                else:
                    raise TimeoutError("provider socket did not appear")
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            logs = [(out / f"{role}.log").read_text(errors="replace")
                    if (out / f"{role}.log").exists() else "" for role in ["provider", "consumer"]]
            if any(any(marker in log for marker in ["ZB_GUEST_FAIL", "Kernel panic", "BUG:", "Oops:", "FAIL "])
                   for log in logs):
                raise RuntimeError("guest failure; see serial logs")
            if any(proc.poll() is not None for proc in processes):
                raise RuntimeError("QEMU exited before readiness; see serial and .qemu.log files")
            if args.keep_running:
                # printk can interrupt a userspace serial write mid-marker.
                # Retain raw logs for fault checks; strip only timestamped
                # kernel records when reconstructing userspace readiness.
                ready = [re.sub(r"\[\s*\d+\.\d+\][^\n]*\n", "", log) for log in logs]
                if ("PROVIDER READY" in ready[0] and "CONSUMER APERTURE_SPAN_PASS" in ready[1]
                        and "CONSUMER HEARTBEAT seq=1" in ready[1]):
                    leave_running = True
                    manifest["status"] = "running"
                    print(f"PAIR RUNNING capacity={args.capacity} pids={manifest['pids']} {out}")
                    break
            elif args.query_unknown and "PROVIDER QUARANTINE_PASS" in logs[0] and "ZB_BOOT role=consumer" in logs[1]:
                manifest["status"] = "passed"
                print("QUARANTINE PASS", out)
                break
            elif all("ZB_GUEST_PASS" in log for log in logs):
                manifest["status"] = "passed"
                print("PAIR PASS", out)
                break
            time.sleep(0.25)
        else:
            raise TimeoutError("pair startup/test timed out")
    except BaseException:
        manifest["status"] = "failed"
        raise
    finally:
        if not leave_running:
            for proc in processes:
                if proc.poll() is None:
                    proc.terminate()
            for proc in processes:
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()
        save_manifest()
        for log in files:
            log.close()
        print("Logs:", out)


if __name__ == "__main__":
    main()
