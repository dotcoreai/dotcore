//! Verifies `just clean` confirms and tears down workloads before deleting workspace state.
//!
//! Each test runs the real `Justfile` and `dev/Justfile` from a temporary fake repository so the
//! workspace-root guard and the deletions never touch the developer's checkout.

import assert from "node:assert/strict";
import { chmod, copyFile, mkdtemp, mkdir, readFile, rm, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import test from "node:test";

function terminalTool() {
  const version = spawnSync("script", ["--version"], { encoding: "utf8" });
  return !version.error && `${version.stdout}${version.stderr}`.includes("util-linux");
}
const needsTerminal = { skip: !terminalTool() && "needs util-linux script for a pseudo-terminal" };

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-clean-recipe-"));
  t.after(() => rm(root, { force: true, recursive: true }));
  const bin = path.join(root, "bin");
  const calls = path.join(root, "calls");
  const site = path.join(root, ".dotcore", "site");
  const worker = path.join(root, ".dotcore", "two-node-worker");
  await mkdir(bin);
  await writeFile(calls, "");
  await mkdir(path.join(root, "dev"));
  await copyFile(path.join(repositoryRoot, "dev", "Justfile"), path.join(root, "dev", "Justfile"));
  // The dev recipes source this probe and the cluster record helpers, and mount the remote and cluster
  // modules, so the fake repository carries them too.
  for (const file of ["pc-endpoint.sh", "confirm.sh", "remote/Justfile", "cluster/Justfile", "cluster/targets.sh"]) {
    await mkdir(path.dirname(path.join(root, "dev", file)), { recursive: true });
    await copyFile(path.join(repositoryRoot, "dev", file), path.join(root, "dev", file));
  }
  await copyFile(path.join(repositoryRoot, "Justfile"), path.join(root, "Justfile"));
  for (const module of ["frontend", "backend", "proto", "apps", "engines", "installer", "ci"]) {
    await mkdir(path.join(root, module));
    await writeFile(path.join(root, module, "Justfile"), `clean:\n  @echo "${module} clean" >> "$TEST_CALLS"\n`);
  }
  for (const directory of [site, worker, path.join(root, ".scratch"), path.join(root, ".vite"), path.join(root, "chrome-profile")]) {
    await mkdir(directory, { recursive: true });
    await writeFile(path.join(directory, "marker"), "state");
  }
  await writeFile(`${site}.lock`, "lock");
  await writeFile(`${worker}.lock`, "lock");
  await writeFile(`${worker}.join-token`, "token");
  await writeFile(path.join(worker, "worker.yaml"), "joined node");
  await writeFile(path.join(root, ".env.local"), "kept");
  // The fake dotc records its arguments, exits with the code configured for the command, and on
  // success removes the directory a site delete targets, like the real command.
  // The fake remote tool lists the hosts a test configures and records its clean calls, like the real one
  // would reach them over ssh; anything else runs the real node.
  await writeFile(path.join(bin, "node"), `#!/bin/sh
if [ "$1" != dev/remote/remote-exec.mjs ]; then exec "${process.execPath}" "$@"; fi
shift
printf 'remote %s\\n' "$*" >> "$TEST_CALLS"
case "$1" in
  hosts) printf '%s\\n' $FAKE_REMOTE_HOSTS; exit "\${FAKE_REMOTE_HOSTS_EXIT:-0}" ;;
  resolve) echo "$2" ;;
  status) printf 'workspace: /remote/workspace (present)\\nchanges: none\\nlock: free\\nruns: none\\n' ;;
  clean) exit "\${FAKE_REMOTE_CLEAN_EXIT:-0}" ;;
esac
`);
  await chmod(path.join(bin, "node"), 0o755);
  await writeFile(path.join(bin, "dotc"), `#!/bin/sh
printf 'dotc %s\\n' "$*" >> "$TEST_CALLS"
case "$*" in
  "dev workload teardown"*) exit "\${FAKE_WORKLOAD_EXIT:-0}" ;;
  "site delete --site-dir $DOTCORE_SITE_DIR"*)
    if [ "\${FAKE_SITE_EXIT:-0}" != 0 ]; then exit "$FAKE_SITE_EXIT"; fi
    rm -rf "$DOTCORE_SITE_DIR" ;;
  "site delete --site-dir $DOTCORE_WORKER_DIR"*) rm -rf "$DOTCORE_WORKER_DIR" ;;
  "node leave --site-dir $DOTCORE_WORKER_DIR"*)
    if [ "\${FAKE_LEAVE_EXIT:-0}" != 0 ]; then exit "$FAKE_LEAVE_EXIT"; fi
    rm -rf "$DOTCORE_WORKER_DIR" "$DOTCORE_WORKER_DIR.leave-pending" ;;
esac
`);
  await chmod(path.join(bin, "dotc"), 0o755);
  // A socket file stands in for a stack, but only an answering endpoint is a running one: the stub
  // answers the guards' probe for the endpoints a test marks live, and otherwise fails the way the client
  // does when nothing is listening there. Stop requests are recorded; a stoppable stack stops on one.
  const setLiveSocket = (liveSockets, { stoppable = false } = {}) => writeFile(path.join(bin, "process-compose"), `#!/bin/sh
case "$*" in
  *"project state"*)
    case "$*" in
${[liveSockets].flat().map((socket) => `      *"--unix-socket ${socket} project state"*) test -e "${socket}.stopped" || exit 0 ;;`).join("\n")}
    esac
    printf 'dial unix socket: connect: connection refused\\n' >&2
    exit 1 ;;
  *" down")
    printf 'process-compose %s\\n' "$*" >> "$TEST_CALLS"
    ${stoppable ? 'touch "$3.stopped"' : ""} ;;
esac
exit 0
`);
  // A client that cannot read the endpoint is not evidence of a stopped stack, so its failure is
  // neither a refused nor an absent connection.
  const setBrokenProbe = () => writeFile(path.join(bin, "process-compose"), `#!/bin/sh
case "$*" in
  *"project state"*)
    printf 'unknown command "project" for "process-compose"\\n' >&2
    exit 1 ;;
esac
exit 0
`);
  // Nothing answers until a test marks an endpoint live.
  await setLiveSocket(path.join(root, "no-live-endpoint"));
  await chmod(path.join(bin, "process-compose"), 0o755);
  const environment = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    DOTCORE_SITE_DIR: site,
    DOTCORE_WORKER_DIR: worker,
    PC_SOCKET_PATH: path.join(root, "process-compose.sock"),
    TEST_CALLS: calls,
  };
  delete environment.DOTCORE_YES;
  delete environment.DOTCORE_REMOTE_CONFIG;
  delete environment.DOTCORE_SKIP_RUNTIME_CLEANUP;
  const run = (arguments_, extraEnvironment = {}) => spawnSync("just", arguments_, {
    cwd: root,
    encoding: "utf8",
    env: { ...environment, ...extraEnvironment },
  });
  const recorded = async () => (await readFile(calls, "utf8")).split("\n").filter(Boolean);
  const runInTerminal = (answer, arguments_, extraEnvironment = {}) => spawnSync("script", ["-qec", ["just", ...arguments_].map((word) => `'${word.replace(/'/g, "'\\''")}'`).join(" "), "/dev/null"], {
    cwd: root,
    encoding: "utf8",
    env: { ...environment, ...extraEnvironment },
    input: answer,
  });
  const exists = (target) => stat(target).then(() => true, () => false);
  const assertIntact = async () => {
    for (const directory of [site, worker, path.join(root, ".scratch"), path.join(root, ".vite"), path.join(root, "chrome-profile")]) {
      assert.equal(await exists(path.join(directory, "marker")), true, `${directory} was deleted`);
    }
  };
  return { assertIntact, exists, recorded, root, run, runInTerminal, setBrokenProbe, setLiveSocket, site, socket: path.join(root, "process-compose.sock"), worker };
}

