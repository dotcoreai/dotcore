//! Verifies workspace discovery, allocation, derivation, and atomic environment publication.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { chmod, link, lstat, mkdtemp, mkdir, open, readFile, realpath, rename, rm, stat, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";

import {
  ENVIRONMENT_VARIABLES,
  createLocalFile,
  deriveEnvironment,
  discoverWorkspaceRoots,
  environmentArtifactPath,
  initializeEnvironment,
  parseGitWorktreePorcelain,
  parseJjWorkspaceRoots,
  processComposeSocketPath,
  publishEnvironmentArtifact,
  serializeEnvironment,
  warnIfWorkerLoopbackMissing,
} from "./workspace-environment.mjs";

const schemaOrder = [
  "DOTCORE_MACHINE_HOST", "DOTCORE_DEV_INDEX", "DOTCORE_SITE_DIR",
  "DOTCORE_CATALOG_DIR", "DOTCORE_CACHE_DIR", "DOTCORE_BACKEND_RUN_MODE",
  "DOTCORE_FRONTEND_RUN_MODE", "HF_XET_CACHE", "DOTCORE_FRONTEND_PORT",
  "DOTCORE_MANAGEMENT_PORT", "DOTCORE_INFERENCE_PORT", "PC_SOCKET_PATH",
  "DOTCORE_FRONTEND_ORIGIN", "DOTCORE_MANAGEMENT_ADDR", "DOTCORE_MANAGEMENT_ORIGIN",
  "DOTCORE_INFERENCE_ADDR", "DOTCORE_INFERENCE_ORIGIN", "DOTCORE_PRIVATE_PORT",
  "DOTCORE_PRIVATE_ADDR", "DOTCORE_WORKER_PRIVATE_PORT",
  "DOTCORE_DEV_PUBLIC_PORT", "DOTCORE_PUBLIC_PORT", "DOTCORE_PUBLIC_ORIGIN", "DOTCORE_WORKER_DIR", "DOTCORE_DEV_SOURCE_FLAKE",
  "VITE_DOTCORE_MANAGEMENT_ORIGIN", "VITE_DOTCORE_INFERENCE_ORIGIN",
  "JUST_TEMPDIR", "RUST_LIB_BACKTRACE",
];

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-workspace-environment-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  return root;
}

async function workspace(root, name, main = false) {
  const workspaceRoot = path.join(root, name);
  await mkdir(path.join(workspaceRoot, ".jj"), { recursive: true });
  if (main) await mkdir(path.join(workspaceRoot, ".jj", "repo"));
  else await writeFile(path.join(workspaceRoot, ".jj", "repo"), "pointer\n");
  await writeFile(path.join(workspaceRoot, ".env.base"),
    "DOTCORE_MACHINE_HOST=127.0.0.1\nJUST_TEMPDIR=/tmp\nRUST_LIB_BACKTRACE=1\n");
  return workspaceRoot;
}

async function fakeExecutables(root, {
  jj,
  git,
  jjAfterLock = jj,
  gitAfterLock = git,
  slotLockPath,
}) {
  const bin = path.join(root, "bin");
  await mkdir(bin);
  const script = [
    "#!/usr/bin/env node",
    "const { existsSync } = require('node:fs');",
    "const name = process.argv[1].split('/').pop();",
    "const suffix = process.env.SLOT_LOCK_PATH && existsSync(process.env.SLOT_LOCK_PATH) ? '_AFTER_LOCK' : '';",
    "const outputKey = `${name === 'jj' ? 'JJ' : 'GIT'}_OUTPUT${suffix}`;",
    "process.stdout.write(Buffer.from(process.env[outputKey] || '', 'base64'));",
    "",
  ].join("\n");
  for (const name of ["jj", "git"]) {
    const executable = path.join(bin, name);
    await writeFile(executable, script);
    await chmod(executable, 0o755);
  }
  return {
    PATH: `${bin}:${process.env.PATH}`,
    JJ_OUTPUT: Buffer.from(jj).toString("base64"),
    GIT_OUTPUT: Buffer.from(git).toString("base64"),
    JJ_OUTPUT_AFTER_LOCK: Buffer.from(jjAfterLock).toString("base64"),
    GIT_OUTPUT_AFTER_LOCK: Buffer.from(gitAfterLock).toString("base64"),
    SLOT_LOCK_PATH: slotLockPath,
  };
}

async function holdProcessLock(lockPath) {
  await mkdir(path.dirname(lockPath), { recursive: true });
  const childCode = `
    import { spawnSync } from "node:child_process";
    import { open } from "node:fs/promises";
    const handle = await open(process.argv[1], "a+", 0o600);
    const locked = spawnSync("flock", ["-n", "-E", "75", "3"],
      { stdio: ["ignore", "ignore", "pipe", handle.fd], timeout: 2000 });
    if (locked.status !== 0) { console.error(locked.stderr.toString()); process.exit(1); }
    console.log("READY");
    process.stdin.resume();
    process.stdin.once("data", async () => { await handle.close(); process.exit(0); });
    setInterval(() => {}, 1000);
  `;
  const holder = spawn(process.execPath, ["--input-type=module", "-e", childCode, lockPath],
    { stdio: ["pipe", "pipe", "pipe"] });
  try {
    const [ready] = await once(holder.stdout, "data", { signal: AbortSignal.timeout(5000) });
    assert.equal(ready.toString(), "READY\n");
    return holder;
  } catch (error) {
    if (holder.exitCode === null && holder.signalCode === null) holder.kill("SIGKILL");
    throw error;
  }
}

