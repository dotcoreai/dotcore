import { readFileSync } from "node:fs";
import {
  chmod,
  mkdir,
  mkdtemp,
  readFile,
  readdir,
  rm,
  writeFile,
} from "node:fs/promises";
import http from "node:http";
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
} = imageWorkflowKit("Hermes");

const IMAGE = "dotcore/hermes:dev";
const WORKLOAD = "dotcore-hermes-image-test";
const TIMEOUT_MS = 120_000;
const POLL_MS = 1_000;
const INFERENCE_URL = "http://127.0.0.1:50052/v1";
const ROUTE = "fixture-route-1.5";
// Startup values the wrapper refuses before Hermes runs, as [inference url, model].
const INVALID_STARTUPS = [
  ["", ROUTE],
  ["ftp://127.0.0.1:50052/v1", ROUTE],
  ["http://127.0.0.1:50052/v1/extra", ROUTE],
  ['http://127.0.0.1:50052/"/v1', ROUTE],
  [INFERENCE_URL, "Fixture route"],
];
// The login the catalog sets for the dashboard: deliberate public placeholders, the same for every
// deployment.
const LOGIN_USERNAME = "admin";
const LOGIN_PASSWORD = "dotcore";
const BASE_IMAGE = dockerfileBaseImage(
  readFileSync(new URL("./Dockerfile", import.meta.url), "utf8"),
  /^nousresearch\/hermes-agent@sha256:[a-f0-9]{64}$/,
  "one immutable upstream FROM reference",
);

/** Qualifies the Hermes image on the native Docker platform. */
async function verifyHermesImage() {
  const platform = dockerNativePlatform(TIMEOUT_MS);
  const image = inspectImage(IMAGE, TIMEOUT_MS);
  verifyPlatform("built image", image, platform);
  docker(["pull", "--platform", platform, BASE_IMAGE]);
  const baseImage = inspectImage(BASE_IMAGE, TIMEOUT_MS);
  verifyPlatform("upstream base", baseImage, platform);
  verifyBaseLayers(image, baseImage);

  const versionOutput = docker(["run", "--rm", IMAGE, "--version"]);
  const version = versionOutput
    .split("\n")
    .find((line) => /^Hermes Agent v\S+/.test(line));
  if (!version)
    throw new Error(
      `Hermes command returned no version line:\n${versionOutput}`,
    );

  await verifyStartup();
  return { platform, image: image.Id, version, gateway: await verifyGateway() };
}

/**
 * Verifies the startup wrapper: it writes Hermes' configuration for a linked and an unlinked
 * route while keeping the rest of the data directory, and refuses values it cannot quote safely.
 */
async function verifyStartup() {
  const directory = await mkdtemp(
    path.join(tmpdir(), "dotcore-hermes-startup-"),
  );
  const data = path.join(directory, "data");
  const bin = path.join(directory, "bin");
  try {
    await Promise.all([mkdir(data), mkdir(bin)]);
    await writeFile(path.join(data, "sentinel"), "retained application data\n");
    await writeFile(path.join(data, "config.yaml"), "obsolete configuration\n");
    // A stand-in for Hermes itself, so the wrapper runs to its exec without starting the gateway.
    await writeFile(path.join(bin, "hermes"), "#!/bin/sh\nexit 0\n");
    await chmod(path.join(bin, "hermes"), 0o755);

    const start = (inferenceUrl, model) =>
      runDocker(
        [
          "run",
          "--rm",
          "--entrypoint",
          "/bin/sh",
          "--mount",
          `type=bind,src=${data},dst=/opt/data`,
          "--mount",
          `type=bind,src=${bin},dst=/dotcore-test/bin,readonly`,
          "--env",
          `HERMES_INFERENCE_BASE_URL=${inferenceUrl}`,
          "--env",
          `HERMES_MODEL=${model}`,
          IMAGE,
          "-c",
          "PATH=/dotcore-test/bin:$PATH exec /usr/local/bin/dotcore-hermes-start hermes gateway run",
        ],
        { timeout: TIMEOUT_MS },
      );

    for (const model of [ROUTE, ""]) {
      commandOutput(start(INFERENCE_URL, model));
      await verifyStartupFiles(data, model);
    }
    for (const [inferenceUrl, model] of INVALID_STARTUPS) {
      const result = start(inferenceUrl, model);
      if (result.status !== 64) {
        throw new Error(
          `Hermes startup answered ${JSON.stringify([inferenceUrl, model])} with exit ${result.status}, expected 64.`,
        );
      }
      await verifyStartupFiles(data, "");
    }
  } finally {
    await rm(directory, { force: true, recursive: true });
  }
}