test("clean removes workloads first, then the site, the worker folder and scratch output", async (t) => {
  const { worker, exists, recorded, root, run, site } = await fixture(t);

  const cleaned = run(["clean"]);

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual(await recorded(), [
    "dotc dev workload teardown --all",
    `dotc site delete --site-dir ${site}`,
    `dotc node leave --site-dir ${worker}`,
    "frontend clean",
    "backend clean",
  ]);
  for (const removed of [site, worker, `${site}.lock`, `${worker}.lock`, `${worker}.join-token`, path.join(root, ".scratch"), path.join(root, ".vite"), path.join(root, "chrome-profile")]) {
    assert.equal(await exists(removed), false, `${removed} remains`);
  }
  assert.equal(await readFile(path.join(root, ".env.local"), "utf8"), "kept");
});

test("clean releases the workspace's stored Docker image roots, names them, and leaves the rest of .dotcore", async (t) => {
  const { exists, root, run } = await fixture(t);
  const roots = path.join(root, ".dotcore", "docker-images");
  await mkdir(roots, { recursive: true });
  await symlink("/nix/store/fake-stored", path.join(roots, "docker-image-comfyui-linux-amd64"));
  await symlink("/nix/store/fake-stored-2", path.join(roots, "docker-image-ninfer-linux-amd64"));
  const slotLock = path.join(root, ".dotcore", ".workspace-environment-slot-allocation-v2.lock");
  await writeFile(slotLock, "");

  const cleaned = run(["clean"], { DOTCORE_YES: "1" });

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.match(cleaned.stderr, /released the stored Docker images of this workspace; .*: docker-image-comfyui-linux-amd64 docker-image-ninfer-linux-amd64\n/);
  assert.equal(await exists(roots), false);
  assert.equal(await exists(slotLock), true);

  const again = run(["clean"], { DOTCORE_YES: "1" });

  assert.equal(again.status, 0, again.stderr);
  assert.doesNotMatch(again.stderr, /released the stored Docker images/);
  assert.equal(await exists(slotLock), true);
});