function gitRecord(root) {
  return `worktree ${root}\0HEAD ${"a".repeat(40)}\0branch refs/heads/main\0\0`;
}

function inputs(index = "00") {
  return {
    cacheRoot: "/cache/dotcore", siteConfig: "/workspace/.dotcore/site",
    masterCatalog: "/workspace/catalog", host: "100.64.0.9", index,
    justTempdir: "/tmp", rustLibBacktrace: "1",
  };
}

test("slot ports follow one formula from slot 00 through slot 99", () => {
  const ports = (index) => {
    const environment = deriveEnvironment(inputs(index));
    return [
      environment.DOTCORE_FRONTEND_PORT, environment.DOTCORE_MANAGEMENT_PORT, environment.DOTCORE_INFERENCE_PORT,
      environment.DOTCORE_PRIVATE_PORT, environment.DOTCORE_WORKER_PRIVATE_PORT, environment.DOTCORE_DEV_PUBLIC_PORT,
    ];
  };

  assert.deepEqual(ports("00"), ["21000", "21001", "21002", "21003", "21004", "21005"]);
  assert.deepEqual(ports("01"), ["21010", "21011", "21012", "21013", "21014", "21015"]);
  assert.deepEqual(ports("99"), ["21990", "21991", "21992", "21993", "21994", "21995"]);
});

test("bare dotcore keeps the bundle public port only in the main workspace", () => {
  for (const [index, bundlePublicPort, developmentPublicPort] of [["00", "8080", "21005"], ["07", "21075", "21075"]]) {
    const environment = deriveEnvironment(inputs(index));
    assert.equal(environment.DOTCORE_PUBLIC_PORT, bundlePublicPort, index);
    assert.equal(environment.DOTCORE_DEV_PUBLIC_PORT, developmentPublicPort, index);
  }
});

test("development host accepts IPv4 literals and canonical DNS names", () => {
  for (const host of ["127.0.0.1", "localhost", "manager", "manager.example.ts.net", "a-1.b2", `${"a".repeat(63)}.example`, "0x0.example"]) {
    const environment = deriveEnvironment({ ...inputs("07"), host });
    assert.equal(environment.DOTCORE_MACHINE_HOST, host);
    assert.equal(environment.DOTCORE_FRONTEND_ORIGIN, `http://${host}:21070`);
    assert.equal(environment.DOTCORE_PUBLIC_ORIGIN, `http://${host}:21075`);
    assert.equal(environment.DOTCORE_PRIVATE_ADDR, `${host}:21073`);
  }
});

test("development host rejects wildcard, non-canonical and malformed values", () => {
  for (const host of [
    "0.0.0.0", "::1", "", "Manager", "manager.", "-a.example", "a-.example", "a..example", "a_b.example",
    "a.1", "100.64.0", "100.64.0.256", "100.64.0.9.1", `${"a".repeat(64)}.example`, `${"a.".repeat(127)}aa`,
    "0x0", "0x7f000001", "1.0x2",
  ]) {
    assert.throws(() => deriveEnvironment({ ...inputs("07"), host }), /DOTCORE_MACHINE_HOST/, host);
  }
  assert.throws(() => deriveEnvironment({ ...inputs("07"), host: undefined }), /DOTCORE_MACHINE_HOST/);
});

