#!/usr/bin/env python3
"""Run a tool-using llama.cpp agent on a fresh ATS-enabled Zettbridge VM pair."""
import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import shutil
import subprocess
import time

from edge_pair import qmp, snapshot
from guest_console import command


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def main():
    repo = pathlib.Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", type=pathlib.Path, required=True)
    parser.add_argument("--binary", type=pathlib.Path,
                        default=repo / "artifacts/llama-build/zb-llama-agent")
    parser.add_argument("--order", type=pathlib.Path,
                        default=repo / "tools/llama_agent/order.json")
    parser.add_argument("--output", type=pathlib.Path, required=True)
    parser.add_argument("--timeout", type=int, default=14400)
    args = parser.parse_args()
    for path in [args.model, args.binary, args.order]:
        if not path.is_file():
            parser.error(f"missing input file: {path}")
    out = args.output.resolve()
    out.mkdir(mode=0o700)  # ownership: no existing run directory is accepted
    payload = out / "payload"
    payload.mkdir()
    metadata = {"status": "starting", "inputs": {}}
    build_manifest = args.binary.parent / "agent-build.json"
    if build_manifest.is_file():
        build = json.loads(build_manifest.read_text())
        if build["binary_sha256"] != digest(args.binary):
            parser.error("binary does not match build manifest; rebuild with build_llama_agent.py")
        metadata["build"] = build
    for source, name in [(args.model, "model.gguf"), (args.binary, "zb-llama-agent"),
                         (args.order, "order.json")]:
        source = source.resolve()
        metadata["inputs"][name] = {"source": str(source), "sha256": digest(source)}
        shutil.copyfile(source, payload / name)
        shutil.copymode(source, payload / name)
    image = out / "workload.ext4"
    size = sum(path.stat().st_size for path in payload.iterdir()) + (256 << 20)
    size = (size + (256 << 20) - 1) // (256 << 20) * (256 << 20)
    with image.open("xb") as file:
        file.truncate(size)
    subprocess.run(["/usr/sbin/mke2fs", "-q", "-t", "ext4", "-F", "-d",
                    str(payload), str(image)], check=True)
    pair = out / "pair"
    samples = []
    try:
        subprocess.run(["python3", str(repo / "tools/run_pair.py"), "--keep-running",
                        "--consumer-memory", "4G", "--consumer-disk", str(image),
                        "--output", str(pair)], check=True)
        command(pair, "consumer", "mkdir -p /workload; mount -t ext4 -o ro /dev/vda /workload")
        baseline = snapshot(pair, "consumer")
        metadata["consumer_before"] = baseline
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(command, pair, "consumer",
                                 "/workload/zb-llama-agent /workload/model.gguf "
                                 "/workload/order.json cxl", args.timeout)
            started = time.monotonic()
            try:
                while not future.done():
                    value = snapshot(pair, "provider")
                    value["elapsed_seconds"] = time.monotonic() - started
                    samples.append(value)
                    temporary = out / "provider-samples.json.tmp"
                    temporary.write_text(json.dumps(samples, indent=2) + "\n")
                    temporary.replace(out / "provider-samples.json")
                    time.sleep(1)
            except BaseException:
                # Unblock the serial reader before the executor joins it.
                # These sockets belong exclusively to this fresh run.
                for role in ["consumer", "provider"]:
                    try:
                        qmp(pair, role, "quit")
                    except (OSError, ValueError):
                        pass
                raise
            transcript = future.result()
        (out / "agent-console.log").write_text(transcript)
        events = []
        for line in transcript.replace("\r", "").splitlines():
            if line.startswith('{"event":'):
                events.append(json.loads(line))
        metadata["events"] = events
        passed = [e for e in events if e["event"] == "AGENT_E2E_PASS"]
        assert len(passed) == 1 and passed[0]["token_exact_match"] and passed[0]["direct_kv"], transcript[-5000:]
        assert passed[0]["logits_hash_match"]
        mappings = [e for e in events if e["event"] == "KV_MAPPED"]
        checks = [e for e in events if e["event"] == "KV_DIRECT_VERIFIED"]
        releases = [e for e in events if e["event"] == "KV_RELEASED"]
        assert mappings and all(e["remote"] for e in mappings)
        assert len(checks) == 3 and all(e["remote"] for e in checks)
        assert [e["round"] for e in checks] == [0, 1, 2]
        assert all(a["graph_refs"] < b["graph_refs"] for a, b in zip(checks, checks[1:]))
        assert all(a["attention_rows"] < b["attention_rows"] for a, b in zip(checks, checks[1:]))
        assert len(releases) == len(mappings) and all(e["completed"] for e in releases)
        assert not any(e["event"].startswith("STATE_") for e in events)
        assert not any(e["event"] == "AGENT_E2E_FAIL" for e in events)
        allocated = int(baseline["stats"]["allocated"])
        mirror = [s for s in samples if s["usage_valid"] == "1" and
                  int(s["stats"]["allocated"]) > allocated]
        assert mirror, "provider never reported the direct KV lease"
        kv_bytes = sum(e["bytes"] for e in mappings)
        assert all(e["bytes"] == kv_bytes for e in checks)
        assert any(int(s["stats"]["allocated"]) == allocated + kv_bytes for s in mirror)
        assert all(s["ats_status"] == "7" and s["ats_transitions"] == "1" and
                   s["ats_faults"] == "0" for s in samples), "ATS continuity failed"
        assert all(s["usage_valid"] == "1" for s in samples), "usage reporting expired during inference"
        assert int(samples[-1]["stats"]["report"]) > int(samples[0]["stats"]["report"])
        metadata["ats_translation_requests_delta"] = int(samples[-1]["ats_requests"]) - int(samples[0]["ats_requests"])
        metadata["ats_hits_delta"] = int(samples[-1]["ats_hits"]) - int(samples[0]["ats_hits"])
        assert metadata["ats_translation_requests_delta"] > 1000
        assert metadata["ats_hits_delta"] > 100000, "no substantial shared-memory DMA traffic"
        after = snapshot(pair, "consumer")
        metadata["consumer_after"] = after
        assert int(after["stats"]["allocated"]) == allocated
        assert int(after["stats"]["releasing"]) == 0
        assert int(after["stats"]["quarantined"]) == 0
        metadata["provider_peak_allocated"] = max(int(s["stats"]["allocated"]) for s in mirror)
        subprocess.run(["python3", str(repo / "tools/stop_pair.py"), str(pair)], check=True)
        metadata["status"] = "passed"
        print("LLAMA AGENT E2E PASS", out, flush=True)
    except BaseException as exc:
        metadata["status"] = "failed"
        metadata["error"] = str(exc)
        raise
    finally:
        (out / "result.json").write_text(json.dumps(metadata, indent=2) + "\n")
        for role in ["consumer", "provider"]:
            try:
                qmp(pair, role, "quit")
            except (OSError, ValueError):
                pass


if __name__ == "__main__":
    main()