test("DOTCORE_YES forwards the confirmation bypass to every command", async (t) => {
  const { worker, recorded, run, site } = await fixture(t);

  const cleaned = run(["clean"], { DOTCORE_YES: "1" });

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual((await recorded()).slice(0, 3), [
    "dotc dev workload teardown --all --yes",
    `dotc site delete --site-dir ${site} --yes`,
    `dotc node leave --site-dir ${worker} --yes`,
  ]);
});

test("clean drops the local worker from the cluster record and names the remote workers it cannot reach", async (t) => {
  const { recorded, run, site } = await fixture(t);
  await writeFile(`${site}.cluster`, "local node-1\ngb10:spark node-2\n");

  const result = run(["clean"], { DOTCORE_YES: "1" });
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stderr, /remote cluster workers remain: gb10:spark ; remove them with just dev::cluster::clean/);
  assert.equal(await readFile(`${site}.cluster`, "utf8"), "gb10:spark node-2\n");
  assert.ok((await recorded()).length > 0);
});

test("DOTCORE_SKIP_RUNTIME_CLEANUP skips workload teardown and runtime cleanup, leaving the worker through the site delete", async (t) => {
  const { worker, recorded, run, site } = await fixture(t);

  const cleaned = run(["clean"], { DOTCORE_SKIP_RUNTIME_CLEANUP: "1" });

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.match(cleaned.stderr, /skipping development workload teardown; workloads may remain/);
  assert.deepEqual((await recorded()).slice(0, 2), [
    `dotc site delete --site-dir ${site} --skip-runtime-cleanup`,
    `dotc site delete --site-dir ${worker} --skip-runtime-cleanup`,
  ]);
});

test("a worker folder without worker.yaml is deleted through the site delete", async (t) => {
  const { worker, exists, recorded, run } = await fixture(t);
  await rm(path.join(worker, "worker.yaml"));

  const cleaned = run(["clean"]);

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual((await recorded()).slice(2, 3), [`dotc site delete --site-dir ${worker} --skip-runtime-cleanup`]);
  assert.equal(await exists(worker), false);
});