async function verifyStartupFiles(data, model) {
  const [sentinel, config, entries] = await Promise.all([
    readFile(path.join(data, "sentinel"), "utf8"),
    readFile(path.join(data, "config.yaml"), "utf8"),
    readdir(data),
  ]);
  if (
    sentinel !== "retained application data\n" ||
    config !== expectedConfig(model)
  ) {
    throw new Error(
      `Hermes startup did not keep unrelated data and write its configuration:\n${config}`,
    );
  }
  if (entries.some((entry) => entry.startsWith(".config.yaml."))) {
    throw new Error("Hermes startup left a temporary configuration file.");
  }
}

/**
 * Starts the gateway with a linked route and the catalog's login, and checks the dashboard serves,
 * selects Dotcore, and offers login and chat to a client outside the container.
 */
async function verifyGateway() {
  let created = false;
  try {
    docker([
      "run",
      "--detach",
      "--name",
      WORKLOAD,
      "--env",
      `HERMES_INFERENCE_BASE_URL=${INFERENCE_URL}`,
      "--env",
      `HERMES_MODEL=${ROUTE}`,
      "--env",
      "HERMES_DASHBOARD=1",
      "--env",
      "HERMES_DASHBOARD_HOST=0.0.0.0",
      "--env",
      "API_SERVER_ENABLED=false",
      "--env",
      `HERMES_DASHBOARD_BASIC_AUTH_USERNAME=${LOGIN_USERNAME}`,
      "--env",
      `HERMES_DASHBOARD_BASIC_AUTH_PASSWORD=${LOGIN_PASSWORD}`,
      // Published on the host's loopback: the dashboard then sees a non-loopback peer, like a browser.
      "--publish",
      "127.0.0.1::9119",
      IMAGE,
      "/usr/local/bin/dotcore-hermes-start",
      "hermes",
      "gateway",
      "run",
    ]);
    created = true;

    const deadline = Date.now() + TIMEOUT_MS;
    let readiness;
    for (;;) {
      readiness = runDocker(
        ["exec", WORKLOAD, PYTHON, "-c", GATEWAY_READINESS_SCRIPT],
        { timeout: TIMEOUT_MS },
      );
      if (readiness.status === 0) break;
      if (
        docker([
          "inspect",
          "--format",
          "{{.State.Running}}",
          WORKLOAD,
        ]).trim() !== "true" ||
        Date.now() >= deadline
      ) {
        throw new Error(
          `Hermes gateway did not become ready: ${readiness.stderr}\n${docker(["logs", WORKLOAD])}`,
        );
      }
      await sleep(POLL_MS);
    }

    const provider = JSON.parse(
      docker(["exec", WORKLOAD, PYTHON, "-c", MODEL_SELECTION_SCRIPT]),
    );
    const expected = {
      slug: "dotcore",
      model: ROUTE,
      apiUrl: INFERENCE_URL,
      current: true,
      models: [ROUTE],
    };
    if (JSON.stringify(provider) !== JSON.stringify(expected)) {
      throw new Error(
        `Hermes did not select the Dotcore provider and route: ${JSON.stringify(provider)}`,
      );
    }

    const port = publishedPort();
    const chat = await verifyLoginAndChat(port);
    return { readiness: readiness.stdout.trim(), provider, chat };
  } finally {
    if (created) removeWorkload(WORKLOAD, TIMEOUT_MS);
  }
}

/** The host port Docker published for the dashboard. */
function publishedPort() {
  const mapping = docker(["port", WORKLOAD, "9119/tcp"]).trim().split("\n")[0];
  return Number(mapping.slice(mapping.lastIndexOf(":") + 1));
}

/**
 * Checks the dashboard through the published port, as a browser reaches it: readiness answers
 * without a login, a wrong password is refused, the login yields a one-use ticket, and the Chat
 * page's WebSocket upgrades only with that ticket.
 */
