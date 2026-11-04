//! Verifies that development service controls target the exact workspace Process Compose socket.

import assert from "node:assert/strict";
import { chmod, mkdtemp, mkdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-process-compose-"));
  t.after(() => rm(root, { force: true, recursive: true }));
  return root;
}

test("Process Compose recipes forward the workspace socket and user arguments exactly", async (t) => {
  const root = await fixture(t);
  const bin = path.join(root, "bin");
  await mkdir(bin);
  await writeFile(path.join(bin, "process-compose"), "#!/bin/sh\nprintf 'cwd=<%s>\\n' \"$PWD\"\nprintf 'arg=<%s>\\n' \"$@\"\n");
  await chmod(path.join(bin, "process-compose"), 0o755);
  // A site and the nix frontend mode satisfy the bootstrap checks without depending on this checkout's state.
  const siteDir = path.join(root, "site");
  await mkdir(siteDir);
  await writeFile(path.join(siteDir, "site.yaml"), "");
  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    PC_SOCKET_PATH: "/tmp/dotcore-test.sock",
    DOTCORE_SITE_DIR: siteDir,
    DOTCORE_FRONTEND_RUN_MODE: "nix",
  };
  const run = (...arguments_) => spawnSync("just", ["dev", ...arguments_], { cwd: repositoryRoot, encoding: "utf8", env: environment });

  const foreground = run("up", "--status");
  assert.equal(foreground.status, 0, foreground.stderr);
  assert.equal(foreground.stdout, `cwd=<${repositoryRoot}>\narg=<--config>\narg=<dev/process-compose.yaml>\narg=<--disable-dotenv>\narg=<--use-uds>\narg=<--unix-socket>\narg=</tmp/dotcore-test.sock>\narg=<--status>\n`);

  const client = run("pc", "ps");
  assert.equal(client.status, 0, client.stderr);
  assert.equal(client.stdout, `cwd=<${repositoryRoot}>\narg=<--use-uds>\narg=<--unix-socket>\narg=</tmp/dotcore-test.sock>\narg=<ps>\n`);

  const attach = run("attach");
  assert.equal(attach.status, 0, attach.stderr);
  assert.equal(attach.stdout, `cwd=<${repositoryRoot}>\narg=<--use-uds>\narg=<--unix-socket>\narg=</tmp/dotcore-test.sock>\narg=<attach>\n`);

  const down = run("down");
  assert.equal(down.status, 0, down.stderr);
  assert.equal(down.stdout, `cwd=<${repositoryRoot}>\narg=<--use-uds>\narg=<--unix-socket>\narg=</tmp/dotcore-test.sock>\narg=<down>\n`);

  const logsDefault = run("logs");
  assert.equal(logsDefault.status, 0, logsDefault.stderr);
  assert.equal(logsDefault.stdout, `cwd=<${repositoryRoot}>\narg=<--use-uds>\narg=<--unix-socket>\narg=</tmp/dotcore-test.sock>\narg=<process>\narg=<logs>\narg=<backend>\narg=<--tail>\narg=<200>\narg=<--raw-log>\n`);

  const logsTargeted = run("logs", "frontend", "20");
  assert.equal(logsTargeted.status, 0, logsTargeted.stderr);
  assert.equal(logsTargeted.stdout, `cwd=<${repositoryRoot}>\narg=<--use-uds>\narg=<--unix-socket>\narg=</tmp/dotcore-test.sock>\narg=<process>\narg=<logs>\narg=<frontend>\narg=<--tail>\narg=<20>\narg=<--raw-log>\n`);
});

test("backend run passes through the configured private listener", async (t) => {
  const root = await fixture(t);
  const bin = path.join(root, "bin");
  await mkdir(bin);
  await writeFile(
    path.join(bin, "cargo"),
    "#!/bin/sh\nprintf 'private=<%s>\\n' \"${DOTCORE_PRIVATE_ADDR-unset}\"\n",
  );
  await chmod(path.join(bin, "cargo"), 0o755);
  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    DOTCORE_PRIVATE_ADDR: "127.0.0.1:50053",
  };
  const run = (env) =>
    spawnSync("just", ["backend", "run"], { cwd: repositoryRoot, encoding: "utf8", env });

  const configured = run(environment);
  assert.equal(configured.status, 0, configured.stderr);
  assert.match(configured.stdout, /private=<127\.0\.0\.1:50053>/);

  const unconfigured = run({ ...environment, DOTCORE_PRIVATE_ADDR: undefined });
  assert.equal(unconfigured.status, 0, unconfigured.stderr);
  assert.match(unconfigured.stdout, /private=<unset>/);
});