test("a leftover pending marker reruns the node leave even when the folder is gone", async (t) => {
  const { worker, exists, recorded, run } = await fixture(t);
  await rm(worker, { force: true, recursive: true });
  await writeFile(`${worker}.leave-pending`, "site\nnode\n");

  const cleaned = run(["clean"]);

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual((await recorded()).slice(2, 3), [`dotc node leave --site-dir ${worker}`]);
  for (const removed of [`${worker}.leave-pending`, `${worker}.lock`, `${worker}.join-token`]) {
    assert.equal(await exists(removed), false, `${removed} remains`);
  }
});

test("a declined or failed worker leave keeps the worker folder and scratch and stops the other cleans", async (t) => {
  for (const code of ["130", "1"]) {
    const { exists, recorded, root, run, worker } = await fixture(t);

    const cleaned = run(["clean"], { FAKE_LEAVE_EXIT: code });

    assert.equal(cleaned.status, Number(code));
    assert.equal((await recorded()).at(-1), `dotc node leave --site-dir ${worker}`);
    assert.equal(await exists(path.join(worker, "worker.yaml")), true);
    assert.equal(await exists(path.join(root, ".scratch", "marker")), true);
  }
});

test("bypass variables accept only 1; 0 is off and other values are refused before any teardown", async (t) => {
  const { recorded, run } = await fixture(t);

  const off = run(["clean"], { DOTCORE_YES: "0", DOTCORE_SKIP_RUNTIME_CLEANUP: "0" });
  assert.equal(off.status, 0, off.stderr);
  assert.match((await recorded())[0], /^dotc dev workload teardown --all$/);

  const refused = await fixture(t);
  const invalid = refused.run(["dev", "clean"], { DOTCORE_SKIP_RUNTIME_CLEANUP: "yes" });
  assert.equal(invalid.status, 2);
  assert.match(invalid.stderr, /DOTCORE_SKIP_RUNTIME_CLEANUP must be 1 or unset/);
  assert.deepEqual(await refused.recorded(), []);
  await refused.assertIntact();
});

test("a declined workload teardown deletes nothing and stops the other cleans", async (t) => {
  const { assertIntact, recorded, run } = await fixture(t);

  const cleaned = run(["clean"], { FAKE_WORKLOAD_EXIT: "130" });

  assert.equal(cleaned.status, 130);
  assert.deepEqual(await recorded(), ["dotc dev workload teardown --all"]);
  await assertIntact();
});

test("a declined or failed site delete keeps the worker folder and scratch and stops the other cleans", async (t) => {
  for (const code of ["130", "1"]) {
    const { assertIntact, worker, recorded, run, site } = await fixture(t);

    const cleaned = run(["clean"], { FAKE_SITE_EXIT: code });

    assert.equal(cleaned.status, Number(code));
    assert.deepEqual(await recorded(), [
      "dotc dev workload teardown --all",
      `dotc site delete --site-dir ${site}`,
    ]);
    await assertIntact({ worker });
  }
});

test("an absent site directory is not deleted again and the rest still cleans", async (t) => {
  const { worker, exists, recorded, run, site } = await fixture(t);
  await rm(site, { force: true, recursive: true });

  const cleaned = run(["clean"]);

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual((await recorded()).slice(0, 2), [
    "dotc dev workload teardown --all",
    `dotc node leave --site-dir ${worker}`,
  ]);
  assert.equal(await exists(worker), false);
});

