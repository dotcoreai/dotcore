"""Prepares oMLX's working directory for one model and execs `omlx serve`.

Dotcore adapters only pass flags and environment, and each workload starts in an empty private
working directory. This launcher turns its flags into the settings files oMLX reads, links the
model snapshot, sets the environment and replaces itself with the server.
"""

import argparse
import json
import os
import platform
import secrets
import sys
from pathlib import Path

DEFAULT_MIN_MACOS = 26


def main():
    args = parse_args()
    check_macos(args.min_macos)

    work = Path.cwd()
    dirs = Directories(work)
    dirs.create()
    link_model(dirs.models / args.served_id, Path(args.snapshot))
    write_json(dirs.base / "settings.json", settings(args, dirs))
    write_json(dirs.base / "model_settings.json", model_settings(args))

    if args.prepare_only:
        return

    os.execve(sys.executable, serve_argv(args, dirs), serve_env(dirs))


class Directories:
    """The working directory layout the server runs in."""

    def __init__(self, work):
        self.base = work / "omlx"
        self.home = work / "home"
        self.tmp = work / "tmp"
        self.models = work / "models"

    def create(self):
        for directory in (self.base, self.home, self.tmp, self.models):
            directory.mkdir(parents=True, exist_ok=True)


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--snapshot", required=True, help="model snapshot directory")
    parser.add_argument("--served-id", required=True, help="model id the server answers to")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--context-per-request", type=int, required=True)
    parser.add_argument("--parallel-requests", type=int, required=True)
    parser.add_argument("--memory-ceiling-gb", type=float, required=True, help="GiB, whole process")
    parser.add_argument("--prompt-cache-gb", type=float, default=0, help="in-RAM prompt cache")
    parser.add_argument("--min-macos", type=int, default=DEFAULT_MIN_MACOS)
    parser.add_argument(
        "--prepare-only", action="store_true", help="write the files, then exit without serving"
    )
    return parser.parse_args()


def check_macos(minimum):
    found = platform.mac_ver()[0]
    if int(found.split(".")[0] or 0) < minimum:
        sys.exit(f"omlx: macOS {minimum} or newer is required, found {found}")


def link_model(link, snapshot):
    if link.is_symlink() or link.exists():
        link.unlink()
    link.symlink_to(snapshot.resolve(), target_is_directory=True)


def settings(args, dirs):
    cache_on = args.prompt_cache_gb > 0
    return {
        "version": "1.0",
        "auth": {"allow_unauthenticated_inference": True, "skip_api_key_verification": False},
        "usage": {"usage_history": False},
        "logging": {"retention_days": 1},
        "huggingface": {"hf_cache_enabled": False},
        "cache": {
            "enabled": cache_on,
            "hot_cache_only": True,
            "hot_cache_max_size": f"{args.prompt_cache_gb}GB",
            "ssd_cache_dir": str(dirs.base / "cache"),
            "ssd_cache_max_size": "1GB",
        },
    }


def model_settings(args):
    return {
        "version": 3,
        "models": {
            args.served_id: {
                "max_context_window": args.context_per_request,
                "is_pinned": True,
                "is_default": True,
            }
        },
    }


def serve_env(dirs):
    return {
        "HOME": str(dirs.home),
        "TMPDIR": str(dirs.tmp),
        "HF_HOME": str(dirs.home / "hf"),
        "HF_HUB_OFFLINE": "1",
        "TRANSFORMERS_OFFLINE": "1",
        "HF_HUB_DISABLE_TELEMETRY": "1",
        "OMLX_BASE_PATH": str(dirs.base),
        "OMLX_API_KEY": secrets.token_urlsafe(32),
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "PYTHONHOME": os.environ["PYTHONHOME"],
        "PYTHONPATH": os.environ["PYTHONPATH"],
        "PYTHONDONTWRITEBYTECODE": "1",
    }


def serve_argv(args, dirs):
    argv = [
        sys.executable, "-m", "omlx.cli", "serve",
        "--model-dir", str(dirs.models),
        "--base-path", str(dirs.base),
        "--host", "127.0.0.1",
        "--port", str(args.port),
        "--headless",
        "--no-hf-cache",
        "--sse-keepalive-mode", "comment",
        "--max-concurrent-requests", str(args.parallel_requests),
        "--memory-guard-gb", str(args.memory_ceiling_gb),
    ]  # fmt: skip
    if args.prompt_cache_gb > 0:
        return argv + ["--hot-cache-max-size", f"{args.prompt_cache_gb}GB"]
    return argv + ["--no-cache"]


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2))


if __name__ == "__main__":
    main()
