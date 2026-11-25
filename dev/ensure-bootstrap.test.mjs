//! Verifies that recipes bootstrap the frontend and backend on demand, once, and never under a running stack.

import assert from "node:assert/strict";
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawn, spawnSync } from "node:child_process";
import test from "node:test";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** Builds a minimal repository with the real Justfiles and scripts, and fakes for `npm`, `dotc` and `process-compose`. */
async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-ensure-bootstrap-"));
  t.after(() => rm(root, { force: true, recursive: true }));

  for (const area of ["frontend", "backend", "dev"]) {
    await mkdir(path.join(root, area));
    await copyFile(path.join(repositoryRoot, area, "Justfile"), path.join(root, area, "Justfile"));
  }
  for (const script of ["ensure-bootstrap.sh", "pc-endpoint.sh", "site-choices.sh"]) {
    await copyFile(path.join(repositoryRoot, "dev", script), path.join(root, "dev", script));
  }
  // The dev module declares these submodules; empty ones are enough to load it.
  for (const submodule of ["remote", "cluster"]) {
    await mkdir(path.join(root, "dev", submodule));
    await writeFile(path.join(root, "dev", submodule, "Justfile"), "");
  }
  await writeFile(
    path.join(root, "Justfile"),
    "mod frontend 'frontend/Justfile'\nmod backend 'backend/Justfile'\nmod dev 'dev/Justfile'\n",
  );
  await writeFile(path.join(root, "frontend", "package-lock.json"), "lock-v1\n");

  const bin = path.join(root, "bin");
  const calls = path.join(root, "calls.log");
  const running = path.join(root, "stack-running");
  const siteDir = path.join(root, "site");
  await mkdir(bin);
  const fake = async (name, body) => {
    await writeFile(path.join(bin, name), `#!/bin/sh\n${body}\n`);
    await chmod(path.join(bin, name), 0o755);
  };
  // `npm ci` takes a moment so that concurrent callers overlap.
  await fake("npm", `printf 'npm %s\\n' "$*" >> "${calls}"\nif [ "$1" = ci ]; then sleep 0.3; mkdir -p node_modules; fi`);
  await fake("dotc", `printf 'dotc %s\\n' "$*" >> "${calls}"\nmkdir -p "${siteDir}"\n: > "${siteDir}/site.yaml"`);
  // A stack answers the state probe only while the marker file exists; starting one is recorded.
  await fake(
    "process-compose",
    `case "$*" in\n  *"project state"*) [ -e "${running}" ] || { echo "connect: no such file or directory" >&2; exit 1; } ;;\n  *) printf 'started\\n' >> "${calls}" ;;\nesac`,
  );

  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    PC_SOCKET_PATH: path.join(root, "pc.sock"),
    DOTCORE_SITE_DIR: siteDir,
    DOTCORE_CATALOG_DIR: path.join(root, "catalog"),
    DOTCORE_MACHINE_HOST: "192.0.2.44",
    DOTCORE_FRONTEND_RUN_MODE: "",
  };
  const run = (recipe) => spawnSync("just", recipe, { cwd: root, encoding: "utf8", env: environment });
  const runAsync = (recipe) =>
    new Promise((resolve) => {
      const child = spawn("just", recipe, { cwd: root, env: environment });
      let stderr = "";
      child.stderr.on("data", (chunk) => (stderr += chunk));
      child.on("close", (status) => resolve({ status, stderr }));
    });
  const bootstrapFrontend = async (lock = "lock-v1\n") => {
    await mkdir(path.join(root, "frontend", "node_modules"), { recursive: true });
    await writeFile(path.join(root, "frontend", "node_modules", ".dotcore-lock"), lock);
  };
  const bootstrapBackend = async () => {
    await mkdir(siteDir, { recursive: true });
    await writeFile(path.join(siteDir, "site.yaml"), "");
  };
  const startStack = () => writeFile(running, "");
  const recorded = async () => (await readFile(calls, "utf8").catch(() => "")).split("\n").filter(Boolean);

  return { root, run, runAsync, bootstrapFrontend, bootstrapBackend, startStack, recorded };
}

const frontendInstall = ["npm ci"];
const siteInit = [
  "dotc site init --site-dir SITE --catalog-dir CATALOG --host 192.0.2.44 --inference simulated --applications simulated --non-interactive",
];

/** Keeps the lines the bootstrap step prints, dropping the recipe command lines just echoes. */
const announcements = (stderr) => stderr.split("\n").filter((line) => line.includes("bootstrapped"));

/** Replaces the fixture's paths in recorded calls with placeholders. */
const normalized = (root, lines) => lines.map((line) => line.replace(path.join(root, "site"), "SITE").replace(path.join(root, "catalog"), "CATALOG"));

test("up bootstraps what is missing, announces it, then starts the stack", async (t) => {
  const { root, run, recorded } = await fixture(t);

  const result = run(["dev", "up"]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(
    announcements(result.stderr),
    ["frontend is not bootstrapped (dependencies missing or outdated); bootstrapping it now",
      `backend is not bootstrapped (no development site at ${path.join(root, "site")}); bootstrapping it now`],
  );
  assert.deepEqual(normalized(root, await recorded()), [...frontendInstall, ...siteInit, "started"]);
  assert.equal(await readFile(path.join(root, "frontend", "node_modules", ".dotcore-lock"), "utf8"), "lock-v1\n");
});

test("up starts silently when both are already bootstrapped", async (t) => {
  const { run, bootstrapFrontend, bootstrapBackend, recorded } = await fixture(t);
  await bootstrapFrontend();
  await bootstrapBackend();

  const result = run(["dev", "up"]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(announcements(result.stderr), []);
  assert.deepEqual(await recorded(), ["started"]);
});

test("a changed lockfile reinstalls the frontend dependencies when no stack runs", async (t) => {
  const { root, run, bootstrapFrontend, bootstrapBackend, recorded } = await fixture(t);
  await bootstrapFrontend();
  await bootstrapBackend();
  await writeFile(path.join(root, "frontend", "package-lock.json"), "lock-v2\n");

  const result = run(["frontend", "lint"]);
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(await recorded(), ["npm ci", "npm run lint"]);
  assert.equal(await readFile(path.join(root, "frontend", "node_modules", ".dotcore-lock"), "utf8"), "lock-v2\n");
});

test("a changed lockfile is not reinstalled under a running stack", async (t) => {
  const { root, run, bootstrapFrontend, startStack, recorded } = await fixture(t);
  await bootstrapFrontend();
  await startStack();
  await writeFile(path.join(root, "frontend", "package-lock.json"), "lock-v2\n");

  const result = run(["frontend", "lint"]);
  assert.equal(result.status, 1);
  assert.equal(result.stderr, "frontend dependencies are outdated and the development stack is running; run `just dev::down`, then retry\n");
  assert.deepEqual(await recorded(), []);
});

test("concurrent recipes install the frontend dependencies once", async (t) => {
  const { runAsync, recorded } = await fixture(t);

  const results = await Promise.all([runAsync(["frontend", "lint"]), runAsync(["frontend", "typecheck"])]);
  assert.deepEqual(results.map((result) => result.status), [0, 0]);
  assert.deepEqual([...(await recorded())].sort(), ["npm ci", "npm run lint", "npm run typecheck"]);
});