test("guards refuse before any teardown or deletion", async (t) => {
  const stack = await fixture(t);
  await stack.setLiveSocket(stack.socket);
  await writeFile(stack.socket, "");
  const stackRun = stack.run(["dev", "clean"]);
  assert.equal(stackRun.status, 2);
  assert.match(stackRun.stderr, /stop the development stack first: just dev::down, or rerun with DOTCORE_YES=1 to stop it/);
  assert.equal(await stack.exists(stack.socket), true, "the refusal removed a live socket");
  assert.deepEqual(await stack.recorded(), []);
  await stack.assertIntact();

  const worker = await fixture(t);
  await worker.setLiveSocket(`${worker.socket}-worker`);
  await writeFile(`${worker.socket}-worker`, "");
  const workerRun = worker.run(["dev", "clean"]);
  assert.equal(workerRun.status, 2);
  assert.match(workerRun.stderr, /stop the local worker first: just dev::cluster::down local, or rerun with DOTCORE_YES=1 to stop it/);
  assert.equal(await worker.exists(`${worker.socket}-worker`), true, "the refusal removed a live socket");
  assert.deepEqual(await worker.recorded(), []);
  await worker.assertIntact();

  const outside = await fixture(t);
  const outsideRun = outside.run(["dev", "clean"], { DOTCORE_SITE_DIR: "/somewhere/else/site" });
  assert.equal(outsideRun.status, 2);
  assert.match(outsideRun.stderr, /refusing to clean '\/somewhere\/else\/site': outside the workspace root/);
  assert.deepEqual(await outside.recorded(), []);
  await outside.assertIntact();
});

test("clean in a terminal stops the running stacks after one yes, then cleans", needsTerminal, async (t) => {
  const { exists, recorded, runInTerminal, setLiveSocket, site, socket, worker } = await fixture(t);
  await setLiveSocket([socket, `${socket}-worker`], { stoppable: true });

  const cleaned = runInTerminal("y\n", ["clean"]);
  assert.equal(cleaned.status, 0, cleaned.stdout);
  assert.match(cleaned.stdout, /These stacks are running and must be stopped first:\r?\n\* the local worker\r?\n\* the development stack\r?\nStop them and continue\? \[y\/N\]/);
  assert.equal(cleaned.stdout.match(/Stop them and continue\?/g).length, 1);
  assert.deepEqual((await recorded()).slice(0, 3), [
    `process-compose --use-uds --unix-socket ${socket}-worker down`,
    `process-compose --use-uds --unix-socket ${socket} down`,
    "dotc dev workload teardown --all",
  ]);
  assert.equal(await exists(site), false);
  assert.equal(await exists(worker), false);
});

test("clean in a terminal stops nothing and changes nothing after a no", needsTerminal, async (t) => {
  const { assertIntact, recorded, runInTerminal, setLiveSocket, socket } = await fixture(t);
  await setLiveSocket(socket, { stoppable: true });

  const declined = runInTerminal("n\n", ["clean"]);
  assert.equal(declined.status, 130, declined.stdout);
  assert.match(declined.stdout, /cleaning this workspace cancelled; nothing was changed/);
  assert.deepEqual(await recorded(), []);
  await assertIntact();
});

test("DOTCORE_YES stops a running stack without asking, then cleans", async (t) => {
  const { exists, recorded, run, setLiveSocket, site, socket } = await fixture(t);
  await setLiveSocket(socket, { stoppable: true });
  await writeFile(socket, "");

  const cleaned = run(["clean"], { DOTCORE_YES: "1" });
  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.doesNotMatch(cleaned.stderr, /Stop them and continue/);
  assert.deepEqual((await recorded()).slice(0, 2), [
    `process-compose --use-uds --unix-socket ${socket} down`,
    "dotc dev workload teardown --all --yes",
  ]);
  assert.equal(await exists(site), false);
  assert.equal(await exists(socket), false, "the stopped stack's socket remains");
});

test("a stack still running after its stop request fails before any teardown", async (t) => {
  const { assertIntact, recorded, run, setLiveSocket, socket } = await fixture(t);
  await setLiveSocket(socket);

  const stuck = run(["clean"], { DOTCORE_YES: "1" });
  assert.equal(stuck.status, 1);
  assert.match(stuck.stderr, /the development stack still runs after the stop; stop the development stack first: just dev::down/);
  assert.deepEqual(await recorded(), [`process-compose --use-uds --unix-socket ${socket} down`]);
  await assertIntact();
});