test("workspace activation preserves the ordered development schema", () => {
  assert.deepEqual(ENVIRONMENT_VARIABLES, schemaOrder);
  const environment = deriveEnvironment(inputs("07"));
  assert.deepEqual(Object.keys(environment), schemaOrder);
  assert.deepEqual(environment, {
    DOTCORE_MACHINE_HOST: "100.64.0.9", DOTCORE_DEV_INDEX: "07",
    DOTCORE_SITE_DIR: "/workspace/.dotcore/site", DOTCORE_CATALOG_DIR: "/workspace/catalog",
    DOTCORE_CACHE_DIR: "/cache/dotcore", DOTCORE_BACKEND_RUN_MODE: "dev",
    DOTCORE_FRONTEND_RUN_MODE: "dev", HF_XET_CACHE: "/cache/dotcore/xet",
    DOTCORE_FRONTEND_PORT: "21070", DOTCORE_MANAGEMENT_PORT: "21071", DOTCORE_INFERENCE_PORT: "21072",
    PC_SOCKET_PATH: processComposeSocketPath("/workspace", { DOTCORE_DEV_INDEX: "07" }),
    DOTCORE_FRONTEND_ORIGIN: "http://100.64.0.9:21070", DOTCORE_MANAGEMENT_ADDR: "100.64.0.9:21071",
    DOTCORE_MANAGEMENT_ORIGIN: "http://100.64.0.9:21071", DOTCORE_INFERENCE_ADDR: "100.64.0.9:21072",
    DOTCORE_INFERENCE_ORIGIN: "http://100.64.0.9:21072", DOTCORE_PRIVATE_PORT: "21073",
    DOTCORE_PRIVATE_ADDR: "100.64.0.9:21073",
    DOTCORE_WORKER_PRIVATE_PORT: "21074", DOTCORE_DEV_PUBLIC_PORT: "21075",
    DOTCORE_PUBLIC_PORT: "21075", DOTCORE_PUBLIC_ORIGIN: "http://100.64.0.9:21075", DOTCORE_WORKER_DIR: "/workspace/.dotcore/two-node-worker",
    DOTCORE_DEV_SOURCE_FLAKE: "git+file:///workspace",
    VITE_DOTCORE_MANAGEMENT_ORIGIN: "same-origin",
    VITE_DOTCORE_INFERENCE_ORIGIN: "same-origin", JUST_TEMPDIR: "/tmp", RUST_LIB_BACKTRACE: "1",
  });
});

test("development source flake URL-escapes the workspace root", () => {
  const environment = deriveEnvironment({ ...inputs(), workspaceRoot: "/work/my repo#1?x" });
  assert.equal(environment.DOTCORE_DEV_SOURCE_FLAKE, "git+file:///work/my%20repo%231%3Fx");
});

test("cache defaults treat empty and unset XDG_CACHE_HOME as a HOME fallback", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n`, git: gitRecord(main) });
  const env = { ...fake, HOME: "/home/user", XDG_CACHE_HOME: "" };
  const expected = "/home/user/.cache/dotcore";

  const empty = await initializeEnvironment({ cwd: main, env });
  assert.equal(empty.environment.DOTCORE_CACHE_DIR, expected);
  assert.equal(empty.environment.HF_XET_CACHE, `${expected}/xet`);

  const unset = await initializeEnvironment({ cwd: main, env: { ...env, XDG_CACHE_HOME: undefined } });
  assert.equal(unset.environment.DOTCORE_CACHE_DIR, expected);
});

test("cache defaults validate XDG and HOME only when needed", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n`, git: gitRecord(main) });
  const env = { ...fake, HOME: "/home/user", XDG_CACHE_HOME: "/xdg/cache" };

  const xdg = await initializeEnvironment({ cwd: main, env });
  assert.equal(xdg.environment.DOTCORE_CACHE_DIR, "/xdg/cache/dotcore");
  await assert.rejects(
    initializeEnvironment({ cwd: main, env: { ...env, XDG_CACHE_HOME: "relative-cache" } }),
    { message: "XDG_CACHE_HOME or HOME must provide an absolute path for DOTCORE_CACHE_DIR." },
  );
  await assert.rejects(
    initializeEnvironment({ cwd: main, env: { ...env, XDG_CACHE_HOME: "", HOME: "relative-home" } }),
    { message: "XDG_CACHE_HOME or HOME must provide an absolute path for DOTCORE_CACHE_DIR." },
  );
  await assert.rejects(
    initializeEnvironment({ cwd: main, env: { ...env, XDG_CACHE_HOME: "", HOME: undefined } }),
    { message: "XDG_CACHE_HOME or HOME must provide an absolute path for DOTCORE_CACHE_DIR." },
  );

  const explicit = await initializeEnvironment({
    cwd: main,
    env: { ...env, XDG_CACHE_HOME: "relative-cache", HOME: undefined, DOTCORE_CACHE_DIR: "/explicit/cache" },
  });
  assert.equal(explicit.environment.DOTCORE_CACHE_DIR, "/explicit/cache");
});

test("workspace discovery uses JJ roots and Git worktrees from fake executables", async (t) => {
  const root = await fixture(t); const main = await workspace(root, "main", true); const secondary = await workspace(root, "secondary");
  const commands = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const discovered = await discoverWorkspaceRoots({ cwd: secondary, env: { ...process.env, ...commands } });
  assert.deepEqual(discovered, { currentRoot: secondary, mainRoot: main, roots: [main, secondary] });
  assert.deepEqual(parseJjWorkspaceRoots(commands.JJ_OUTPUT && `${JSON.stringify(main)}\n`), [main]);
  assert.deepEqual(parseGitWorktreePorcelain(gitRecord(main)), [main]);
});

