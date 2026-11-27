"""Tests the Strata start wrapper's engine paths: the run configuration each model file layout gets.

Run it with the image's Python and the wrapper's path, as `just image-test` does. Each test lays out
fake model files in a temporary folder and stops the wrapper where it would start the server. Upstream's
tools are fakes, except for the image's Inkling tokenizer converter, which one test runs.
"""

import contextlib
import importlib.machinery
import io
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
IMAGE_TERMINAL = Path("/opt/strata/terminal")
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
# A tokenizer builder standing in for upstream's Inkling one: it writes the folder and counts its runs.
FAKE_INKLING_TOKENIZER = """
import pathlib, sys
out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1])
out.mkdir(parents=True)
(out / "vocab.json").write_text("{}")
with open(out.parents[1] / "tokenizer-runs", "a") as runs:
    runs.write(" ".join(sys.argv[1:]) + "\\n")
"""
# The smallest tokenizer.json upstream's Inkling converter accepts: its o200k pre-tokenizer and one token.
INKLING_TOKENIZER_JSON = json.dumps({
    "pre_tokenizer": {"type": "Sequence", "pretokenizers": [
        {"type": "Split", "pattern": {"Regex": r"\p{N}{1,3}"}, "behavior": "Isolated"},
        {"type": "ByteLevel", "add_prefix_space": False, "use_regex": False},
    ]},
    "model": {"type": "BPE", "vocab": {"a": 0}, "merges": []},
    "added_tokens": [{"id": 1, "content": "<|endoftext|>", "special": True}],
})


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
        (self.terminal / "tools" / "inkling_tokenizer.py").write_text(FAKE_INKLING_TOKENIZER)
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

    def test_a_host_memory_budget_pins_the_hottest_experts_and_maps_the_rest(self):
        model = self.files("model", ["IQ2_XS/m-00001-of-00002.gguf", "IQ2_XS/m-00002-of-00002.gguf"])

        budget = self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "100", "--expert-host-memory-mib", "1536")
        mapped = self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "100", "--expert-host-memory-mib", "0")

        self.assertEqual(budget["args"][-2:], ["--resident-budget-gib", "1.5"])
        self.assertEqual(mapped["args"][-1:], ["--mmap-experts"])
        self.assertEqual(budget["args"][:-2], mapped["args"][:-1])
        self.assertEqual(mapped["args"][-3:-1], ["--kv", "int8"])

    def test_a_host_memory_budget_is_refused_where_the_engine_path_takes_none(self):
        gguf = self.files("single", ["model.gguf"]) / "model.gguf"
        split = self.files("model", ["IQ2_XS/m-00001-of-00002.gguf", "IQ2_XS/m-00002-of-00002.gguf"])
        cases = [
            (["--model", gguf, "--arch", "qwen35moe", "--mtp", "2"],
             "--pack, its GGUF options, --unified-memory and --expert-host-memory-mib need --draft-layer"),
            (["--model", split, "--draft-layer", self.draft, "--unified-memory"],
             "--expert-host-memory-mib can't be used with --unified-memory, where the GPU's cache already holds the experts in host memory"),
        ]
        for args, message in cases:
            stderr = io.StringIO()
            with self.subTest(message=message), contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as stopped:
                self.run_wrapper(*args, "--expert-vram-mib", "100", "--expert-host-memory-mib", "1024")
            self.assertEqual(stopped.exception.code, 2)
            self.assertIn(f"error: {message}", stderr.getvalue())

    def test_a_cap_below_one_expert_block_is_refused(self):
        model = self.files("model", ["IQ2_XS/m-00001-of-00002.gguf", "IQ2_XS/m-00002-of-00002.gguf"])

        with self.assertRaises(SystemExit) as stopped:
            self.run_wrapper("--model", model, "--draft-layer", self.draft, "--expert-vram-mib", "2")

        self.assertEqual(stopped.exception.code, f"dotcore-strata-start: --expert-vram-mib 2 holds no expert block of {BLOB} bytes")

    def test_inkling_pack_runs_the_inkling_engine_on_the_pack_in_place(self):
        model = self.files("model", ["SQ-10.strata", "tokenizer.json", "chat_template.jinja"])

        config = self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata", "--parallel", "2")
        self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata", "--parallel", "2")

        tokenizer = self.data / "tokenizer" / "tokenizer"
        self.assertEqual(config, {
            "exe": "component:strata-inkling",
            "args": ["--native", str(model / "SQ-10.strata"), "--max-context", "4096", "--kv", "fp16", "--kv-offload", "off"],
            "cwd": str(self.terminal),
            "tokenizer": str(tokenizer),
            "model_name": "m",
            "arch": "inkling2_loop",
            "log": str(self.data / "strata-engine.log"),
            "parallel": 2,
            "drafting": "dspark",
            "env": {"STRATA_INK_OOM_RETRY": "0"},
        })
        self.assertEqual(
            (self.data / "tokenizer-runs").read_text(),
            f"--hf-json {model / 'tokenizer.json'} --template {model / 'chat_template.jinja'} --out {tokenizer}\n",
        )

        (model / "chat_template.jinja").write_text("changed")
        self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata")
        self.assertEqual(len((self.data / "tokenizer-runs").read_text().splitlines()), 2)

    def test_inkling_pack_takes_the_kv_cache_type_and_a_vision_file(self):
        model = self.files("model", ["SQ-10.strata", "tokenizer.json", "chat_template.jinja"])
        vision = self.files("vision", ["Vision.strata"]) / "Vision.strata"

        config = self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata", "--kv", "fp8", "--vision", vision)

        self.assertEqual(config["args"], [
            "--native", str(model / "SQ-10.strata"), "--max-context", "4096", "--kv", "fp8", "--kv-offload", "off",
            "--vision", str(vision),
        ])

    def test_kv_cache_type_and_vision_are_checked(self):
        model = self.files("model", ["SQ-10.strata", "tokenizer.json", "chat_template.jinja"])

        for args in (["--kv", "k8v4"], ["--vision", model / "missing.strata"]):
            with self.subTest(args=args), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as stopped:
                self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata", *args)
            self.assertEqual(stopped.exception.code, 2)

        gguf = self.files("gguf", ["m.gguf"]) / "m.gguf"
        with contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as stopped:
            self.run_wrapper("--model", gguf, "--arch", "qwen35moe", "--mtp", "2", "--kv", "fp8")
        self.assertEqual(stopped.exception.code, 2)

    def test_the_image_builds_an_inkling_tokenizer_from_its_files(self):
        model = self.files("model", ["SQ-10.strata"])
        (model / "tokenizer.json").write_text(INKLING_TOKENIZER_JSON)
        (model / "chat_template.jinja").write_text("{{ messages }}")
        self.start.TERMINAL = IMAGE_TERMINAL

        config = self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata")

        tokenizer = Path(config["tokenizer"])
        self.assertEqual(json.loads((tokenizer / "vocab.json").read_text()), {"a": 0, "<|endoftext|>": 1})
        self.assertEqual((tokenizer / "chat_template.jinja").read_text(), "{{ messages }}")

    def test_an_inkling_pack_without_its_tokenizer_files_is_refused(self):
        model = self.files("model", ["SQ-10.strata", "tokenizer.json"])

        with self.assertRaises(SystemExit) as stopped:
            self.run_wrapper("--model", model, "--inkling-pack", "SQ-10.strata")

        self.assertEqual(stopped.exception.code, f"dotcore-strata-start: the Inkling pack has no {model / 'chat_template.jinja'}")

    def test_the_engine_path_is_explicit(self):
        model = self.files("model", ["SQ-10.strata", "tokenizer.json", "chat_template.jinja"])
        gguf = self.files("single", ["model.gguf"]) / "model.gguf"
        inkling = ["--model", model, "--inkling-pack", "SQ-10.strata"]
        main_engine_refusal = ("--inkling-pack runs Strata's Inkling engine, which takes no --draft-layer, --pack or its GGUF "
                               "options, --arch, --mtp, --unified-memory or expert options")
        cases = [
            (inkling + ["--arch", "qwen35moe"], main_engine_refusal),
            (inkling + ["--mtp", "0"], main_engine_refusal),
            (inkling + ["--draft-layer", self.draft], main_engine_refusal),
            (inkling + ["--expert-vram-mib", "100"], main_engine_refusal),
            (inkling + ["--expert-host-memory-mib", "0"], main_engine_refusal),
            (inkling + ["--unified-memory"], main_engine_refusal),
            (inkling + ["--pack", "pack"], main_engine_refusal),
            (["--model", model, "--inkling-pack", "SQ-10.gguf"], "--inkling-pack SQ-10.gguf is not a .strata file within --model"),
            (["--model", model, "--inkling-pack", "../SQ-10.strata"], "--inkling-pack ../SQ-10.strata is not a .strata file within --model"),
            (["--model", gguf, "--inkling-pack", "SQ-10.strata"], f"--model {gguf} is not a folder"),
            (["--model", gguf, "--arch", "qwen35moe"], "--arch and --mtp are needed without --inkling-pack"),
        ]
        for args, message in cases:
            stderr = io.StringIO()
            with self.subTest(message=message, args=args), contextlib.redirect_stderr(stderr), self.assertRaises(SystemExit) as stopped:
                self.run_wrapper(*args)
            self.assertEqual(stopped.exception.code, 2)
            self.assertIn(f"error: {message}", stderr.getvalue())

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
        if not {"--arch", "--mtp", "--inkling-pack"} & set(map(str, args)):
            defaults += ["--arch", "qwen4exp", "--mtp", "4"]
        argv = ["dotcore-strata-start", *map(str, args), *defaults]
        with mock.patch.object(sys, "argv", argv), mock.patch.object(self.start.os, "execv") as execv:
            self.start.main()
        execv.assert_called_once()
        return json.loads((self.data / "strata.json").read_text())


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1], verbosity=2)