test("a socket file left behind by a stack that is gone is cleared instead of blocking the clean", async (t) => {
  const stack = await fixture(t);
  await writeFile(stack.socket, "");
  const cleaned = stack.run(["clean"]);

  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.equal(await stack.exists(stack.socket), false, "the stale stack socket remains");
  assert.equal(await stack.exists(stack.site), false);

  const worker = await fixture(t);
  await writeFile(`${worker.socket}-worker`, "");
  const workerCleaned = worker.run(["clean"]);

  assert.equal(workerCleaned.status, 0, workerCleaned.stderr);
  assert.equal(await worker.exists(`${worker.socket}-worker`), false, "the stale worker socket remains");
  assert.equal(await worker.exists(worker.worker), false);
});

test("an endpoint the client cannot read fails closed instead of passing for a stopped stack", async (t) => {
  const stack = await fixture(t);
  await stack.setBrokenProbe();
  await writeFile(stack.socket, "");

  const refused = stack.run(["clean"]);

  assert.equal(refused.status, 2);
  assert.match(refused.stderr, /cannot tell whether a development stack is running/);
  assert.deepEqual(await stack.recorded(), []);
  assert.equal(await stack.exists(stack.socket), true, "the unreadable socket was removed");
  await stack.assertIntact();
});

test("refuses a site folder that is a symlink to a folder outside the workspace", async (t) => {
  const f = await fixture(t);
  const target = await mkdtemp(path.join(tmpdir(), "dotcore-clean-recipe-outside-"));
  t.after(() => rm(target, { force: true, recursive: true }));
  await writeFile(path.join(target, "marker"), "state");
  const link = path.join(f.root, ".dotcore", "linked-site");
  await symlink(target, link);

  const run = f.run(["dev", "clean"], { DOTCORE_SITE_DIR: link });
  assert.equal(run.status, 2);
  assert.match(run.stderr, /refusing to clean '.*linked-site': outside the workspace root/);
  assert.deepEqual(await f.recorded(), []);
  assert.equal(await f.exists(path.join(target, "marker")), true);
  await f.assertIntact();
});

test("accepts a site folder spelled through a symlink to the workspace", async (t) => {
  const f = await fixture(t);
  const alias = `${f.root}-alias`;
  await symlink(f.root, alias);
  t.after(() => rm(alias, { force: true }));

  const run = f.run(["dev", "clean"], {
    DOTCORE_YES: "1",
    DOTCORE_SITE_DIR: path.join(alias, ".dotcore", "site"),
    DOTCORE_WORKER_DIR: path.join(alias, ".dotcore", "two-node-worker"),
  });
  assert.equal(run.status, 0, run.stderr);
  assert.equal(await f.exists(f.site), false);
});

const remoteEnvironment = { DOTCORE_REMOTE_CONFIG: "/config.json", FAKE_REMOTE_HOSTS: "gb10:spark apple:mac" };

test("clean says nothing about remote hosts when none are configured", async (t) => {
  const { recorded, run } = await fixture(t);

  const cleaned = run(["clean"], { DOTCORE_YES: "1" });
  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.doesNotMatch(cleaned.stderr, /remote hosts/);
  assert.equal((await recorded()).filter((call) => call.startsWith("remote ")).length, 0);
});

test("clean without a terminal lists the configured hosts without reaching them", async (t) => {
  const { recorded, run, site, exists } = await fixture(t);

  const cleaned = run(["clean"], remoteEnvironment);
  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.match(cleaned.stderr, /remote hosts are configured and were not cleaned: gb10:spark apple:mac; clean them with: just dev::remote::clean/);
  assert.deepEqual((await recorded()).filter((call) => call.startsWith("remote ")), ["remote hosts"]);
  assert.equal(await exists(site), false);
});

