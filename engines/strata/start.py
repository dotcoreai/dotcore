#!/opt/strata/terminal/python/bin/python3 -I
"""Starts Strata's server for one Dotcore model deployment, without an API key.

Dotcore runs this as the container command. It prepares the model's tokenizer and Strata's run
configuration in the deployment's data folder, then replaces itself with Strata's server, so the
container's init supervises the server directly. It serves Strata's native engine path, which reads
the GGUF in place and drafts with the model's own MTP head: no pack and no separate draft layer.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

DATA = Path("/data")
TERMINAL = Path("/opt/strata/terminal")
GGUF_PY = Path("/opt/strata/seed/third_party/llama.cpp/gguf-py")
MAX_PARALLEL = 8
MAX_MTP = 8


def main() -> None:
    args = parse_args()
    tokenizer = prepare_tokenizer(args.model)
    config = write_config(args, tokenizer)

    # An empty key in the environment makes the server refuse to start, and any other value would
    # turn a key on: the deployment is keyless, so none reaches it.
    os.environ.pop("STRATA_API_KEY", None)
    server = [
        sys.executable,
        str(TERMINAL / "serve" / "server.py"),
        "--engine", "strata",
        "--config", str(config),
        "--host", "0.0.0.0",
        "--port", str(args.port),
        "--no-api-key",
        "--offline",
    ]
    os.execv(sys.executable, server)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Start Strata's server for one model, without an API key.")
    parser.add_argument("--model", type=Path, required=True, help="the model's GGUF file")
    parser.add_argument("--model-name", required=True, help="the model name the API reports")
    parser.add_argument("--arch", required=True, help="the model's architecture, as its GGUF names it")
    parser.add_argument("--mtp", type=int, required=True, help=f"tokens drafted per step with the model's MTP head, 1 to {MAX_MTP}")
    parser.add_argument("--context", type=int, required=True, help="tokens of context per request")
    parser.add_argument("--parallel", type=int, default=1, help=f"requests served at once, 1 to {MAX_PARALLEL}")
    parser.add_argument("--expert-vram-mib", type=int, help="MiB of GPU memory for the model's experts; the rest stay in host memory")
    parser.add_argument("--port", type=int, default=8080, help="the port the server listens on")
    args = parser.parse_args()

    if not args.model.is_file():
        parser.error(f"--model {args.model} is not a file")
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.model_name):
        parser.error("--model-name takes letters, digits, '.', '_' and '-'")
    if not re.fullmatch(r"[a-z0-9_]+", args.arch):
        parser.error("--arch takes lowercase letters, digits and '_'")
    if not 1 <= args.mtp <= MAX_MTP:
        parser.error(f"--mtp must be 1 to {MAX_MTP}")
    if args.context < 1:
        parser.error("--context must be at least 1")
    if not 1 <= args.parallel <= MAX_PARALLEL:
        parser.error(f"--parallel must be 1 to {MAX_PARALLEL}")
    if args.expert_vram_mib is not None and args.expert_vram_mib < 1:
        parser.error("--expert-vram-mib must be at least 1")
    if not 1 <= args.port <= 65535:
        parser.error("--port must be 1 to 65535")
    return args


def prepare_tokenizer(model: Path) -> Path:
    """Returns the tokenizer folder for `model`, extracting it from the GGUF when missing or made from another file."""
    out = DATA / "tokenizer"
    tokenizer = out / "tokenizer"
    stamp = out / "source"
    source = f"{model.name} {model.stat().st_size}"
    if (tokenizer / "vocab.json").is_file() and stamp.is_file() and stamp.read_text() == source:
        return tokenizer

    shutil.rmtree(out, ignore_errors=True)
    subprocess.run(
        [sys.executable, str(TERMINAL / "tools" / "strata_tokenizer.py"), "--gguf", str(model), "--out", str(out)],
        env={**os.environ, "STRATA_GGUF_PY": str(GGUF_PY)},
        check=True,
    )
    stamp.write_text(source)
    return tokenizer


def write_config(args: argparse.Namespace, tokenizer: Path) -> Path:
    """Writes Strata's run configuration for this launch and returns its path."""
    engine_args = ["--native", str(args.model), "--max-context", str(args.context), "--prefill", "auto", "--mtp", str(args.mtp)]
    if args.expert_vram_mib is not None:
        # The engine takes GiB. A MiB count over 1024 is exactly representable, and repr prints its
        # shortest exact form, so the cap reaching the engine is the MiB asked for, never more.
        engine_args += ["--expert-vram", repr(args.expert_vram_mib / 1024)]
    config = {
        "exe": "component:strata-engine",
        "args": engine_args,
        "cwd": str(TERMINAL),
        "tokenizer": str(tokenizer),
        "model_name": args.model_name,
        "arch": args.arch,
        "log": str(DATA / "strata-engine.log"),
        "parallel": args.parallel,
    }

    path = DATA / "strata.json"
    temporary = path.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(config, indent=1))
    temporary.replace(path)
    return path


if __name__ == "__main__":
    main()