test("ingress runs on the workspace endpoints in the manager stack only", async (t) => {
  const root = await fixture(t);
  const bin = path.join(root, "bin");
  await mkdir(bin);
  await writeFile(
    path.join(bin, "dotcore-ingress"),
    "#!/bin/sh\nfor v in DOTCORE_MACHINE_HOST DOTCORE_PUBLIC_PORT DOTCORE_MANAGEMENT_UPSTREAM DOTCORE_INFERENCE_UPSTREAM DOTCORE_FRONTEND_UPSTREAM; do printenv \"$v\" | sed \"s/^/$v=/\"; done\nprintf 'arg=<%s>\\n' \"$@\"\n",
  );
  await chmod(path.join(bin, "dotcore-ingress"), 0o755);
  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    DOTCORE_MACHINE_HOST: "100.64.0.9",
    DOTCORE_DEV_PUBLIC_PORT: "21075",
    DOTCORE_MANAGEMENT_ORIGIN: "http://100.64.0.9:21071",
    DOTCORE_INFERENCE_ORIGIN: "http://100.64.0.9:21072",
    DOTCORE_FRONTEND_ORIGIN: "http://100.64.0.9:21070",
  };

  const ingress = spawnSync("just", ["dev", "ingress"], { cwd: repositoryRoot, encoding: "utf8", env: environment });
  assert.equal(ingress.status, 0, ingress.stderr);
  assert.equal(
    ingress.stdout,
    "DOTCORE_MACHINE_HOST=100.64.0.9\nDOTCORE_PUBLIC_PORT=21075\n" +
      "DOTCORE_MANAGEMENT_UPSTREAM=http://100.64.0.9:21071\nDOTCORE_INFERENCE_UPSTREAM=http://100.64.0.9:21072\n" +
      "DOTCORE_FRONTEND_UPSTREAM=http://100.64.0.9:21070\narg=<>\n",
  );

  // A bundle public port inherited from the shell never becomes the development ingress port.
  const inherited = spawnSync("just", ["dev", "ingress"], {
    cwd: repositoryRoot,
    encoding: "utf8",
    env: { ...environment, DOTCORE_DEV_PUBLIC_PORT: undefined, DOTCORE_PUBLIC_PORT: "8080" },
  });
  assert.notEqual(inherited.status, 0);
  assert.match(inherited.stderr, /DOTCORE_DEV_PUBLIC_PORT must be set/);
  assert.equal(inherited.stdout, "");

  // The worker serves only its private listener, so only the manager stack fronts the console.
  const manager = await readFile(path.join(repositoryRoot, "dev", "process-compose.yaml"), "utf8");
  const worker = await readFile(path.join(repositoryRoot, "dev", "cluster", "process-compose-worker.yaml"), "utf8");
  assert.match(manager, /^ {2}ingress:\n {4}command: just dev::ingress$/m);
  assert.doesNotMatch(worker, /ingress/);
});

test("down clears a socket file left behind by a stack that is gone", async (t) => {
  const root = await fixture(t);
  const bin = path.join(root, "bin");
  await mkdir(bin);
  await writeFile(
    path.join(bin, "process-compose"),
    `#!/bin/sh
case "$*" in
  *"project state"*)
    if [ "\${FAKE_PC_PROBE_EXIT:-1}" = 0 ]; then exit 0; fi
    printf '%s\\n' "\${FAKE_PC_PROBE_TEXT:-dial unix socket: connect: connection refused}" >&2
    exit 1 ;;
  *" down"*) printf 'failed to stop project\\n' >&2; exit 1 ;;
esac
exit 0
`,
  );
  await chmod(path.join(bin, "process-compose"), 0o755);
  const socket = path.join(root, "process-compose.sock");
  const run = (extraEnvironment = {}) =>
    spawnSync("just", ["dev", "down"], {
      cwd: repositoryRoot,
      encoding: "utf8",
      env: { ...process.env, PATH: `${bin}:${process.env.PATH}`, PC_SOCKET_PATH: socket, ...extraEnvironment },
    });

  // Nothing answers on the socket file, so the stack counts as stopped and the file goes with it.
  await writeFile(socket, "");
  const stale = run();
  assert.equal(stale.status, 0, stale.stderr);
  assert.match(stale.stdout, /removed the stale socket/);
  assert.doesNotMatch(stale.stderr, /failed to stop project/);
  assert.equal(await exists(socket), false);

  // A stack that answers and still refuses to stop is a failure, and its endpoint stays for a retry.
  await writeFile(socket, "");
  const refusing = run({ FAKE_PC_PROBE_EXIT: "0" });
  assert.equal(refusing.status, 1);
  assert.match(refusing.stderr, /failed to stop project/);
  assert.equal(await exists(socket), true);
  // A client that cannot read the endpoint must not pass for a stopped stack either.
  await writeFile(socket, "");
  const unreadable = run({ FAKE_PC_PROBE_TEXT: 'unknown command "project" for "process-compose"' });
  assert.equal(unreadable.status, 1);
  assert.match(unreadable.stderr, /cannot tell whether a development stack is running/);
  assert.equal(await exists(socket), true);
});

function exists(target) {
  return stat(target).then(() => true, () => false);
}
