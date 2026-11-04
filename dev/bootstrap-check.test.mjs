//! Verifies that the stack refuses to start until the frontend and backend are bootstrapped.

import assert from "node:assert/strict";
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

/** Builds a minimal repository with the real frontend, backend and dev Justfiles and a recording `process-compose`. */
async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-bootstrap-check-"));
  t.after(() => rm(root, { force: true, recursive: true }));

  for (const area of ["frontend", "backend", "dev"]) {
    await mkdir(path.join(root, area));
    await copyFile(path.join(repositoryRoot, area, "Justfile"), path.join(root, area, "Justfile"));
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
  const calls = path.join(root, "process-compose.log");
  await mkdir(bin);
  await writeFile(path.join(bin, "process-compose"), `#!/bin/sh\nprintf 'started\\n' >> "${calls}"\n`);
  await chmod(path.join(bin, "process-compose"), 0o755);

  const siteDir = path.join(root, "site");
  const run = (recipe, extraEnvironment = {}) =>
    spawnSync("just", recipe, {
      cwd: root,
      encoding: "utf8",
      env: {
        ...process.env,
        PATH: `${bin}:${process.env.PATH}`,
        PC_SOCKET_PATH: path.join(root, "pc.sock"),
        DOTCORE_SITE_DIR: siteDir,
        DOTCORE_FRONTEND_RUN_MODE: "",
        ...extraEnvironment,
      },
    });
  const bootstrapFrontend = async (lock = "lock-v1\n") => {
    await mkdir(path.join(root, "frontend", "node_modules"), { recursive: true });
    await writeFile(path.join(root, "frontend", "node_modules", ".dotcore-lock"), lock);
  };
  const bootstrapBackend = async () => {
    await mkdir(siteDir, { recursive: true });
    await writeFile(path.join(siteDir, "site.yaml"), "");
  };
  const started = () => readFile(calls, "utf8").catch(() => "");

  return { root, run, bootstrapFrontend, bootstrapBackend, started };
}

const frontendMessage = "frontend is not bootstrapped (dependencies missing or outdated); run `just frontend::bootstrap`\n";

test("up reports every missing piece and does not start the stack", async (t) => {
  const { root, run, started } = await fixture(t);

  const result = run(["dev", "up"]);
  assert.equal(result.status, 1);
  assert.equal(result.stderr, `${frontendMessage}backend is not bootstrapped (no development site at ${path.join(root, "site")}); run \`just backend::bootstrap\`\n`);
  assert.equal(await started(), "");
});

test("up starts the stack once both are bootstrapped, and a changed lockfile stops it again", async (t) => {
  const { root, run, bootstrapFrontend, bootstrapBackend, started } = await fixture(t);
  await bootstrapFrontend();
  await bootstrapBackend();

  const result = run(["dev", "up"]);
  assert.equal(result.status, 0, result.stderr);
  assert.equal(await started(), "started\n");

  await writeFile(path.join(root, "frontend", "package-lock.json"), "lock-v2\n");
  const outdated = run(["dev", "up"]);
  assert.equal(outdated.status, 1);
  assert.equal(outdated.stderr, frontendMessage);
  assert.equal(await started(), "started\n");
});