test("activation allocates main slot 00 and the first unused secondary slot", async (t) => {
  const root = await fixture(t); const main = await workspace(root, "main", true); const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const result = await initializeEnvironment({ cwd: secondary, env, random: () => 0 });
  assert.equal(result.environment.DOTCORE_DEV_INDEX, "01");
  assert.equal(await readFile(path.join(secondary, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=01\n");
  assert.equal(result.environment.DOTCORE_FRONTEND_PORT, "21010");
  assert.equal(result.environment.DOTCORE_MANAGEMENT_PORT, "21011");
  assert.equal(result.environment.DOTCORE_INFERENCE_PORT, "21012");
});

test("concurrent activation rechecks shared slot allocation before creating local environments", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const first = await workspace(root, "first");
  const second = await workspace(root, "second");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(first)}\n${JSON.stringify(second)}\n`, git: gitRecord(main) + gitRecord(first) + gitRecord(second) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };

  const [firstResult, secondResult] = await Promise.all([
    initializeEnvironment({ cwd: first, env, random: () => 0 }),
    initializeEnvironment({ cwd: second, env, random: () => 0 }),
  ]);

  assert.deepEqual(
    [firstResult.environment.DOTCORE_DEV_INDEX, secondResult.environment.DOTCORE_DEV_INDEX].sort(),
    ["01", "02"],
  );
  assert.notEqual(
    await readFile(path.join(first, ".env.local"), "utf8"),
    await readFile(path.join(second, ".env.local"), "utf8"),
  );
});

test("same-workspace contenders keep one persisted slot", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const results = await Promise.all([
    initializeEnvironment({ cwd: secondary, env, random: () => 0 }),
    initializeEnvironment({ cwd: secondary, env, random: () => 0.9 }),
  ]);
  assert.deepEqual(results.map((result) => result.createdLocalFile).sort(), [false, true]);
  const persistedIndex = results.find((result) => result.createdLocalFile).environment.DOTCORE_DEV_INDEX;
  assert.match(persistedIndex, /^(01|90)$/);
  assert.deepEqual(results.map((result) => result.environment.DOTCORE_DEV_INDEX), [persistedIndex, persistedIndex]);
  assert.equal(await readFile(path.join(secondary, ".env.local"), "utf8"), `DOTCORE_DEV_INDEX=${persistedIndex}\n`);
});

test("legacy sentinel collision fails closed before writing a slot", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const legacyPath = path.join(main, ".dotcore", ".workspace-environment-slot-allocation.lock");
  const v2Path = path.join(main, ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  await mkdir(path.dirname(legacyPath), { recursive: true });
  const oldOwner = await open(legacyPath, "wx", 0o600);
  try {
    await assert.rejects(
      initializeEnvironment({ cwd: secondary, env, random: () => 0 }),
      /legacy.*lock|legacy.*sentinel/i,
    );
    assert.equal((await stat(legacyPath)).size, 0);
    await assert.rejects(readFile(path.join(secondary, ".env.local")), { code: "ENOENT" });
  } finally {
    await oldOwner.close();
    await unlink(legacyPath);
  }
  const activated = await initializeEnvironment({ cwd: secondary, env, random: () => 0 });
  const marker = await lstat(legacyPath, { bigint: true });
  const lock = await lstat(v2Path, { bigint: true });
  assert.equal(activated.environment.DOTCORE_DEV_INDEX, "01");
  assert.equal(await readFile(path.join(secondary, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=01\n");
  assert.deepEqual([marker.dev, marker.ino], [lock.dev, lock.ino]);
});

test("legacy owner winning marker creation cannot be overwritten", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const legacyPath = path.join(main, ".dotcore", ".workspace-environment-slot-allocation.lock");
  let raced = false;
  const slotLockFileSystem = { lstat, mkdir, open, realpath,
    link: async (source, destination) => {
      const older = await open(destination, "wx", 0o600);
      await older.close();
      raced = true;
      await link(source, destination);
    },
  };
  await assert.rejects(
    initializeEnvironment({ cwd: secondary, env: { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" }, random: () => 0, slotLockFileSystem }),
    /legacy.*lock/i,
  );
  assert.equal(raced, true);
  assert.equal((await stat(legacyPath)).size, 0);
  await assert.rejects(readFile(path.join(secondary, ".env.local")), { code: "ENOENT" });
});

test("replacing a held lock pathname fails before slot publication", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const lockPath = path.join(main, ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  let replaced = false;
  const slotLockFileSystem = { lstat, mkdir, open, realpath,
    link: async (source, destination) => {
      await rename(source, `${source}.replaced`);
      await writeFile(source, "different inode");
      replaced = true;
      await link(source, destination);
    },
  };
  await assert.rejects(
    initializeEnvironment({ cwd: secondary, env: { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" }, random: () => 0, slotLockFileSystem }),
    /legacy.*changed|lock path.*changed/i,
  );
  assert.equal(replaced, true);
  assert.notEqual((await lstat(lockPath)).ino, (await lstat(`${lockPath}.replaced`)).ino);
  await assert.rejects(readFile(path.join(secondary, ".env.local")), { code: "ENOENT" });
});

test("concurrent allocation waits for a process lock and retains unique slots", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const first = await workspace(root, "first");
  const second = await workspace(root, "second");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(first)}\n${JSON.stringify(second)}\n`, git: gitRecord(main) + gitRecord(first) + gitRecord(second) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const lockPath = path.join(main, ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  const holder = await holdProcessLock(lockPath);
  const { promise: contended, resolve: noteContention, reject: rejectContention } = Promise.withResolvers();
  const deadline = setTimeout(() => rejectContention(new Error("allocators never contended on the process lock")), 5000);
  const pending = Promise.allSettled([
    initializeEnvironment({ cwd: first, env, random: () => 0, onSlotLockContention: noteContention }),
    initializeEnvironment({ cwd: second, env, random: () => 0, onSlotLockContention: noteContention }),
  ]);
  try {
    await contended;
    await assert.rejects(readFile(path.join(first, ".env.local")), { code: "ENOENT" });
    await assert.rejects(readFile(path.join(second, ".env.local")), { code: "ENOENT" });
    const exited = once(holder, "exit");
    holder.stdin.write("release\n");
    assert.deepEqual(await exited, [0, null]);
  } finally {
    clearTimeout(deadline);
    if (holder.exitCode === null && holder.signalCode === null) {
      const exited = once(holder, "exit"); holder.kill("SIGKILL"); await exited;
    }
    await pending;
  }
  const results = (await pending).map((outcome) => {
    if (outcome.status === "rejected") throw outcome.reason;
    return outcome.value;
  });
  assert.deepEqual(results.map((result) => result.environment.DOTCORE_DEV_INDEX).sort(), ["01", "02"]);
  assert.equal(await readFile(path.join(first, ".env.local"), "utf8"), `DOTCORE_DEV_INDEX=${results[0].environment.DOTCORE_DEV_INDEX}\n`);
  assert.equal(await readFile(path.join(second, ".env.local"), "utf8"), `DOTCORE_DEV_INDEX=${results[1].environment.DOTCORE_DEV_INDEX}\n`);
});

test("allocator SIGKILL releases the lock for a new assignment", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const lockPath = path.join(main, ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  const holder = await holdProcessLock(lockPath);
  const { promise: contended, resolve: noteContention, reject: rejectContention } = Promise.withResolvers();
  const deadline = setTimeout(() => rejectContention(new Error("allocator never contended on the process lock")), 5000);
  const pending = initializeEnvironment({ cwd: secondary, env: { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" }, random: () => 0, onSlotLockContention: noteContention });
  try {
    await contended;
    await assert.rejects(readFile(path.join(secondary, ".env.local")), { code: "ENOENT" });
    const exited = once(holder, "exit");
    holder.kill("SIGKILL");
    assert.deepEqual(await exited, [null, "SIGKILL"]);
    const activated = await pending;
    assert.equal(activated.environment.DOTCORE_DEV_INDEX, "01");
    assert.equal(await readFile(path.join(secondary, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=01\n");
  } finally {
    clearTimeout(deadline);
    if (holder.exitCode === null && holder.signalCode === null) {
      const exited = once(holder, "exit"); holder.kill("SIGKILL"); await exited;
    }
    await pending.catch(() => {});
  }
});

test("allocation times out without writing when the process lock stays held", async (t) => {
  const root = await fixture(t);
  const alias = path.join(root, "alias");
  await symlink(root, alias);
  const main = await workspace(alias, "main", true);
  const secondary = await workspace(alias, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const lockPath = path.join(await realpath(main), ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  const holder = await holdProcessLock(lockPath);
  try {
    await assert.rejects(
      initializeEnvironment({ cwd: secondary, env: { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" }, random: () => 0, slotLockTimeoutMilliseconds: 50 }),
      { message: `Timed out waiting for development slot allocation lock '${lockPath}'.` },
    );
    await assert.rejects(readFile(path.join(secondary, ".env.local")), { code: "ENOENT" });
    await assert.rejects(lstat(path.join(main, ".dotcore", ".workspace-environment-slot-allocation.lock")), { code: "ENOENT" });
  } finally {
    const exited = once(holder, "exit"); holder.kill("SIGKILL"); await exited;
  }
});

test("new slot rejects symlinked lock paths while established slots bypass them", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const directory = path.join(main, ".dotcore");
  await mkdir(directory);
  const lockPath = path.join(directory, ".workspace-environment-slot-allocation-v2.lock");
  const target = path.join(root, "target.lock");
  await writeFile(target, "unchanged");
  await symlink(target, lockPath);
  await assert.rejects(initializeEnvironment({ cwd: secondary, env, random: () => 0 }), /lock|ELOOP/i);
  assert.equal(await readFile(target, "utf8"), "unchanged");
  await assert.rejects(readFile(path.join(secondary, ".env.local")), { code: "ENOENT" });
  await writeFile(path.join(secondary, ".env.local"), "DOTCORE_DEV_INDEX=01\n");
  const established = await initializeEnvironment({ cwd: secondary, env });
  assert.equal(established.environment.DOTCORE_DEV_INDEX, "01");
});

test("activation refreshes workspace roots under the allocation lock", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const current = await workspace(root, "current");
  const peer = await workspace(root, "peer");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  await writeFile(path.join(peer, ".env.local"), "DOTCORE_DEV_INDEX=01\n");
  const lockPath = path.join(main, ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  const fake = await fakeExecutables(root, {
    jj: `${JSON.stringify(main)}\n${JSON.stringify(current)}\n`,
    git: gitRecord(main) + gitRecord(current),
    jjAfterLock: `${JSON.stringify(main)}\n${JSON.stringify(current)}\n${JSON.stringify(peer)}\n`,
    gitAfterLock: gitRecord(main) + gitRecord(current) + gitRecord(peer),
    slotLockPath: lockPath,
  });

  const result = await initializeEnvironment({
    cwd: current,
    env: { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" },
    random: () => 0,
  });

  assert.equal(result.environment.DOTCORE_DEV_INDEX, "02");
  assert.equal(result.environment.DOTCORE_FRONTEND_PORT, "21020");
  assert.equal(result.environment.DOTCORE_MANAGEMENT_PORT, "21021");
  assert.equal(result.environment.DOTCORE_INFERENCE_PORT, "21022");
  assert.equal(await readFile(path.join(current, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=02\n");
  assert.equal(await readFile(path.join(peer, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=01\n");
});

test("local environment files are created atomically with restrictive permissions", async (t) => {
  const root = await fixture(t);
  const localPath = path.join(root, ".env.local");
  let linked = false;
  await createLocalFile(localPath, 4, {
    fileSystem: {
      open,
      unlink,
      link: async (temporaryPath, destinationPath) => {
        assert.equal(await readFile(temporaryPath, "utf8"), "DOTCORE_DEV_INDEX=04\n");
        await assert.rejects(readFile(destinationPath, "utf8"), { code: "ENOENT" });
        linked = true;
        await link(temporaryPath, destinationPath);
      },
    },
  });
  assert.equal(linked, true);
  assert.equal(await readFile(localPath, "utf8"), "DOTCORE_DEV_INDEX=04\n");
});

test("a post-link failure reports that the established slot may already exist", async (t) => {
  const root = await fixture(t);
  const localPath = path.join(root, ".env.local");
  await assert.rejects(
    createLocalFile(localPath, 4, { fileSystem: {
      open, unlink,
      link: async (...args) => {
        await link(...args);
        throw Object.assign(new Error("link acknowledgment lost"), { code: "EIO" });
      },
    } }),
    /assignment may already exist.*reload/i,
  );
  assert.equal(await readFile(localPath, "utf8"), "DOTCORE_DEV_INDEX=04\n");
});

test("process death after slot publication retains the assignment for retry", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const childCode = `
    import { link, open, unlink } from "node:fs/promises";
    const { initializeEnvironment } = await import(process.argv[1]);
    await initializeEnvironment({ cwd: process.argv[2], env: process.env, random: () => 0,
      fileSystem: { open, unlink, link: async (source, destination) => {
        await link(source, destination);
        process.kill(process.pid, "SIGKILL");
      } },
    });
  `;
  const moduleUrl = new URL("./workspace-environment.mjs", import.meta.url).href;
  const child = spawn(process.execPath, ["--input-type=module", "-e", childCode, moduleUrl, secondary],
    { env, stdio: ["ignore", "ignore", "pipe"], signal: AbortSignal.timeout(5000) });
  const [code, signal] = await once(child, "exit");
  assert.deepEqual([code, signal], [null, "SIGKILL"]);
  assert.equal(await readFile(path.join(secondary, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=01\n");
  const recovered = await initializeEnvironment({ cwd: secondary, env, random: () => 0.9 });
  assert.equal(recovered.createdLocalFile, false);
  assert.equal(recovered.environment.DOTCORE_DEV_INDEX, "01");
});

test("environment publication failure retries without reallocating the established slot", async (t) => {
  const root = await fixture(t);
  const main = await workspace(root, "main", true);
  const secondary = await workspace(root, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const fileSystem = { link, mkdir, open, readFile, unlink,
    rename: async () => { throw new Error("generated artifact unavailable"); },
  };
  await assert.rejects(initializeEnvironment({ cwd: secondary, env, random: () => 0, fileSystem }), /generated artifact unavailable/);
  assert.equal(await readFile(path.join(secondary, ".env.local"), "utf8"), "DOTCORE_DEV_INDEX=01\n");
  const recovered = await initializeEnvironment({ cwd: secondary, env, random: () => 0.9 });
  assert.equal(recovered.createdLocalFile, false);
  assert.equal(recovered.environment.DOTCORE_DEV_INDEX, "01");
  assert.equal(await readFile(recovered.artifactPath, "utf8"), serializeEnvironment(recovered.environment));
});

test("post-allocation marker inspection failure keeps the established slot", async (t) => {
  const root = await fixture(t);
  const alias = path.join(root, "alias");
  await symlink(root, alias);
  const main = await workspace(alias, "main", true);
  const secondary = await workspace(alias, "secondary");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n${JSON.stringify(secondary)}\n`, git: gitRecord(main) + gitRecord(secondary) });
  const env = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const localPath = path.join(secondary, ".env.local");
  const legacyPath = path.join(await realpath(main), ".dotcore", ".workspace-environment-slot-allocation.lock");
  const slotLockFileSystem = { link, mkdir, open, realpath,
    lstat: async (file, options) => {
      if (file === legacyPath && (await stat(localPath).catch(() => undefined))?.isFile()) {
        throw Object.assign(new Error("marker inspection failed"), { code: "EIO" });
      }
      return lstat(file, options);
    },
  };
  await assert.rejects(
    initializeEnvironment({ cwd: secondary, env, random: () => 0, slotLockFileSystem }),
    /assignment may already exist.*reload.*marker inspection failed/i,
  );
  assert.equal(await readFile(localPath, "utf8"), "DOTCORE_DEV_INDEX=01\n");
  const recovered = await initializeEnvironment({ cwd: secondary, env, random: () => 0.9 });
  assert.equal(recovered.createdLocalFile, false);
  assert.equal(recovered.environment.DOTCORE_DEV_INDEX, "01");
});

test("local values override inherited/base inputs and pass through derived-looking and unknown keys", async (t) => {
  const root = await fixture(t); const main = await workspace(root, "main", true);
  await writeFile(path.join(main, ".env.base"), "DOTCORE_MACHINE_HOST=127.0.0.1\nJUST_TEMPDIR=/base\nRUST_LIB_BACKTRACE=1\nDOTCORE_BACKEND_RUN_MODE=nix\n");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\nDOTCORE_SITE_DIR=/override/site\nDOTCORE_CATALOG_DIR=/override/catalog\nVITE_DOTCORE_INFERENCE_ORIGIN=/override/inference\nUNKNOWN_KEY=kept\n");
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n`, git: gitRecord(main) });
  const result = await initializeEnvironment({ cwd: main, env: { ...process.env, ...fake, DOTCORE_MACHINE_HOST: "100.64.0.9", JUST_TEMPDIR: "/inherited", DOTCORE_BACKEND_RUN_MODE: undefined } });
  assert.equal(result.environment.DOTCORE_MACHINE_HOST, "100.64.0.9");
  assert.equal(result.environment.DOTCORE_BACKEND_RUN_MODE, "nix");
  assert.equal(result.environment.DOTCORE_SITE_DIR, "/override/site");
  assert.equal(result.environment.DOTCORE_CATALOG_DIR, "/override/catalog");
  assert.equal(result.environment.VITE_DOTCORE_INFERENCE_ORIGIN, "/override/inference");
  assert.equal(result.environment.UNKNOWN_KEY, "kept");
});

test("the old DOTCORE_DEV_HOST name is an unrelated variable and never feeds the machine host", async (t) => {
  const root = await fixture(t); const main = await workspace(root, "main", true);
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n`, git: gitRecord(main) });
  await writeFile(path.join(main, ".env.base"), "DOTCORE_MACHINE_HOST=127.0.0.1\nJUST_TEMPDIR=/base\nRUST_LIB_BACKTRACE=1\nDOTCORE_BACKEND_RUN_MODE=nix\n");
  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\n");
  const withoutOld = await initializeEnvironment({ cwd: main, env: { ...process.env, ...fake, DOTCORE_MACHINE_HOST: undefined, DOTCORE_DEV_HOST: undefined } });

  await writeFile(path.join(main, ".env.local"), "DOTCORE_DEV_INDEX=00\nDOTCORE_DEV_HOST=100.64.0.9\n");
  const withOld = await initializeEnvironment({ cwd: main, env: { ...process.env, ...fake, DOTCORE_MACHINE_HOST: undefined, DOTCORE_DEV_HOST: "100.64.0.8" } });

  assert.equal(withOld.environment.DOTCORE_MACHINE_HOST, "127.0.0.1");
  assert.equal(withOld.environment.DOTCORE_MANAGEMENT_ADDR, withoutOld.environment.DOTCORE_MANAGEMENT_ADDR);
  assert.equal(withOld.environment.DOTCORE_FRONTEND_ORIGIN, withoutOld.environment.DOTCORE_FRONTEND_ORIGIN);
  assert.equal(withOld.environment.DOTCORE_DEV_HOST, "100.64.0.9");
});

test("dev-release follows local, inherited, and base precedence and publishes exact values", async (t) => {
  const root = await fixture(t); const main = await workspace(root, "main", true);
  const fake = await fakeExecutables(root, { jj: `${JSON.stringify(main)}\n`, git: gitRecord(main) });
  const inherited = { ...process.env, ...fake, XDG_CACHE_HOME: "/shared/cache" };
  const localPath = path.join(main, ".env.local");
  const basePath = path.join(main, ".env.base");

  await writeFile(basePath, "DOTCORE_MACHINE_HOST=127.0.0.1\nJUST_TEMPDIR=/tmp\nRUST_LIB_BACKTRACE=1\nDOTCORE_BACKEND_RUN_MODE=nix\n");
  await writeFile(localPath, "DOTCORE_DEV_INDEX=00\nDOTCORE_BACKEND_RUN_MODE=dev-release\n");
  const fromLocal = await initializeEnvironment({ cwd: main, env: { ...inherited, DOTCORE_BACKEND_RUN_MODE: "dev" } });
  assert.equal(fromLocal.environment.DOTCORE_BACKEND_RUN_MODE, "dev-release");
  assert.equal(await readFile(fromLocal.artifactPath, "utf8"), serializeEnvironment(fromLocal.environment));

  await writeFile(localPath, "DOTCORE_DEV_INDEX=00\n");
  const fromInherited = await initializeEnvironment({ cwd: main, env: { ...inherited, DOTCORE_BACKEND_RUN_MODE: "dev-release" } });
  assert.equal(fromInherited.environment.DOTCORE_BACKEND_RUN_MODE, "dev-release");
  assert.equal(await readFile(fromInherited.artifactPath, "utf8"), serializeEnvironment(fromInherited.environment));

  await writeFile(basePath, "DOTCORE_MACHINE_HOST=127.0.0.1\nJUST_TEMPDIR=/tmp\nRUST_LIB_BACKTRACE=1\nDOTCORE_BACKEND_RUN_MODE=dev-release\n");
  const fromBase = await initializeEnvironment({ cwd: main, env: { ...inherited, DOTCORE_BACKEND_RUN_MODE: undefined } });
  assert.equal(fromBase.environment.DOTCORE_BACKEND_RUN_MODE, "dev-release");
  assert.equal(await readFile(fromBase.artifactPath, "utf8"), serializeEnvironment(fromBase.environment));
});

test("backend and frontend run modes reject values outside their exact supported sets", () => {
  assert.throws(
    () => deriveEnvironment({ ...inputs(), backendRunMode: "release" }),
    { message: "DOTCORE_BACKEND_RUN_MODE must be one of: dev, dev-release, nix." },
  );
  assert.throws(
    () => deriveEnvironment({ ...inputs(), frontendRunMode: "dev-release" }),
    { message: "DOTCORE_FRONTEND_RUN_MODE must be one of: dev, nix." },
  );
});

test("publishes stable bytes atomically and retains exact socket/cache/origin derivation", async (t) => {
  const root = await fixture(t); const environment = deriveEnvironment({ ...inputs("00"), workspaceRoot: root });
  const artifact = environmentArtifactPath(root); await mkdir(path.dirname(artifact), { recursive: true });
  let renamed = false;
  const fs = { mkdir, open, readFile, unlink,
    rename: async (temporary, destination) => { renamed = true; await rename(temporary, destination); } };
  await publishEnvironmentArtifact(root, environment, { fileSystem: fs });
  const first = await readFile(artifact, "utf8");
  await publishEnvironmentArtifact(root, environment, { fileSystem: fs });
  assert.equal(renamed, true);
  assert.equal(first, await readFile(artifact, "utf8"));
  assert.equal(first, serializeEnvironment(environment));
  assert.equal(first, schemaOrder.map((key) => `${key}=${environment[key]}`).join("\n") + "\n");
  assert.match(first, /DOTCORE_FRONTEND_PORT=21000\nDOTCORE_MANAGEMENT_PORT=21001\nDOTCORE_INFERENCE_PORT=21002/);
  assert.match(first, /PC_SOCKET_PATH=.*\n/);
  assert.match(first, /HF_XET_CACHE=.*\/xet\n/);
});

test("warns on darwin when the worker loopback alias is missing, without failing", async () => {
  const messages = []; const probed = [];
  const missing = await warnIfWorkerLoopbackMissing({
    platform: "darwin",
    probe: async (host) => { probed.push(host); return true; },
    write: (message) => messages.push(message),
  });
  assert.equal(missing, true);
  assert.deepEqual(probed, ["127.0.0.2"]);
  assert.deepEqual(messages, [
    "warning: 127.0.0.2 is not bound on this host; run `just dev::mac-loopback`, or the two-process backend tests skip and the node join CLI tests fail.\n",
  ]);
});

test("stays silent on darwin when the alias is available or the probe fails", async () => {
  for (const probe of [async () => false, async () => { throw new Error("boom"); }]) {
    const messages = [];
    assert.equal(await warnIfWorkerLoopbackMissing({ platform: "darwin", probe, write: (m) => messages.push(m) }), false);
    assert.deepEqual(messages, []);
  }
});

test("does not probe on non-darwin platforms", async () => {
  let probed = false;
  const messages = [];
  const missing = await warnIfWorkerLoopbackMissing({
    platform: "linux",
    probe: async () => { probed = true; return true; },
    write: (message) => messages.push(message),
  });
  assert.equal(missing, false);
  assert.equal(probed, false);
  assert.deepEqual(messages, []);
});
