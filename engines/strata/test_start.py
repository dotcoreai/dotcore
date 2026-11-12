"""Tests the Strata start wrapper's engine paths: the run configuration each model file layout gets.

Run it with the image's Python and the wrapper's path, as `just image-test` does. Each test lays out
fake model files in a temporary folder and stops the wrapper where it would start the server.
"""

import importlib.machinery
import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

# Loading the wrapper must not leave a bytecode cache beside it.
sys.dont_write_bytecode = True
WRAPPER = Path(sys.argv[1] if len(sys.argv) > 1 else "/usr/local/bin/dotcore-strata-start")
MIB = 1024 * 1024
BLOB = 2764816
EXPERTS_TABLE = (
    "# strata native experts v3: layer gu_type d_type offset blob_bytes (n_expert 512, total 2831171584)\n"
    f"0 40 40 0 {BLOB}\n"
    f"1 40 40 1415585792 {BLOB - 16}\n"
)
# A packer standing in for upstream's: it writes the pack's files and counts its runs.
FAKE_PACKER = f"""
import pathlib, sys
out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1])
(out / "tokenizer").mkdir(parents=True, exist_ok=True)
(out / "tokenizer" / "vocab.json").write_text("{{}}")
(out / "native_experts.txt").write_text({EXPERTS_TABLE!r})
with open(out.parent / "packer-runs", "a") as runs:
    runs.write(sys.argv[sys.argv.index("--gguf") + 1] + "\\n")
"""


class StartWrapperTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.data = self.root / "data"
        self.terminal = self.root / "terminal"
        self.data.mkdir()
        (self.terminal / "tools").mkdir(parents=True)
        (self.terminal / "tools" / "iq_pack.py").write_text(FAKE_PACKER)
        self.draft = self.files("draft", ["mtp/dense.bin", "mtp/dense.txt", "mtp/experts.bin"])

        loader = importlib.machinery.SourceFileLoader("start", str(WRAPPER))
        self.start = importlib.util.module_from_spec(importlib.util.spec_from_loader("start", loader))
        loader.exec_module(self.start)
        self.start.DATA = self.data
        self.start.TERMINAL = self.terminal

    def test_single_gguf_reads_the_gguf_in_place_with_its_own_mtp_head(self):
        model = self.files("model", ["model.gguf"]) / "model.gguf"
        tokenizer = self.data / "tokenizer"
        (tokenizer / "tokenizer").mkdir(parents=True)
        (tokenizer / "tokenizer" / "vocab.json").write_text("{}")
        (tokenizer / "source").write_text("model.gguf 0")

        config = self.run_wrapper("--model", model, "--arch", "qwen35moe", "--mtp", "2", "--expert-vram-mib", "1536")

        self.assertEqual(config, {
            "exe": "component:strata-engine",
            "args": ["--native", str(model), "--max-context", "4096", "--prefill", "auto", "--mtp", "2", "--expert-vram", "1.5"],
            "cwd": str(self.terminal),
            "tokenizer": str(tokenizer / "tokenizer"),
            "model_name": "m",
            "arch": "qwen35moe",
            "log": str(self.data / "strata-engine.log"),
            "parallel": 1,
        })

    def test_split_gguf_is_packed_once_and_runs_from_the_pack(self):
        model = self.files("model", ["IQ2_XS/m-00001-of-00002.gguf", "IQ2_XS/m-00002-of-00002.gguf"])
        first = model / "IQ2_XS" / "m-00001-of-00002.gguf"

        config = self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "100")
        self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "100")

        pack = self.data / "pack"
        self.assertEqual((self.data / "packer-runs").read_text(), f"{first}\n")
        self.assertEqual(config["tokenizer"], str(pack / "tokenizer"))
        self.assertEqual(config["drafting"], "mtp")
        self.assertEqual(config["args"], [
            "--pack", str(pack),
            "--native", str(first),
            "--expert-profile", str(self.terminal / "data" / "expert-profile.bin"),
            "--expert-cache", str(100 * MIB // BLOB),
            "--prefill", "auto",
            "--spec", "4",
            "--spec-min-p", "0.5",
            "--mtp", str(self.draft / "mtp"),
            "--mtp-draft-vocab", str(self.terminal / "data" / "draft_vocab.bin"),
            "--max-context", "4096",
            "--kv", "int8",
        ])

        (model / "IQ2_XS" / "m-00002-of-00002.gguf").write_text("changed")
        self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "100")
        self.assertEqual((self.data / "packer-runs").read_text(), f"{first}\n{first}\n")

    def test_ready_pack_on_unified_memory_names_its_files_and_caps_the_cache_at_every_expert(self):
        model = self.files("model", ["pack/tokenizer/vocab.json", "dense.gguf", "ple.gguf", "embd.gguf"])
        (model / "pack" / "native_experts.txt").write_text(EXPERTS_TABLE)

        config = self.run_wrapper(
            "--model", model, "--draft-layer", self.draft,
            "--pack", "pack", "--dense-gguf", "dense.gguf", "--lookup-table-gguf", "ple.gguf", "--token-embedding-gguf", "embd.gguf",
            "--expert-vram-mib", "1000000", "--unified-memory",
        )

        self.assertFalse((self.data / "pack").exists())
        self.assertEqual(config["tokenizer"], str(model / "pack" / "tokenizer"))
        self.assertEqual(config["args"], [
            "--pack", str(model / "pack"),
            "--native", str(model / "dense.gguf"),
            "--native-dense-gguf", str(model / "dense.gguf"),
            "--ple-gguf", str(model / "ple.gguf"),
            "--embd-gguf", str(model / "embd.gguf"),
            "--expert-profile", str(self.terminal / "data" / "expert-profile.bin"),
            "--expert-cache", str(2 * 512),
            "--prefill", "auto:32768",
            "--spec", "4",
            "--spec-min-p", "0.5",
            "--mtp", str(self.draft / "mtp"),
            "--mtp-draft-vocab", str(self.terminal / "data" / "draft_vocab.bin"),
            "--max-context", "4096",
            "--kv", "int8",
            "--mmap-experts", "--mtp-q4", "all", "--mtp-hnorm", "stream",
        ])

    def test_a_cap_below_one_expert_block_is_refused(self):
        model = self.files("model", ["IQ2_XS/m-00001-of-00002.gguf", "IQ2_XS/m-00002-of-00002.gguf"])

        with self.assertRaises(SystemExit) as stopped:
            self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "2")

        self.assertEqual(stopped.exception.code, f"dotcore-strata-start: --expert-vram-mib 2 holds no expert block of {BLOB} bytes")

    def files(self, name, paths):
        """Creates empty files under a new folder `name` and returns the folder."""
        folder = self.root / name
        for path in paths:
            (folder / path).parent.mkdir(parents=True, exist_ok=True)
            (folder / path).write_text("")
        return folder

    def run_wrapper(self, *args):
        """Runs the wrapper up to where it starts the server and returns the run configuration it wrote."""
        defaults = ["--model-name", "m", "--context", "4096"]
        if "--arch" not in args:
            defaults += ["--arch", "qwen4exp", "--mtp", "4"]
        argv = ["dotcore-strata-start", *map(str, args), *defaults]
        with mock.patch.object(sys, "argv", argv), mock.patch.object(self.start.os, "execv") as execv:
            self.start.main()
        execv.assert_called_once()
        return json.loads((self.data / "strata.json").read_text())


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=2)
