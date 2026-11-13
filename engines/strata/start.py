#!/opt/strata/terminal/python/bin/python3 -I
"""Starts Strata's server for one Dotcore model deployment, without an API key.

Dotcore runs this as the container command. It prepares the model's tokenizer, and its pack when the
model needs one, and Strata's run configuration in the deployment's data folder, then replaces itself
with Strata's server, so the container's init supervises the server directly.

It serves two engine paths. A single GGUF is read in place and drafts with its own MTP head. A model
with a separate draft layer runs from a pack: split GGUF shards are packed on first start, keeping
their experts in the GGUF, while a ready-made pack is used as it is.
"""

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import NoReturn

DATA = Path("/data")
TERMINAL = Path("/opt/strata/terminal")
MAX_PARALLEL = 8
MAX_MTP = 8
MIB = 1024 * 1024
FIRST_SHARD = re.compile(r".+-00001-of-\d{5}\.gguf")
READY_PACK_OPTIONS = ("pack", "dense_gguf", "lookup_table_gguf", "token_embedding_gguf")


def main() -> None:
    args = parse_args()
    if args.draft_layer is None:
        tokenizer = prepare_tokenizer(args.model)
        engine_args = gguf_engine_args(args)
    else:
        pack, model_args = ready_pack_files(args) if args.pack else split_gguf_files(args.model)
        tokenizer = pack / "tokenizer"
        engine_args = model_args + pack_engine_args(args, pack)
    config = write_config(args, engine_args, tokenizer)

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
    parser.add_argument("--model", type=Path, required=True, help="the model's GGUF file, or with --draft-layer the folder holding the model's files")
    parser.add_argument("--draft-layer", type=Path, help="the folder holding the model's separate MTP draft layer; runs the model from a pack")
    parser.add_argument("--pack", help="with --draft-layer, the ready-made pack's folder within --model; without it, the GGUF shards in --model are packed")
    parser.add_argument("--dense-gguf", help="with --pack, the GGUF within --model holding the dense weights")
    parser.add_argument("--lookup-table-gguf", help="with --pack, the GGUF within --model holding the per-layer embedding lookup table")
    parser.add_argument("--token-embedding-gguf", help="with --pack, the GGUF within --model holding the token embedding")
    parser.add_argument("--unified-memory", action="store_true", help="with --draft-layer, the GPU shares the host's memory")
    parser.add_argument("--model-name", required=True, help="the model name the API reports")
    parser.add_argument("--arch", required=True, help="the model's architecture, as its GGUF names it")
    parser.add_argument("--mtp", type=int, required=True, help=f"tokens drafted per step with the model's MTP layer, 1 to {MAX_MTP}")
    parser.add_argument("--context", type=int, required=True, help="tokens of context per request")
    parser.add_argument("--parallel", type=int, default=1, help=f"requests served at once, 1 to {MAX_PARALLEL}")
    parser.add_argument("--expert-vram-mib", type=int, help="MiB of GPU memory for the model's experts; the rest stay in host memory")
    parser.add_argument("--port", type=int, default=8080, help="the port the server listens on")
    args = parser.parse_args()

    if args.draft_layer is None:
        if not args.model.is_file():
            parser.error(f"--model {args.model} is not a file")
        if args.unified_memory or any(getattr(args, option) for option in READY_PACK_OPTIONS):
            parser.error("--pack, its GGUF options and --unified-memory need --draft-layer")
    else:
        if not args.model.is_dir():
            parser.error(f"--model {args.model} is not a folder")
        if not args.draft_layer.is_dir():
            parser.error(f"--draft-layer {args.draft_layer} is not a folder")
        roles = [getattr(args, option) for option in READY_PACK_OPTIONS]
        if any(roles) and not all(roles):
            parser.error("--pack, --dense-gguf, --lookup-table-gguf and --token-embedding-gguf go together")
        for option, role in zip(READY_PACK_OPTIONS, roles):
            if role and (Path(role).is_absolute() or ".." in Path(role).parts):
                parser.error(f"--{option.replace('_', '-')} {role} is not a path within --model")
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
        check=True,
    )
    stamp.write_text(source)
    return tokenizer


def gguf_engine_args(args: argparse.Namespace) -> list[str]:
    """Returns the engine arguments of the single-GGUF path, which reads the GGUF in place."""
    engine_args = ["--native", str(args.model), "--max-context", str(args.context), "--prefill", "auto", "--mtp", str(args.mtp)]
    if args.expert_vram_mib is not None:
        # The engine takes GiB. A MiB count over 1024 is exactly representable, and repr prints its
        # shortest exact form, so the cap reaching the engine is the MiB asked for, never more.
        engine_args += ["--expert-vram", repr(args.expert_vram_mib / 1024)]
    return engine_args


def ready_pack_files(args: argparse.Namespace) -> tuple[Path, list[str]]:
    """Returns a ready-made pack's folder and the engine arguments naming it and its GGUF files."""
    pack = args.model / args.pack
    dense = args.model / args.dense_gguf
    for path in (pack / "native_experts.txt", pack / "tokenizer" / "vocab.json", dense,
                 args.model / args.lookup_table_gguf, args.model / args.token_embedding_gguf):
        if not path.is_file():
            fail(f"the ready-made pack has no {path}")
    # The dense GGUF is also the native model: alone, --native would look for a second shard beside it.
    return pack, [
        "--pack", str(pack),
        "--native", str(dense),
        "--native-dense-gguf", str(dense),
        "--ple-gguf", str(args.model / args.lookup_table_gguf),
        "--embd-gguf", str(args.model / args.token_embedding_gguf),
    ]


