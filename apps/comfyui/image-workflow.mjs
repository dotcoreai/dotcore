import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import {
  mkdir,
  mkdtemp,
  readdir,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { setTimeout as sleep } from "node:timers/promises";

import { imageWorkflowKit } from "../../nix/docker-images/image-workflow.mjs";

const {
  commandOutput,
  dockerNativePlatform,
  dockerfileBaseImage,
  inspectImage,
  removeWorkload,
  runDocker,
  verifyBaseLayers,
  verifyPlatform,
} = imageWorkflowKit("ComfyUI");

const IMAGE = "dotcore/comfyui:dev";
const WORKLOAD = "dotcore-comfyui-image-test";
const SERVICE_PORT = 8188;
const BASE_DIRECTORY = "/data";
const TIMEOUT_MS = 300_000;
const POLL_MS = 1_000;
const EXTRA_PATHS_CONFIG = "/opt/comfyui/extra-model-paths.yaml";
const DOWNLOAD_NODE = "/opt/comfyui/custom_nodes/dotcore_model_downloads";
const DOWNLOAD_SCRIPT =
  "/extensions/dotcore_model_downloads/model-downloads.js";
const DOWNLOADS_ROUTE = "/api/dotcore/model-downloads";
// A small public file, pinned by commit so its bytes never change.
const MODEL_URL =
  "https://huggingface.co/hf-internal-testing/tiny-random-gpt2/resolve/71034c5d8bde858ff824298bdedc65515b97d2b9/model.safetensors";
const MODEL_SHA256 =
  "8111d5afb0715dbf5a31396d31432cb56370ba23f6650a035ea0fc8a20b4e500";
// The diffusion models key lists the legacy unet folder first, yet downloads land in the folder
// the workflow names.
const DOWNLOAD_TARGETS = [
  { directory: "loras", name: "tiny.safetensors" },
  { directory: "diffusion_models", name: "tiny.safetensors" },
];
const FIXTURE = path.posix.join("models", "checkpoints", "fixture.safetensors");
// A temporary file an interrupted download left behind, which the node removes as it loads.
const LEFTOVER_PART = path.posix.join(
  "models",
  "checkpoints",
  ".interrupted.safetensors.k2x9.part",
);
// Requests the node must refuse, each one change away from an allowed download.
const REFUSALS = [
  {
    request: {
      url: "https://github.com/comfyanonymous/ComfyUI/raw/master/x.safetensors",
      directory: "checkpoints",
      name: "x.safetensors",
    },
    status: 400,
    reason: /not from an allowed source/,
  },
  {
    request: {
      url: MODEL_URL,
      directory: "checkpoints",
      name: "../../custom_nodes/x.safetensors",
    },
    status: 400,
    reason: /not a plain file name/,
  },
  {
    request: {
      url: MODEL_URL,
      directory: "custom_nodes",
      name: "x.safetensors",
    },
    status: 400,
    reason: /custom_nodes is not a model folder/,
  },
  {
    request: {
      url: MODEL_URL,
      directory: "checkpoints",
      name: "fixture.safetensors",
    },
    status: 409,
    reason: /already exists/,
  },
  {
    // A browser sends a cross-site form post without a preflight; only JSON is accepted.
    request: {
      url: MODEL_URL,
      directory: "checkpoints",
      name: "form.safetensors",
    },
    contentType: "text/plain",
    status: 415,
    reason: /the request must be JSON/,
  },
];

const BASE_IMAGE = dockerfileBaseImage(
  readFileSync(new URL("./Dockerfile", import.meta.url), "utf8"),
  /^nvidia\/cuda:13\.[0-9]+\.[0-9]+-runtime-ubuntu24\.04@sha256:[a-f0-9]{64}$/,
  "one immutable CUDA 13 runtime base reference",
);
const NODE_SOURCE = readFileSync(
  new URL(
    "./custom_nodes/dotcore_model_downloads/__init__.py",
    import.meta.url,
  ),
  "utf8",
);

/** Qualifies the ComfyUI image on the native Docker platform without a device. */
async function verifyComfyUiImage() {
  const platform = dockerNativePlatform(TIMEOUT_MS);
  const image = inspectImage(IMAGE, TIMEOUT_MS);
  verifyPlatform("built image", image, platform);
  docker(["pull", "--platform", platform, BASE_IMAGE]);
  const baseImage = inspectImage(BASE_IMAGE, TIMEOUT_MS);
  verifyPlatform("upstream base", baseImage, platform);
  verifyBaseLayers(image, baseImage);

  const directory = await mkdtemp(path.join(tmpdir(), "dotcore-comfyui-"));
  const storage = path.join(directory, "storage");
  let created = false;
  try {
    await mkdir(path.join(storage, "models", "checkpoints"), {
      recursive: true,
    });
    await writeFile(path.join(storage, FIXTURE), "fixture checkpoint\n");
    await writeFile(path.join(storage, LEFTOVER_PART), "partial\n");

    // No device is passed: this qualifies the CPU-only path. The image runs as root, as in
    // production, so the storage is handed back before it is removed.
    docker([
      "run",
      "--detach",
      "--name",
      WORKLOAD,
      "--publish",
      `127.0.0.1::${SERVICE_PORT}`,
      "--mount",
      `type=bind,src=${storage},dst=${BASE_DIRECTORY}`,
      IMAGE,
    ]);
    created = true;
    const base = `http://127.0.0.1:${publishedPort()}`;
    const stats = await readSystemStats(base);

    if (stats.devices?.length !== 1 || stats.devices[0].type !== "cpu") {
      throw new Error(
        `ComfyUI started without a device but reported ${JSON.stringify(stats.devices)}.`,
      );
    }
    const argv = stats.system.argv ?? [];
    for (const [flag, value] of [
      ["--base-directory", BASE_DIRECTORY],
      ["--extra-model-paths-config", EXTRA_PATHS_CONFIG],
    ]) {
      if (argv[argv.indexOf(flag) + 1] !== value) {
        throw new Error(
          `ComfyUI was not started with ${flag} ${value}: ${JSON.stringify(argv)}`,
        );
      }
    }
    if (!argv.includes("--cpu")) {
      throw new Error(
        `ComfyUI started without a device but without --cpu: ${JSON.stringify(argv)}`,
      );
    }

    const torch = JSON.parse(
      docker(["exec", WORKLOAD, "python", "-c", TORCH_PROBE]),
    );
    if (
      Number.parseInt(torch.cuda, 10) < 13 ||
      !torch.architectures.includes("sm_120")
    ) {
      throw new Error(
        `ComfyUI torch needs CUDA 13 with sm_120 kernels, observed ${JSON.stringify(torch)}.`,
      );
    }

    const downloads = await verifyModelDownloads(base, storage);
    return {
      platform,
      image: image.Id,
      comfyuiVersion: stats.system.comfyui_version,
      torch,
      ...downloads,
    };
  } finally {
    if (created) removeWorkload(WORKLOAD, TIMEOUT_MS);
    runDocker(
      [
        "run",
        "--rm",
        "--entrypoint",
        "chown",
        "--mount",
        `type=bind,src=${storage},dst=${BASE_DIRECTORY}`,
        IMAGE,
        "--recursive",
        `${process.getuid()}:${process.getgid()}`,
        BASE_DIRECTORY,
      ],
      { timeout: TIMEOUT_MS },
    );
    await rm(directory, { force: true, recursive: true });
  }
}

/**
 * Verifies the bundled model download node: it loads from the image and cleans leftovers, the
 * pinned frontend still hands the missing-models dialog's downloads to it with the same allowed
 * sources, a public file lands in the named model folder, and unsafe requests are refused.
 */
async function verifyModelDownloads(base, storage) {
  // ComfyUI logs its custom-node imports on stderr.
  const logged = runDocker(["logs", WORKLOAD], { timeout: TIMEOUT_MS });
  if (
    !(commandOutput(logged) + logged.stderr).includes(
      `seconds: ${DOWNLOAD_NODE}`,
    )
  ) {
    throw new Error(
      `ComfyUI did not import the download node from ${DOWNLOAD_NODE}.`,
    );
  }
  const extensions = await request(`${base}/extensions`);
  if (!parseJson(extensions.body)?.includes(DOWNLOAD_SCRIPT)) {
    throw new Error(
      `ComfyUI does not serve the download node's editor script ${DOWNLOAD_SCRIPT}.`,
    );
  }
  if (await exists(path.join(storage, LEFTOVER_PART))) {
    throw new Error(
      `ComfyUI download node kept the leftover temporary file ${LEFTOVER_PART}.`,
    );
  }

  const frontend = verifyFrontendHook();

  const downloaded = [];
  for (const { directory, name } of DOWNLOAD_TARGETS) {
    const id = `${directory}/${name}`;
    const started = await request(`${base}${DOWNLOADS_ROUTE}`, {
      body: { url: MODEL_URL, directory, name },
    });
    if (started.status !== 202) {
      throw new Error(
        `ComfyUI download node refused ${id}: ${started.status} ${started.body}`,
      );
    }
    const download = await poll(`download of ${id}`, async () => {
      const listed = await request(`${base}${DOWNLOADS_ROUTE}`);
      const entry = parseJson(listed.body)?.downloads?.find(
        (candidate) => candidate.id === id,
      );
      return entry?.state === "downloading" ? undefined : entry;
    });
    if (download?.state !== "done") {
      throw new Error(
        `ComfyUI download of ${id} ended ${JSON.stringify(download)}.`,
      );
    }
    const file = path.join(storage, "models", directory, name);
    const sha256 = createHash("sha256")
      .update(await readFile(file))
      .digest("hex");
    if (sha256 !== MODEL_SHA256) {
      throw new Error(
        `ComfyUI download ${id} holds sha256 ${sha256}, the source serves ${MODEL_SHA256}.`,
      );
    }
    downloaded.push(id);
  }

  const before = await listFiles(storage);
  for (const { request: body, contentType, status, reason } of REFUSALS) {
    const reply = await request(`${base}${DOWNLOADS_ROUTE}`, {
      body,
      contentType,
    });
    if (
      reply.status !== status ||
      !reason.test(parseJson(reply.body)?.error ?? "")
    ) {
      throw new Error(
        `ComfyUI download node answered ${JSON.stringify(body)} with ${reply.status} ${reply.body}, expected ${status} matching ${reason}.`,
      );
    }
  }
  const after = await listFiles(storage);
  if (after.join() !== before.join()) {
    throw new Error(
      `ComfyUI download node refusals changed the storage: ${after.join(", ")}`,
    );
  }

  return { downloaded, refused: REFUSALS.length, frontend };
}

/**
 * The editor's missing-models dialog downloads through the desktop app's bridge when a local one
 * is present, and the node's editor script provides it. A frontend bump that drops the call or
 * changes the sources the dialog allows fails here rather than in the editor.
 */
function verifyFrontendHook() {
  const probe = JSON.parse(
    docker(["exec", WORKLOAD, "python", "-c", FRONTEND_HOOK_PROBE]),
  );
  if (!probe.bridgeCall || !probe.remoteGate) {
    throw new Error(
      `ComfyUI frontend no longer hands missing-model downloads to window.__comfyDesktop2.downloadModel: ${JSON.stringify(probe)}`,
    );
  }
  // The frontend's exact-url list also names its own development server, which the node omits.
  const urls = probe.urls.filter((url) => !url.startsWith("http://localhost:"));
  for (const [name, frontend] of [
    ["ALLOWED_SOURCES", probe.sources],
    ["ALLOWED_SUFFIXES", probe.suffixes],
    ["ALLOWED_URLS", urls],
  ]) {
    const node = pythonStrings(name);
    if (JSON.stringify(frontend) !== JSON.stringify(node)) {
      throw new Error(
        `ComfyUI frontend allows ${JSON.stringify(frontend)}, the download node's ${name} ${JSON.stringify(node)}.`,
      );
    }
  }
  return { sources: probe.sources, suffixes: probe.suffixes, urls };
}

async function readSystemStats(base) {
  try {
    return await poll("readiness", async () => {
      // A container that has not bound its port yet resets the connection.
      const stats = parseJson(
        (await request(`${base}/system_stats`).catch(() => ({ body: "" })))
          .body,
      );
      if (stats?.system?.comfyui_version) return stats;
      if (
        docker([
          "inspect",
          "--format",
          "{{.State.Running}}",
          WORKLOAD,
        ]).trim() !== "true"
      ) {
        throw new Error("ComfyUI exited before readiness.");
      }
      return undefined;
    });
  } catch (error) {
    const logs = runDocker(["logs", WORKLOAD], { timeout: TIMEOUT_MS });
    throw new Error(`${error.message}\n${logs.stdout}${logs.stderr}`);
  }
}

/** Calls `check` until it returns a value, failing after the workflow timeout. */
async function poll(subject, check) {
  const deadline = Date.now() + TIMEOUT_MS;
  for (;;) {
    const value = await check();
    if (value !== undefined) return value;
    if (Date.now() >= deadline)
      throw new Error(
        `ComfyUI ${subject} did not finish within ${TIMEOUT_MS}ms.`,
      );
    await sleep(POLL_MS);
  }
}

function docker(arguments_) {
  return commandOutput(runDocker(arguments_, { timeout: TIMEOUT_MS }));
}

function publishedPort() {
  return docker(["port", WORKLOAD, `${SERVICE_PORT}/tcp`])
    .trim()
    .split(":")
    .at(-1);
}

async function request(url, { body, contentType = "application/json" } = {}) {
  const init =
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: { "content-type": contentType },
          body: JSON.stringify(body),
        };
  const response = await fetch(url, {
    ...init,
    signal: AbortSignal.timeout(5_000),
  });
  return { status: response.status, body: await response.text() };
}