test("clean with an advance confirmation cleans every remote host before any local teardown", async (t) => {
  const { recorded, run, site, exists } = await fixture(t);

  const cleaned = run(["clean"], { ...remoteEnvironment, DOTCORE_YES: "1" });
  assert.equal(cleaned.status, 0, cleaned.stderr);
  const calls = await recorded();
  assert.deepEqual(calls.filter((call) => call.startsWith("remote clean")), ["remote clean gb10:spark", "remote clean apple:mac"]);
  assert.ok(calls.indexOf("remote clean apple:mac") < calls.indexOf("dotc dev workload teardown --all --yes"), calls.join("\n"));
  assert.equal(await exists(site), false);
});

test("clean with an advance confirmation stops before any local teardown when the remote clean fails", async (t) => {
  const { assertIntact, recorded, run } = await fixture(t);

  const cleaned = run(["clean"], { ...remoteEnvironment, DOTCORE_YES: "1", FAKE_REMOTE_CLEAN_EXIT: "1" });
  assert.equal(cleaned.status, 1);
  assert.match(cleaned.stderr, /remote clean failed; nothing local was changed/);
  assert.equal((await recorded()).filter((call) => call.startsWith("dotc ")).length, 0);
  await assertIntact();
});

test("clean fails before deleting anything when the configured hosts cannot be listed", async (t) => {
  const { assertIntact, recorded, run } = await fixture(t);

  const cleaned = run(["clean"], { ...remoteEnvironment, FAKE_REMOTE_HOSTS_EXIT: "1", DOTCORE_YES: "1" });
  assert.equal(cleaned.status, 2);
  assert.match(cleaned.stderr, /cannot list the configured remote hosts; fix DOTCORE_REMOTE_CONFIG or unset it/);
  assert.deepEqual(await recorded(), ["remote hosts"]);
  await assertIntact();
});

test("clean in a terminal cleans every remote host after one yes, before any local teardown", needsTerminal, async (t) => {
  const { recorded, runInTerminal, site, exists } = await fixture(t);

  const cleaned = runInTerminal("y\n", ["clean"], remoteEnvironment);
  assert.equal(cleaned.status, 0, cleaned.stdout);
  assert.match(cleaned.stdout, /Remote hosts are configured:\r?\n\* gb10:spark\r?\n\* apple:mac\r?\nAlso clean this workspace from these remote hosts\? Answer no to skip them\. \[y\/N\]/);
  assert.doesNotMatch(cleaned.stdout, /Clean (gb10:spark|apple:mac)\?/);
  const calls = await recorded();
  assert.deepEqual(calls.filter((call) => call.startsWith("remote clean")), ["remote clean gb10:spark", "remote clean apple:mac"]);
  assert.ok(calls.indexOf("remote clean apple:mac") < calls.indexOf("dotc dev workload teardown --all"), calls.join("\n"));
  assert.equal(await exists(site), false);
});

test("clean in a terminal skips the remote hosts after a no or an empty answer", needsTerminal, async (t) => {
  for (const answer of ["n\ny\ny\ny\n", "\ny\ny\ny\n"]) {
    const { recorded, runInTerminal, site, exists } = await fixture(t);

    const cleaned = runInTerminal(answer, ["clean"], remoteEnvironment);
    assert.equal(cleaned.status, 0, cleaned.stdout);
    assert.deepEqual((await recorded()).filter((call) => call.startsWith("remote ")), ["remote hosts"]);
    assert.equal(await exists(site), false);
  }
});

test("clean stops before any local teardown when the remote clean fails", needsTerminal, async (t) => {
  const { assertIntact, recorded, runInTerminal } = await fixture(t);

  const cleaned = runInTerminal("y\n", ["clean"], { ...remoteEnvironment, FAKE_REMOTE_CLEAN_EXIT: "1" });
  assert.notEqual(cleaned.status, 0);
  assert.match(cleaned.stdout, /remote clean failed; nothing local was changed/);
  assert.equal((await recorded()).filter((call) => call.startsWith("dotc ")).length, 0);
  await assertIntact();
});
