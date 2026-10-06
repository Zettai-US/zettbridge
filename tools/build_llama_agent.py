#!/usr/bin/env python3
"""Build the standalone Zettbridge agent against a pinned unmodified llama.cpp.

Uses a pinned ggml buffer ABI; no patches or submissions to llama.cpp.
The model download is pinned and SHA-256 checked. All outputs go to artifacts/.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import urllib.request

LLAMA_COMMIT = "ad6c66839af3c5646fba8c6c2e2087a1e4e38948"
MODEL_COMMIT = "9217f5db79a29953eb74d5343926648285ec7e67"
MODEL_FILE = "qwen2.5-0.5b-instruct-q8_0.gguf"
MODEL_SHA256 = "ca59ca7f13d0e15a8cfa77bd17e65d24f6844b554a7b6c12e07a5f89ff76844e"
MODEL_URL = f"https://huggingface.co/Qwen/Qwen2.5-0.5B-Instruct-GGUF/resolve/{MODEL_COMMIT}/{MODEL_FILE}"


def sha256(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def main():
    repo = pathlib.Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=pathlib.Path, default=repo / "artifacts/llama-src")
    parser.add_argument("--build", type=pathlib.Path, default=repo / "artifacts/llama-build")
    parser.add_argument("--model-dir", type=pathlib.Path, default=repo / "artifacts/llama-models")
    parser.add_argument("--cmake", default="/usr/bin/cmake")
    parser.add_argument("--jobs", default=8, type=int)
    args = parser.parse_args()
    if not args.source.exists():
        args.source.mkdir(parents=True)
        subprocess.run(["git", "init", str(args.source)], check=True)
        subprocess.run(["git", "-C", str(args.source), "fetch", "--depth=1",
                        "https://github.com/ggml-org/llama.cpp.git", LLAMA_COMMIT], check=True)
        subprocess.run(["git", "-C", str(args.source), "checkout", "--detach", "FETCH_HEAD"], check=True)
    actual = subprocess.check_output(["git", "-C", str(args.source), "rev-parse", "HEAD"], text=True).strip()
    dirty = subprocess.check_output(["git", "-C", str(args.source), "status", "--porcelain"], text=True)
    if actual != LLAMA_COMMIT or dirty:
        parser.error("source must be clean at the pinned commit; existing changes are never overwritten")
    args.model_dir.mkdir(parents=True, exist_ok=True)
    model = args.model_dir / MODEL_FILE
    if not model.exists():
        partial = model.with_suffix(".gguf.partial")
        with urllib.request.urlopen(MODEL_URL, timeout=60) as source, partial.open("wb") as target:
            shutil.copyfileobj(source, target, length=1 << 20)
        if sha256(partial) != MODEL_SHA256:
            raise RuntimeError("model checksum failed; partial file retained for diagnosis")
        partial.rename(model)
    if sha256(model) != MODEL_SHA256:
        parser.error("existing model checksum differs from pinned model; not overwriting")
    subprocess.run([args.cmake, "-S", str(repo / "tools/llama_agent"), "-B", str(args.build),
                    "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release",
                    "-DLLAMA_SOURCE=" + str(args.source.resolve())], check=True)
    subprocess.run([args.cmake, "--build", str(args.build), "--target", "zb-llama-agent", "zb-attention-test",
                    "-j", str(args.jobs)], check=True)
    subprocess.run([str((args.build / "zb-attention-test").resolve())], check=True)
    binary = args.build / "zb-llama-agent"
    manifest = {"llama_repository": "https://github.com/ggml-org/llama.cpp",
                "llama_commit": LLAMA_COMMIT, "llama_modified": False,
                "attention_reference_test_passed": True,
                "adapter_sources": {p.name: sha256(p) for p in sorted((repo / "tools/llama_agent").iterdir())
                                    if p.is_file()},
                "model_url": MODEL_URL, "model_sha256": MODEL_SHA256,
                "binary_sha256": sha256(binary), "binary": str(binary.resolve()),
                "model": str(model.resolve())}
    (args.build / "agent-build.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(json.dumps(manifest, indent=2))


if __name__ == "__main__":
    main()
