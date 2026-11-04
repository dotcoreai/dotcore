//! Verifies that development site recipes are thin workspace-scoped `dotc` delegations.

import assert from "node:assert/strict";
import { chmod, mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-site-recipes-"));
  t.after(() => rm(root, { force: true, recursive: true }));
  const bin = path.join(root, "bin");
  await mkdir(bin);
  const calls = path.join(root, "calls");
  const held = path.join(root, "backend-held");
  const site = path.join(root, "workspace-site");
  const catalog = path.join(root, "workspace-catalog");
  await writeFile(path.join(bin, "dotc"), `#!/bin/sh
printf 'dotc' >> "$TEST_CALLS"
printf ' <%s>' "$@" >> "$TEST_CALLS"
printf '\n' >> "$TEST_CALLS"
if test -f "$TEST_HELD"; then
  printf 'cannot reset site at %s; stop the backend and retry\n' "$DOTCORE_SITE_DIR" >&2
  exit 1
fi
printf 'Site reset\n'
`);
  await writeFile(path.join(bin, "process-compose"), `#!/bin/sh
# The guards probe the endpoint, and a probe is not a process control: it answers, and it is not
# recorded, so the call assertions stay about real controls.
case "$*" in
  *"project state"*) exit 0 ;;
esac
printf 'process-compose' >> "$TEST_CALLS"
printf ' <%s>' "$@" >> "$TEST_CALLS"
printf '\n' >> "$TEST_CALLS"
for argument in "$@"; do
  if test "$argument" = down; then rm -f "$TEST_HELD"; fi
done
`);
  await chmod(path.join(bin, "dotc"), 0o755);
  await chmod(path.join(bin, "process-compose"), 0o755);
  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    DOTCORE_SITE_DIR: site,
    DOTCORE_CATALOG_DIR: catalog,
    DOTCORE_MACHINE_HOST: "192.0.2.44",
    PC_SOCKET_PATH: path.join(root, "process-compose.sock"),
    TEST_CALLS: calls,
    TEST_HELD: held,
  };
  delete environment.DOTCORE_YES;
  delete environment.DOTCORE_SKIP_RUNTIME_CLEANUP;
  const runWith = (overrides, ...arguments_) => spawnSync("just", ["dev", ...arguments_], {
    cwd: repositoryRoot,
    encoding: "utf8",
    env: { ...environment, ...overrides },
  });
  const run = (...arguments_) => runWith({}, ...arguments_);
  return { calls, catalog, environment, held, run, runWith, site };
}

test("site-reset delegates one exact workspace reset to dotc", async (t) => {
  const { calls, catalog, run, site } = await fixture(t);

  const reset = run("site-reset", "simulated", "docker");

  assert.equal(reset.status, 0, reset.stderr);
  assert.equal(reset.stdout, "Site reset\n");
  assert.equal(await readFile(calls, "utf8"), `dotc <site> <reset> <--site-dir> <${site}> <--catalog-dir> <${catalog}> <--host> <192.0.2.44> <--inference> <simulated> <--applications> <docker> <--non-interactive>\n`);
});

test("site-init and site-reset declare Apple inference ephemeral and leave other choices untouched", async (t) => {
  const { calls, catalog, run, site } = await fixture(t);
  const prefix = (command) => `dotc <site> <${command}> <--site-dir> <${site}> <--catalog-dir> <${catalog}> <--host> <192.0.2.44>`;

  for (const command of ["site-init", "site-reset"]) {
    const result = run(command, "apple", "docker");
    assert.equal(result.status, 0, result.stderr);
  }
  const init = run("site-init", "simulated", "docker");
  assert.equal(init.status, 0, init.stderr);

  assert.equal(await readFile(calls, "utf8"), [
    `${prefix("init")} <--inference> <apple> <--applications> <docker> <--non-interactive> <--native-workload-lifetime> <ephemeral>`,
    `${prefix("reset")} <--inference> <apple> <--applications> <docker> <--non-interactive> <--native-workload-lifetime> <ephemeral>`,
    `${prefix("init")} <--inference> <simulated> <--applications> <docker> <--non-interactive>`,
    "",
  ].join("\n"));
});

test("site-init and site-reset pass simulated hardware to dotc as inference choices", async (t) => {
  const { calls, catalog, run, site } = await fixture(t);
  const prefix = (command) => `dotc <site> <${command}> <--site-dir> <${site}> <--catalog-dir> <${catalog}> <--host> <192.0.2.44>`;

  assert.equal(run("site-init", "simulated:rtx-5090,simulated:rtx-4090", "docker").status, 0);
  assert.equal(run("site-reset", "simulated:mac-64g", "none").status, 0);

  assert.equal(await readFile(calls, "utf8"), [
    `${prefix("init")} <--inference> <simulated:rtx-5090> <--inference> <simulated:rtx-4090> <--applications> <docker> <--non-interactive>`,
    `${prefix("reset")} <--inference> <simulated:mac-64g> <--applications> <none> <--non-interactive>`,
    "",
  ].join("\n"));
});