def split_gguf_files(model: Path) -> tuple[Path, list[str]]:
    """Returns the pack built from the GGUF shards in `model` and the engine arguments naming them.

    The engine finds the other shards beside the first one, and the shard holding the lookup table.
    """
    shards = sorted(path for path in model.rglob("*.gguf") if FIRST_SHARD.fullmatch(path.name))
    if len(shards) != 1:
        fail(f"{model} must hold one set of split GGUF shards, but has {len(shards)} first shards")
    first = shards[0]
    return prepare_pack(first), ["--pack", str(DATA / "pack"), "--native", str(first)]


def prepare_pack(first: Path) -> Path:
    """Returns the pack for the GGUF shards beside `first`, building it when missing or made from other files.

    The pack holds the tokenizer and the dense weights in the form the engine reads; the experts stay
    in the GGUF shards.
    """
    pack = DATA / "pack"
    stamp = DATA / "pack.source"
    shards = sorted(first.parent.glob(first.name.replace("-00001-of-", "-*-of-")))
    source = " ".join(f"{shard.name} {shard.stat().st_size}" for shard in shards)
    # The packer writes the experts table last, so a pack with it and a matching stamp is complete.
    if (pack / "native_experts.txt").is_file() and stamp.is_file() and stamp.read_text() == source:
        return pack

    stamp.unlink(missing_ok=True)
    shutil.rmtree(pack, ignore_errors=True)
    subprocess.run(
        [sys.executable, str(TERMINAL / "tools" / "iq_pack.py"), "--gguf", str(first), "--out", str(pack)],
        check=True,
    )
    stamp.write_text(source)
    return pack


def pack_engine_args(args: argparse.Namespace, pack: Path) -> list[str]:
    """Returns the engine arguments of the pack path after the model's files.

    The lookup table is read from disk as needed rather than locked in host memory. With unified
    memory, the experts are mapped from their files so the GPU's cache holds them without a second
    copy, using upstream's measured settings for that part.
    """
    cache = "auto" if args.expert_vram_mib is None else str(expert_cache_slots(pack, args.expert_vram_mib))
    engine_args = [
        "--expert-profile", str(TERMINAL / "data" / "expert-profile.bin"),
        "--expert-cache", cache,
        "--prefill", "auto:32768" if args.unified_memory else "auto",
        "--spec", str(args.mtp),
        "--spec-min-p", "0.5",
        "--mtp", str(draft_layer_folder(args.draft_layer)),
        # The draft layer's own token subset may be another language's; the image's is upstream's default.
        "--mtp-draft-vocab", str(TERMINAL / "data" / "draft_vocab.bin"),
        "--max-context", str(args.context),
        "--kv", "int8",
    ]
    if args.unified_memory:
        engine_args += ["--mmap-experts", "--mtp-q4", "all", "--mtp-hnorm", "stream"]
    return engine_args


def expert_cache_slots(pack: Path, cap_mib: int) -> int:
    """Returns the expert cache slots that fit `cap_mib`, at most one per expert of the pack.

    Each slot holds the pack's largest per-layer expert block, as the engine sizes them.
    """
    layers, largest, experts = 0, 0, None
    for line in (pack / "native_experts.txt").read_text().splitlines():
        if line.startswith("#"):
            found = re.search(r"\(n_expert (\d+)", line)
            experts = int(found[1]) if found else experts
        elif line.strip():
            layers += 1
            largest = max(largest, int(line.split()[4]))
    if experts is None or layers == 0 or largest == 0:
        fail(f"{pack / 'native_experts.txt'} names no expert count or expert blocks")
    slots = min(cap_mib * MIB // largest, layers * experts)
    # The engine reads 0 slots as its automatic size.
    if slots == 0:
        fail(f"--expert-vram-mib {cap_mib} holds no expert block of {largest} bytes")
    return slots


def draft_layer_folder(draft_layer: Path) -> Path:
    """Returns the folder within `draft_layer` holding the draft layer's files."""
    folders = sorted(path.parent for path in draft_layer.rglob("dense.txt"))
    if len(folders) != 1:
        fail(f"{draft_layer} must hold one draft layer, but has {len(folders)}")
    for name in ("dense.bin", "experts.bin"):
        if not (folders[0] / name).is_file():
            fail(f"the draft layer in {folders[0]} has no {name}")
    return folders[0]


def write_config(args: argparse.Namespace, engine_args: list[str], tokenizer: Path) -> Path:
    """Writes Strata's run configuration for this launch and returns its path."""
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
    if args.draft_layer is not None:
        # The server may otherwise pick another drafter by the model's name.
        config["drafting"] = "mtp"

    path = DATA / "strata.json"
    temporary = path.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(config, indent=1))
    temporary.replace(path)
    return path


def fail(message: str) -> NoReturn:
    """Stops the start with `message`."""
    sys.exit(f"dotcore-strata-start: {message}")


if __name__ == "__main__":
    main()