async function verifyLoginAndChat(port) {
  const login = (password) =>
    request(port, "POST", "/auth/password-login", {
      body: JSON.stringify({
        provider: "basic",
        username: LOGIN_USERNAME,
        password,
      }),
    });
  const socket = (query, cookie) =>
    request(port, "GET", `/api/pty${query}`, { cookie, upgrade: true });

  const status = await request(port, "GET", "/api/status");
  const refused = await login("wrong-password");
  const accepted = await login(LOGIN_PASSWORD);
  const cookie = (accepted.setCookie ?? []).map((c) => c.split(";")[0]).join("; ");
  const noSessionTicket = await request(port, "POST", "/api/auth/ws-ticket");
  const ticket = await request(port, "POST", "/api/auth/ws-ticket", { cookie });
  const ticketValue = JSON.parse(ticket.body || "{}").ticket ?? "";

  const result = {
    statusWithoutLogin: status.status,
    wrongPassword: refused.status,
    login: accepted.status,
    ticketWithoutSession: noSessionTicket.status,
    ticket: ticket.status,
    socketWithTicket: ticketValue
      ? (await socket(`?ticket=${ticketValue}`, cookie)).status
      : 0,
    socketWithoutTicket: (await socket("", cookie)).status,
  };
  const expected = {
    statusWithoutLogin: 200,
    wrongPassword: 401,
    login: 200,
    ticketWithoutSession: 401,
    ticket: 200,
    socketWithTicket: 101,
    socketWithoutTicket: 403,
  };
  if (JSON.stringify(result) !== JSON.stringify(expected)) {
    throw new Error(
      `Hermes login and chat did not behave as expected: ${JSON.stringify(result)}`,
    );
  }
  return result;
}

/** Sends one HTTP request to the dashboard, or a WebSocket upgrade, and resolves with its status. */
function request(port, method, path, { body, cookie, upgrade } = {}) {
  const headers = {};
  if (body) headers["content-type"] = "application/json";
  if (cookie) headers.cookie = cookie;
  if (upgrade) {
    Object.assign(headers, {
      connection: "Upgrade",
      upgrade: "websocket",
      "sec-websocket-version": "13",
      "sec-websocket-key": "dGhlIHNhbXBsZSBub25jZQ==",
    });
  }
  return new Promise((resolve, reject) => {
    const outgoing = http.request(
      { host: "127.0.0.1", port, method, path, headers, timeout: TIMEOUT_MS },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () =>
          resolve({
            status: response.statusCode,
            setCookie: response.headers["set-cookie"],
            body: Buffer.concat(chunks).toString("utf8"),
          }),
        );
      },
    );
    outgoing.on("upgrade", (response, upgraded) => {
      upgraded.destroy();
      resolve({ status: response.statusCode });
    });
    outgoing.on("error", reject);
    outgoing.end(body);
  });
}

function docker(arguments_) {
  return commandOutput(runDocker(arguments_, { timeout: TIMEOUT_MS }));
}

/** The configuration the wrapper writes for the fixture endpoint and an optional route name. */
function expectedConfig(model) {
  const route = model
    ? `    models:
      - "${model}"
model:
  provider: dotcore
  default: "${model}"
`
    : `model:
  provider: dotcore
`;
  return `gateway:
  api_server:
    enabled: false
auxiliary:
  title_generation:
    enabled: false
providers:
  dotcore:
    base_url: "${INFERENCE_URL}"
    api_key: "dotcore-no-key"
${route}`;
}

const PYTHON = "/opt/hermes/.venv/bin/python3";

const MODEL_SELECTION_SCRIPT = String.raw`
import json
from hermes_cli.inventory import build_models_payload, load_picker_context

payload = build_models_payload(load_picker_context(), max_models=50)
row = next((entry for entry in payload["providers"] if entry["slug"] == "dotcore"), {})
print(json.dumps({
    "slug": row.get("slug", ""),
    "model": payload["model"],
    "apiUrl": row.get("api_url", ""),
    "current": row.get("is_current", False),
    "models": row.get("models", []),
}))
`;

const GATEWAY_READINESS_SCRIPT = String.raw`
import json
import os
import pathlib
import urllib.request

state = json.loads((pathlib.Path("/opt/data") / "gateway_state.json").read_text())
if state.get("gateway_state") != "running":
    raise SystemExit(f"gateway state is {state.get('gateway_state')!r}")
os.kill(int(state["pid"]), 0)
with urllib.request.urlopen("http://127.0.0.1:9119/api/status", timeout=2) as response:
    if response.status != 200:
        raise SystemExit(f"dashboard HTTP status is {response.status}, expected 200")
print(f"gateway running, pid {state['pid']}, dashboard 200")
`;

console.log(JSON.stringify(await verifyHermesImage(), null, 2));