test("site-init and site-reset expand comma-separated choices into repeated flags", async (t) => {
  const { calls, catalog, run, site } = await fixture(t);
  const prefix = (command) => `dotc <site> <${command}> <--site-dir> <${site}> <--catalog-dir> <${catalog}> <--host> <192.0.2.44>`;

  for (const [command, inference, applications] of [
    ["site-init", "nvidia, apple", "docker,simulated,"],
    ["site-reset", "simulated,", "none"],
  ]) {
    const result = run(command, inference, applications);
    assert.equal(result.status, 0, result.stderr);
  }

  assert.equal(await readFile(calls, "utf8"), [
    `${prefix("init")} <--inference> <nvidia> <--inference> <apple> <--applications> <docker> <--applications> <simulated> <--non-interactive> <--native-workload-lifetime> <ephemeral>`,
    `${prefix("reset")} <--inference> <simulated> <--applications> <none> <--non-interactive>`,
    "",
  ].join("\n"));
});

test("site-init and site-reset ask for the choices when none are given", async (t) => {
  const { calls, catalog, run, site } = await fixture(t);
  const prefix = (command) => `dotc <site> <${command}> <--site-dir> <${site}> <--catalog-dir> <${catalog}> <--host> <192.0.2.44>`;

  for (const command of ["site-init", "site-reset"]) {
    const result = run(command);
    assert.equal(result.status, 0, result.stderr);
  }

  assert.equal(await readFile(calls, "utf8"), [
    `${prefix("init")} <--interactive>`,
    `${prefix("reset")} <--interactive>`,
    "",
  ].join("\n"));
});

test("site-init and site-reset refuse exactly one of the choices before running dotc", async (t) => {
  const { calls, run } = await fixture(t);

  for (const command of ["site-init", "site-reset"]) {
    for (const arguments_ of [["simulated"], ["", "docker"]]) {
      const result = run(command, ...arguments_);

      assert.equal(result.status, 2, `${command} ${arguments_.join(",")}`);
      assert.equal(result.stdout, "");
      assert.match(result.stderr, /pass both inference and applications, or neither to be asked/);
    }
  }
  await assert.rejects(readFile(calls, "utf8"), { code: "ENOENT" });
});

test("site-reset forwards --yes and --skip-runtime-cleanup only for the value 1", async (t) => {
  const { calls, catalog, runWith, site } = await fixture(t);
  const command = `dotc <site> <reset> <--site-dir> <${site}> <--catalog-dir> <${catalog}> <--host> <192.0.2.44> <--inference> <simulated> <--applications> <docker> <--non-interactive>`;

  for (const value of ["1", "0"]) {
    const reset = runWith({ DOTCORE_YES: value, DOTCORE_SKIP_RUNTIME_CLEANUP: value }, "site-reset", "simulated", "docker");
    assert.equal(reset.status, 0, reset.stderr);
  }
  const invalid = runWith({ DOTCORE_SKIP_RUNTIME_CLEANUP: "true" }, "site-reset", "simulated", "docker");

  assert.equal(invalid.status, 2);
  assert.equal(invalid.stdout, "");
  assert.match(invalid.stderr, /DOTCORE_SKIP_RUNTIME_CLEANUP must be 1 or unset/);
  assert.equal(await readFile(calls, "utf8"), `${command} <--yes> <--skip-runtime-cleanup>\n${command}\n`);
});

test("a held backend is not stopped and retry succeeds only after dev down", async (t) => {
  const { calls, held, run, site } = await fixture(t);
  await writeFile(held, "held");

  const blocked = run("site-reset", "simulated", "docker");

  assert.notEqual(blocked.status, 0);
  assert.equal(blocked.stdout, "");
  assert.match(blocked.stderr, new RegExp(`cannot reset site at ${escapeRegex(site)}; stop the backend and retry`));
  assert.equal((await readFile(calls, "utf8")).match(/process-compose/g), null);

  const down = run("down");
  assert.equal(down.status, 0, down.stderr);
  const retried = run("site-reset", "simulated", "docker");
  assert.equal(retried.status, 0, retried.stderr);
  assert.equal(retried.stdout, "Site reset\n");

  const recorded = await readFile(calls, "utf8");
  assert.equal(recorded.match(/^dotc /gm)?.length, 2);
  assert.equal(recorded.match(/^process-compose /gm)?.length, 1);
});

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