/** Lists the storage's files outside ComfyUI's own user state, which it writes as it serves. */
async function listFiles(storage) {
  const entries = await readdir(storage, {
    recursive: true,
    withFileTypes: true,
  });
  return entries
    .filter((entry) => entry.isFile())
    .map((entry) =>
      path.relative(storage, path.join(entry.parentPath, entry.name)),
    )
    .filter((file) => file.split(path.sep)[0] !== "user")
    .sort();
}

/** Reads the string literals of one tuple constant from the download node's Python source. */
function pythonStrings(name) {
  const body = new RegExp(`^${name} = \\(([^)]*)\\)`, "m").exec(
    NODE_SOURCE,
  )?.[1];
  if (!body)
    throw new Error(`ComfyUI download node declares no ${name} tuple.`);
  return [...body.matchAll(/"([^"]*)"/g)].map((match) => match[1]);
}

function parseJson(body) {
  try {
    return JSON.parse(body);
  } catch {
    return null;
  }
}

function exists(file) {
  return stat(file).then(
    () => true,
    () => false,
  );
}

// Finds the missing-models dialog's download function in the frontend bundle the image serves:
// whether it calls the bridge with the model's url, name and folder, whether a local bridge is
// what selects that call, and the source, suffix and exact-url lists it allows.
const FRONTEND_HOOK_PROBE = String.raw`
import json
import pathlib
import re

import comfyui_frontend_package

assets = pathlib.Path(comfyui_frontend_package.__file__).parent / "static" / "assets"
probe = {"bridgeCall": False, "remoteGate": False, "sources": [], "suffixes": [], "urls": []}
for script in sorted(assets.glob("*.js")):
    text = script.read_text(errors="replace")
    if not re.search(r"\.downloadModel\?\.\((\w+)\.url,\1\.name,\1\.directory\)", text):
        continue
    probe["bridgeCall"] = True
    probe["remoteGate"] = bool(re.search(
        r"=window\.__comfyDesktop2,(\w+)=\w+\?\.isRemote\?\.\(\).{0,80}?if\(\w+\?\.downloadModel&&!\1\)",
        text,
    ))
    # The bundle quotes these lists with backticks, which this template cannot hold literally.
    lists = re.search(
        r"\[((?:\x60https://[^\x60]*\x60,?)+)\],\w+=\[((?:\x60\.\w+\x60,?)+)\],\w+=new Set\(\[((?:\x60[^\x60]*\x60,?)+)\]\)",
        text,
    )
    if lists:
        probe["sources"] = re.findall(r"\x60([^\x60]*)\x60", lists.group(1))
        probe["suffixes"] = re.findall(r"\x60([^\x60]*)\x60", lists.group(2))
        probe["urls"] = re.findall(r"\x60([^\x60]*)\x60", lists.group(3))
print(json.dumps(probe))
`;

// Torch reports its compiled architectures from the wheel alone when no driver is present.
const TORCH_PROBE = String.raw`
import json
import torch

architectures = []
try:
    architectures = list(torch.cuda.get_arch_list())
except Exception:
    pass
if not architectures:
    architectures = (torch._C._cuda_getArchFlags() or "").split()
print(json.dumps({"torch": torch.__version__, "cuda": torch.version.cuda or "", "architectures": architectures}))
`;

console.log(JSON.stringify(await verifyComfyUiImage(), null, 2));
