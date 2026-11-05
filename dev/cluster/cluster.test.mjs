//! Verifies the cluster recipes join, start, stop and remove local and remote workers, and keep the cluster record.
//!
//! Each test runs the real Justfiles from a temporary fake repository with recording stubs for dotc, the
//! build, Process Compose and the remote tool, so no site, stack or host is touched.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmod, copyFile, mkdir, mkdtemp, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repositoryRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");

test("join local builds, records the target, hands the token over and records the node id", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });

  const joined = f.run("dev::cluster::join", "local", "--", "--name", "Worker one", "--inference", "simulated", "--applications", "docker", "--host", "127.0.0.2");
  assert.equal(joined.status, 0, joined.stderr);
  const tokenFile = `${f.workerDir}.join-token`;
  assert.deepEqual(await f.calls(), [
    "nix build .#dotc --no-link",
    `dotc node add --site-dir ${f.site} --listen 127.0.0.1:50053 --yes --token-file ${tokenFile}`,
    `dotc node join --site-dir ${f.workerDir} --token-file ${tokenFile} --name Worker one --inference simulated --applications docker --host 127.0.0.2 --private-port 61053 --yes`,
  ]);
  assert.match(joined.stderr, /cluster join local: prepare\n(.*\n)*cluster join local: token\n(.*\n)*cluster join local: join on local\n(.*\n)*cluster join local: wait for node add/);
  assert.match(joined.stdout, /^DOTCORE_NODE_ID=node-1$/m);
  assert.equal(await f.record(), "local node-1\n");
  assert.equal((await stat(f.workerDir)).mode & 0o777, 0o700);
  await assert.rejects(stat(tokenFile), { code: "ENOENT" });

  await f.reset();
  const again = f.run("dev::cluster::join", "local", "--", "--name", "Worker two");
  assert.equal(again.status, 2);
  assert.match(again.stderr, /local already holds a worker of this cluster/);
  assert.deepEqual(await f.calls(), []);
});

test("join refuses while the manager runs without a terminal, before anything is built or recorded", async (t) => {
  const f = await fixture(t);
  await f.setLiveSocket(f.env.PC_SOCKET_PATH, { stoppable: true });

  const refused = f.run("dev::cluster::join", "local", "--", "--name", "w");
  assert.equal(refused.status, 2);
  assert.match(refused.stderr, /stop the manager first, node add needs the site lock: just dev::down, or rerun with DOTCORE_YES=1 to stop it/);
  assert.deepEqual(await f.calls(), []);
  assert.equal(await f.record(), null);
});

test("join confirmed in advance stops a running manager first, then joins", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await f.setLiveSocket(f.env.PC_SOCKET_PATH, { stoppable: true });
  f.env.DOTCORE_YES = "1";

  const joined = f.run("dev::cluster::join", "local", "--", "--name", "w");
  assert.equal(joined.status, 0, joined.stderr);
  const calls = await f.calls();
  assert.deepEqual(calls.slice(0, 2), [
    `process-compose site=${f.site} args=--use-uds --unix-socket ${f.env.PC_SOCKET_PATH} down`,
    "nix build .#dotc --no-link",
  ]);
  assert.equal(await f.record(), "local node-1\n");
});

test("join on a remote host resolves it, prepares through run and joins through exec with the token quoted", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });

  const joined = f.run("dev::cluster::join", "gb10", "--", "--name", "Spark one", "--note", "it's");
  assert.equal(joined.status, 0, joined.stderr);
  const calls = await f.calls();
  assert.deepEqual(calls.slice(0, 2), ["remote resolve gb10", "remote run gb10:spark dev::cluster::_worker-prepare"]);
  assert.match(calls[2], /^dotc node add .*--token-file /);
  assert.equal(calls[3], "remote exec gb10:spark just dev::cluster::_worker-join --token 'secret-token' '--name' 'Spark one' '--note' 'it'\\''s'");
  assert.equal(calls.length, 4);
  // The remote shell re-parses the command, so every argument survives as one word.
  assert.deepEqual(await f.remoteWords(), ["just", "dev::cluster::_worker-join", "--token", "secret-token", "--name", "Spark one", "--note", "it's"]);
  assert.doesNotMatch(joined.stdout + joined.stderr, /secret-token/);
  assert.equal(await f.record(), "gb10:spark node-1\n");
});

test("a failed join names the step, stops node add, and keeps the target recorded for cleanup", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  f.env.TEST_REMOTE_EXEC_STATUS = "1";

  const failed = f.run("dev::cluster::join", "apple", "--", "--name", "mac");
  assert.equal(failed.status, 1);
  assert.match(failed.stderr, /cluster join apple:mac: join failed/);
  // node add waits for the join, so only the trap that kills it lets the recipe return.
  await assert.rejects(stat(`${f.workerDir}.join-token`), { code: "ENOENT" });
  assert.equal(await f.record(), "apple:mac\n");
});

test("up and down act on the manager and every recorded worker, or on one target", async (t) => {
  const f = await fixture(t);
  await f.writeRecord("local node-1\ngb10:spark node-2\n");
  await mkdir(f.workerDir, { recursive: true });
  await writeFile(path.join(f.workerDir, "worker.yaml"), "joined node\n");

  const started = f.run("dev::cluster::up");
  assert.equal(started.status, 0, started.stderr);
  const startCalls = await f.calls();
  assert.equal(startCalls.length, 3);
  assert.match(startCalls[0], new RegExp(`^process-compose site=${escapeRegex(f.site)} args=--config dev/process-compose\\.yaml .*--unix-socket ${escapeRegex(f.env.PC_SOCKET_PATH)} --detached$`));
  assert.match(startCalls[1], new RegExp(`^process-compose site=${escapeRegex(f.workerDir)} args=--config dev/cluster/process-compose-worker\\.yaml .*--unix-socket ${escapeRegex(f.env.PC_SOCKET_PATH)}-worker --detached$`));
  assert.equal(startCalls[2], "remote run gb10:spark dev::cluster::_worker-up");

  await f.reset();
  await f.setLiveSocket(`${f.env.PC_SOCKET_PATH}-worker`);
  const stopped = f.run("dev::cluster::down");
  assert.equal(stopped.status, 0, stopped.stderr);
  assert.deepEqual(await f.calls(), [
    `process-compose site=${f.site} args=--use-uds --unix-socket ${f.env.PC_SOCKET_PATH}-worker down`,
    "remote exec gb10:spark just dev::cluster::_worker-down",
  ]);
  assert.match(stopped.stdout, /no development stack is running/);

  // A stopped worker is left alone, without a failed stop request.
  await f.setLiveSocket(path.join(f.root, "nothing-live"));
  await f.reset();
  const idle = f.run("dev::cluster::down", "local");
  assert.equal(idle.status, 0, idle.stderr);
  assert.deepEqual(await f.calls(), []);
  assert.equal(idle.stderr, "");

  await f.reset();
  assert.equal(f.run("dev::cluster::up", "gb10").status, 0);
  assert.equal(f.run("dev::cluster::logs", "gb10:spark", "50").status, 0);
  assert.deepEqual(await f.calls(), [
    "remote resolve gb10",
    "remote run gb10:spark dev::cluster::_worker-up",
    "remote resolve gb10:spark",
    "remote exec gb10:spark just dev::cluster::_worker-logs 50",
  ]);
});

test("clean with a target makes that worker leave and removes its node, refusing first what it cannot finish", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await f.writeRecord("local node-1\ngb10:spark node-2\n");
  await writeFile(f.env.TEST_NODES, "node-1\nnode-2\n");

  const unconfirmed = f.run("dev::cluster::clean", "gb10");
  assert.equal(unconfirmed.status, 2);
  assert.match(unconfirmed.stderr, /cleaning the remote workers needs confirmation and there is no terminal to ask in; run it in a terminal, or rerun with DOTCORE_YES=1/);
  assert.deepEqual(await f.calls(), ["remote resolve gb10"]);
  await f.reset();

  // A manager still running after its stop request keeps the worker from leaving.
  f.env.DOTCORE_YES = "1";
  await f.setLiveSocket(f.env.PC_SOCKET_PATH);
  const managerStuck = f.run("dev::cluster::clean", "gb10");
  assert.equal(managerStuck.status, 1);
  assert.match(managerStuck.stderr, /the manager still runs after the stop; stop the manager first, node remove needs the site lock: just dev::down/);
  assert.deepEqual(await f.calls(), ["remote resolve gb10", `process-compose site=${f.site} args=--use-uds --unix-socket ${f.env.PC_SOCKET_PATH} down`]);

  await f.setLiveSocket(f.env.PC_SOCKET_PATH, { stoppable: true });
  await f.reset();
  const cleaned = f.run("dev::cluster::clean", "gb10");
  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual(await f.calls(), [
    "remote resolve gb10",
    `process-compose site=${f.site} args=--use-uds --unix-socket ${f.env.PC_SOCKET_PATH} down`,
    "remote exec gb10:spark just dev::cluster::_worker-down",
    "remote exec gb10:spark DOTCORE_YES=1 just dev::cluster::_worker-leave",
    `dotc node list --site-dir ${f.site}`,
    `dotc node remove --site-dir ${f.site} node-2 --forget-deployments --yes`,
  ]);
  assert.equal(await f.record(), "local node-1\n");
  assert.equal(await readFile(f.env.TEST_NODES, "utf8"), "node-1\n");
});

test("a worker leave confirmed in advance stops a running worker first", async (t) => {
  const f = await fixture(t);
  await mkdir(f.workerDir, { recursive: true });
  await writeFile(path.join(f.workerDir, "worker.yaml"), "joined node\n");
  await f.setLiveSocket(`${f.env.PC_SOCKET_PATH}-worker`, { stoppable: true });
  f.env.DOTCORE_YES = "1";

  const left = f.run("dev::cluster::_worker-leave");
  assert.equal(left.status, 0, left.stderr);
  assert.deepEqual(await f.calls(), [
    `process-compose site=${f.site} args=--use-uds --unix-socket ${f.env.PC_SOCKET_PATH}-worker down`,
    `dotc node leave --site-dir ${f.workerDir} --yes`,
  ]);
  assert.equal(await f.exists(f.workerDir), false);
});

test("clean without a target stops everything, makes every worker leave, then deletes the manager site", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await mkdir(f.workerDir, { recursive: true });
  await writeFile(path.join(f.workerDir, "worker.yaml"), "joined node\n");
  await f.writeRecord("local node-1\ngb10:spark node-2\n");
  f.env.DOTCORE_YES = "1";

  // A worker that does not leave keeps the manager site, and the record keeps only what is left.
  f.env.TEST_REMOTE_EXEC_STATUS = "1";
  const failed = f.run("dev::cluster::clean");
  assert.equal(failed.status, 1);
  assert.match(failed.stderr, /the worker on gb10:spark did not leave; the manager site is kept/);
  assert.equal(await f.exists(f.site), true);
  assert.equal(await f.exists(f.workerDir), false);
  assert.equal(await f.record(), "gb10:spark node-2\n");

  delete f.env.TEST_REMOTE_EXEC_STATUS;
  await f.reset();
  const cleaned = f.run("dev::cluster::clean");
  assert.equal(cleaned.status, 0, cleaned.stderr);
  const calls = await f.calls();
  assert.deepEqual(calls.slice(0, 2), [
    "remote exec gb10:spark just dev::cluster::_worker-down",
    "remote exec gb10:spark DOTCORE_YES=1 just dev::cluster::_worker-leave",
  ]);
  assert.deepEqual(calls.slice(2).filter((call) => call.startsWith("dotc")), [
    "dotc dev workload teardown --all --yes",
    `dotc site delete --site-dir ${f.site} --yes`,
  ]);
  assert.equal(await f.exists(f.site), false);
  assert.equal(await f.record(), null);
});

test("remote clean removes the host's worker through the cluster, then the remote workspace", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await f.writeRecord("gb10:spark node-2\n");
  await writeFile(f.env.TEST_NODES, "node-2\n");

  const unconfirmed = f.run("dev::remote::clean", "gb10");
  assert.equal(unconfirmed.status, 2);
  assert.match(unconfirmed.stderr, /cleaning gb10:spark needs confirmation and there is no terminal to ask in; run it in a terminal, or rerun with DOTCORE_YES=1/);
  assert.deepEqual(await f.calls(), ["remote resolve gb10", "remote status gb10:spark"]);
  await f.reset();

  f.env.DOTCORE_YES = "1";
  f.env.TEST_REMOTE_LOCK = "held by run abc (pid 1)";
  const locked = f.run("dev::remote::clean", "gb10");
  assert.equal(locked.status, 1);
  assert.match(locked.stderr, /a run holds the remote workspace on gb10:spark/);
  assert.deepEqual(await f.calls(), ["remote resolve gb10", "remote status gb10:spark"]);

  delete f.env.TEST_REMOTE_LOCK;
  await f.reset();
  const cleaned = f.run("dev::remote::clean", "gb10");
  assert.equal(cleaned.status, 0, cleaned.stderr);
  assert.deepEqual(await f.calls(), [
    "remote resolve gb10",
    "remote status gb10:spark",
    "remote resolve gb10:spark",
    "remote exec gb10:spark just dev::cluster::_worker-down",
    "remote exec gb10:spark DOTCORE_YES=1 just dev::cluster::_worker-leave",
    `dotc node list --site-dir ${f.site}`,
    `dotc node remove --site-dir ${f.site} node-2 --forget-deployments --yes`,
    "remote clean gb10:spark",
  ]);
  assert.equal(await f.record(), null);

  // An unrecorded host is checked against a manager site that still exists, and passes quietly once it is gone.
  const unrecorded = f.run("dev::remote::clean", "gb10");
  assert.equal(unrecorded.status, 0, unrecorded.stderr);
  assert.match(unrecorded.stderr, /cluster clean: no recorded node id for gb10:spark; check the manager with dotc node list/);

  await rm(f.site, { recursive: true });
  const siteless = f.run("dev::remote::clean", "gb10");
  assert.equal(siteless.status, 0, siteless.stderr);
  assert.equal(siteless.stderr, "");
});

test("remote clean without a host cleans every configured host, and a failing host does not stop the others", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await f.writeRecord("gb10:spark node-1\napple:mac node-2\n");
  await writeFile(f.env.TEST_NODES, "node-1\nnode-2\n");
  f.env.TEST_HOSTS = "gb10:spark apple:mac";
  f.env.DOTCORE_YES = "1";

  const cleaned = f.run("dev::remote::clean");
  assert.equal(cleaned.status, 0, cleaned.stderr);
  const calls = await f.calls();
  assert.equal(calls[0], "remote hosts");
  assert.deepEqual(calls.filter((call) => call.startsWith("remote clean")), ["remote clean gb10:spark", "remote clean apple:mac"]);
  assert.equal(await f.record(), null);
  await f.reset();

  await f.writeRecord("gb10:spark node-1\napple:mac node-2\n");
  await writeFile(f.env.TEST_NODES, "node-1\nnode-2\n");
  f.env.TEST_STATUS_FAIL = "gb10:spark";
  const partial = f.run("dev::remote::clean");
  assert.equal(partial.status, 1);
  assert.match(partial.stderr, /remote clean failed on: gb10:spark/);
  assert.deepEqual((await f.calls()).filter((call) => call.startsWith("remote clean")), ["remote clean apple:mac"]);
  assert.equal(await f.record(), "gb10:spark node-1\n");
});

test("remote clean without a host or a terminal refuses every host, and fails when nothing is configured", async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  f.env.TEST_HOSTS = "gb10:spark apple:mac";

  const refused = f.run("dev::remote::clean");
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, /cleaning gb10:spark needs confirmation and there is no terminal/);
  assert.match(refused.stderr, /cleaning apple:mac needs confirmation and there is no terminal/);
  assert.match(refused.stderr, /remote clean failed on: gb10:spark apple:mac/);
  assert.deepEqual((await f.calls()).filter((call) => call.startsWith("remote clean")), []);
  await f.reset();

  delete f.env.TEST_HOSTS;
  const unconfigured = f.run("dev::remote::clean");
  assert.notEqual(unconfigured.status, 0);
  assert.match(unconfigured.stderr, /remote execution is not configured/);
  assert.deepEqual(await f.calls(), ["remote hosts"]);
});

test("remote clean without a host asks per host and skips a declined one", { skip: !terminalTool() && "needs util-linux script for a pseudo-terminal" }, async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await f.writeRecord("gb10:spark node-1\napple:mac node-2\n");
  await writeFile(f.env.TEST_NODES, "node-1\nnode-2\n");
  f.env.TEST_HOSTS = "gb10:spark apple:mac";

  const result = f.runInTerminal("n\ny\n", "dev::remote::clean");
  assert.equal(result.status, 0, result.stdout);
  assert.match(result.stdout, /Clean gb10:spark\?/);
  assert.match(result.stdout, /Clean apple:mac\?/);
  assert.deepEqual((await f.calls()).filter((call) => call.startsWith("remote clean")), ["remote clean apple:mac"]);
  assert.equal(await f.record(), "gb10:spark node-1\n");
});

test("remote clean in a terminal shows what it removes and asks once before any change", { skip: !terminalTool() && "needs util-linux script for a pseudo-terminal" }, async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await f.writeRecord("gb10:spark node-2\n");
  await writeFile(f.env.TEST_NODES, "node-2\n");

  for (const answer of ["n\n", "\n", ""]) {
    const declined = f.runInTerminal(answer, "dev::remote::clean", "gb10");
    assert.equal(declined.status, 130, `${JSON.stringify(answer)}: ${declined.stdout}`);
    assert.match(declined.stdout, /cleaning gb10:spark cancelled; nothing was changed/);
    assert.deepEqual(await f.calls(), ["remote resolve gb10", "remote status gb10:spark"]);
    await f.reset();
  }
  assert.equal(await f.record(), "gb10:spark node-2\n");

  // A busy workspace fails before the question.
  f.env.TEST_REMOTE_LOCK = "held by run abc (pid 1)";
  const locked = f.runInTerminal("y\n", "dev::remote::clean", "gb10");
  assert.equal(locked.status, 1);
  assert.doesNotMatch(locked.stdout, /Clean gb10:spark\?/);
  delete f.env.TEST_REMOTE_LOCK;
  await f.reset();

  f.env.TEST_REMOTE_CHANGES = "\n  M notes.txt";
  const cleaned = f.runInTerminal("y\n", "dev::remote::clean", "gb10");
  assert.equal(cleaned.status, 0, cleaned.stdout);
  assert.match(cleaned.stdout, /Cleaning gb10:spark removes:\r?\n\* worker node-2: it leaves the cluster, which stops its native workloads and Docker resources and deletes its folder, and the manager site forgets its deployments\r?\n\* the remote workspace \/remote\/workspace, with its run records and branches\r?\n  its uncommitted remote changes then stay only in the host's jj history\r?\nClean gb10:spark\? \[y\/N\]/);
  assert.equal(cleaned.stdout.match(/\[y\/N\]/g).length, 1);
  // The answer confirms the leave and the node removal, so the run matches an advance DOTCORE_YES=1.
  assert.deepEqual(await f.calls(), [
    "remote resolve gb10",
    "remote status gb10:spark",
    "remote resolve gb10:spark",
    "remote exec gb10:spark just dev::cluster::_worker-down",
    "remote exec gb10:spark DOTCORE_YES=1 just dev::cluster::_worker-leave",
    `dotc node list --site-dir ${f.site}`,
    `dotc node remove --site-dir ${f.site} node-2 --forget-deployments --yes`,
    "remote clean gb10:spark",
  ]);
  assert.equal(await f.record(), null);
});

test("cluster clean in a terminal asks once for the remote workers and leaves local steps to dotc", { skip: !terminalTool() && "needs util-linux script for a pseudo-terminal" }, async (t) => {
  const f = await fixture(t);
  await mkdir(f.site, { recursive: true });
  await mkdir(f.workerDir, { recursive: true });
  await writeFile(path.join(f.workerDir, "worker.yaml"), "joined node\n");
  await f.writeRecord("local node-1\ngb10:spark node-2\napple:mac\n");

  const declined = f.runInTerminal("n\n", "dev::cluster::clean");
  assert.equal(declined.status, 130, declined.stdout);
  assert.deepEqual(await f.calls(), []);
  assert.equal(await f.exists(f.workerDir), true);

  const unconfirmed = f.run("dev::cluster::clean");
  assert.equal(unconfirmed.status, 2);
  assert.deepEqual(await f.calls(), []);

  const cleaned = f.runInTerminal("y\n", "dev::cluster::clean");
  assert.equal(cleaned.status, 0, cleaned.stdout);
  assert.match(cleaned.stdout, /Cleaning the cluster removes these remote workers:\r?\n\* worker node-2 on gb10:spark: it leaves the cluster, which stops its native workloads and Docker resources and deletes its folder\r?\n\* worker without a completed join on apple:mac: it leaves the cluster, [^\n]*\r?\nMake these workers leave\? \[y\/N\]/);
  assert.equal(cleaned.stdout.match(/\[y\/N\]/g).length, 1);
  const calls = await f.calls();
  assert.deepEqual(calls.filter((call) => call.startsWith("remote exec")), [
    "remote exec gb10:spark just dev::cluster::_worker-down",
    "remote exec apple:mac just dev::cluster::_worker-down",
    "remote exec gb10:spark DOTCORE_YES=1 just dev::cluster::_worker-leave",
    "remote exec apple:mac DOTCORE_YES=1 just dev::cluster::_worker-leave",
  ]);
  // The local worker and the manager site still confirm through dotc, so they run without --yes.
  assert.deepEqual(calls.filter((call) => call.startsWith("dotc")), [
    `dotc node leave --site-dir ${f.workerDir}`,
    "dotc dev workload teardown --all",
    `dotc site delete --site-dir ${f.site}`,
  ]);
  assert.equal(await f.record(), null);
});

test("two-node-init joins a local worker, and the local worker stops and leaves with its folder", async (t) => {
  const f = await fixture(t);

  const noSelector = f.run("dev::cluster::two-node-init", "nvidia", "docker");
  assert.equal(noSelector.status, 2);
  assert.match(noSelector.stderr, /inference nvidia requires an nvidia_device CDI selector/);

  const initialized = f.run("dev::cluster::two-node-init", "nvidia", "docker", "Spark", "127.0.0.2", "nvidia.com/gpu=0");
  assert.equal(initialized.status, 0, initialized.stderr);
  const initCalls = (await f.calls()).filter((call) => call.startsWith("dotc"));
  assert.match(initCalls[0], /^dotc site init .*--inference nvidia --applications docker/);
  assert.match(initCalls[2], /--name Spark --host 127\.0\.0\.2 --inference nvidia --applications docker --nvidia-device nvidia\.com\/gpu=0 --private-port 61053 --yes$/);
  assert.equal(await f.record(), "local node-1\n");

  const again = f.run("dev::cluster::two-node-init", "simulated", "docker");
  assert.equal(again.status, 2);
  assert.match(again.stderr, /worker node folder already exists/);

  // A worker that is still running after the stop owns its folder, so it does not leave.
  await f.setLiveSocket(`${f.env.PC_SOCKET_PATH}-worker`);
  const blocked = f.run("dev::cluster::clean", "local");
  assert.equal(blocked.status, 1);
  assert.match(blocked.stderr, /stop the worker before it leaves(.*\n)*cluster clean: the worker on local did not leave/);
  assert.equal(await f.exists(path.join(f.workerDir, "worker.yaml")), true);

  await f.setLiveSocket(path.join(f.root, "nothing-live"));
  await f.reset();
  const cleaned = f.run("dev::cluster::clean", "local");
  assert.equal(cleaned.status, 0, cleaned.stderr);
  // The worker's workloads stop through the node leave rather than being orphaned by a plain removal.
  assert.deepEqual((await f.calls()).filter((call) => call.startsWith("dotc")), [
    `dotc node leave --site-dir ${f.workerDir}`,
    `dotc node list --site-dir ${f.site}`,
    `dotc node remove --site-dir ${f.site} node-1 --forget-deployments`,
  ]);
  assert.equal(await f.exists(f.workerDir), false);
  assert.equal(await f.record(), null);

  // A folder without worker.yaml (a failed join) falls back to the site delete.
  await mkdir(f.workerDir);
  await f.reset();
  f.env.DOTCORE_YES = "1";
  assert.equal(f.run("dev::cluster::clean", "local").status, 0);
  assert.deepEqual((await f.calls()).filter((call) => call.startsWith("dotc")), [`dotc site delete --site-dir ${f.workerDir} --skip-runtime-cleanup --yes`]);

  // Apple targets are ephemeral on both nodes, so the development stack leaves no native orphans.
  await rm(f.site, { recursive: true });
  await f.reset();
  const apple = f.run("dev::cluster::two-node-init", "apple", "simulated", "Mac", "127.0.0.2");
  assert.equal(apple.status, 0, apple.stderr);
  const appleCalls = (await f.calls()).filter((call) => call.startsWith("dotc"));
  assert.match(appleCalls[0], /^dotc site init .*--inference apple --applications simulated --non-interactive --native-workload-lifetime ephemeral$/);
  assert.match(appleCalls[2], /--inference apple --applications simulated --native-workload-lifetime ephemeral --private-port 61053 --yes$/);
});

test("two-node-init gives the worker its own simulated hardware", async (t) => {
  const f = await fixture(t);

  const initialized = f.run("dev::cluster::two-node-init", "simulated:gb10", "docker", "Mac", "127.0.0.2", "", "simulated:mac-64g");

  assert.equal(initialized.status, 0, initialized.stderr);
  const calls = (await f.calls()).filter((call) => call.startsWith("dotc"));
  assert.match(calls[0], /^dotc site init .*--inference simulated:gb10 --applications docker --non-interactive$/);
  assert.match(calls[2], /--name Mac --host 127\.0\.0\.2 --inference simulated:mac-64g --applications docker --private-port 61053 --yes$/);
});

// A fake repository holding the real root, dev, remote and cluster Justfiles, with recording stubs for dotc,
// nix, Process Compose and the remote tool. The remote tool runs as `node dev/remote/remote-exec.mjs`, so a
// `node` stub on PATH stands in for it and hands every other use to the real node.
async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-cluster-"));
  t.after(() => rm(root, { force: true, recursive: true }));
  const bin = path.join(root, "bin");
  await mkdir(bin);
  for (const file of ["Justfile", "dev/Justfile", "dev/pc-endpoint.sh", "dev/confirm.sh", "dev/site-choices.sh", "dev/remote/Justfile", "dev/cluster/Justfile", "dev/cluster/targets.sh", "dev/cluster/process-compose-worker.yaml"]) {
    await mkdir(path.dirname(path.join(root, file)), { recursive: true });
    await copyFile(path.join(repositoryRoot, file), path.join(root, file));
  }
  for (const module of ["frontend", "backend", "proto", "apps", "engines", "installer", "ci"]) {
    await mkdir(path.join(root, module));
    // The manager stack refuses to start unbootstrapped, so the stubs report a bootstrapped workspace.
    await writeFile(path.join(root, module, "Justfile"), "clean:\n  @true\n\nbootstrap-check:\n  @true\n");
  }
  const site = path.join(root, ".dotcore", "site");
  const workerDir = path.join(root, ".dotcore", "two-node-worker");
  const callsFile = path.join(root, "calls");

  const stub = async (name, body) => {
    await writeFile(path.join(bin, name), `#!/bin/sh\n${body}`);
    await chmod(path.join(bin, name), 0o755);
  };
  // node add publishes its token, waits for the join, then reports the next node id, as the real command does.
  await stub("dotc", `printf 'dotc %s\\n' "$*" >> "$TEST_CALLS"
previous=""
for argument in "$@"; do
  case "$previous" in
    --token-file) token_file="$argument" ;;
    --site-dir) site_dir="$argument" ;;
  esac
  previous="$argument"
done
case "$*" in
  "node add"*)
    printf 'secret-token\\n' > "$token_file"
    for _ in $(seq 1 100); do test ! -e "$TEST_JOINED" || break; sleep 0.1; done
    test -e "$TEST_JOINED" || exit 1
    rm -f "$TEST_JOINED"
    id="node-$(( $(cat "$TEST_NODES" 2>/dev/null | wc -l) + 1 ))"
    echo "$id" >> "$TEST_NODES"
    printf 'Node added\\nDOTCORE_NODE_ID=%s\\n' "$id" ;;
  "node join"*)
    mkdir -m 700 -p "$site_dir"
    printf 'joined node\\n' > "$site_dir/worker.yaml"
    touch "$TEST_JOINED" ;;
  "node list"*) sed 's/^/DOTCORE_NODE_ID=/' "$TEST_NODES" ;;
  "node remove"*) id="$5"; grep -vx "$id" "$TEST_NODES" > "$TEST_NODES.tmp" || true; mv "$TEST_NODES.tmp" "$TEST_NODES" ;;
  "node leave"*|"site delete"*) rm -rf "$site_dir" ;;
  "site init"*) mkdir -p "$site_dir" ;;
esac
`);
  await stub("nix", `printf 'nix %s\\n' "$*" >> "$TEST_CALLS"\n`);
  await stub("node", `if [ "$1" != dev/remote/remote-exec.mjs ]; then exec "${process.execPath}" "$@"; fi
shift
printf 'remote %s\\n' "$*" >> "$TEST_CALLS"
case "$1" in
  resolve) case "$2" in gb10|gb10:spark) echo gb10:spark ;; apple|apple:mac) echo apple:mac ;; *) echo "$2" ;; esac ;;
  hosts) if [ -n "\${TEST_HOSTS:-}" ]; then printf '%s\\n' $TEST_HOSTS; else echo 'remote execution is not configured' >&2; exit 125; fi ;;
  status) if [ "$2" = "\${TEST_STATUS_FAIL:-}" ]; then echo "cannot reach $2" >&2; exit 125; fi
    printf 'workspace: /remote/workspace (present)\\nchanges: %s\\nlock: %s\\nruns: none\\n' "\${TEST_REMOTE_CHANGES:-none}" "\${TEST_REMOTE_LOCK:-free}" ;;
  exec)
    shift 2
    printf '%s' "$*" > "$TEST_REMOTE_COMMAND"
    case "$*" in *_worker-join*|*_worker-leave*)
      test "\${TEST_REMOTE_EXEC_STATUS:-0}" = 0 || exit "$TEST_REMOTE_EXEC_STATUS"
      case "$*" in *_worker-join*) touch "$TEST_JOINED" ;; esac ;;
    esac ;;
esac
exit 0
`);
  // Only the endpoint a test marks live answers the guards' probe; probes are not recorded, so call
  // assertions stay about real controls. A stoppable stack stops on its stop request.
  const setLiveSocket = (liveSocket, { stoppable = false } = {}) => stub("process-compose", `case "$*" in
  *"project state"*)
    case "$*" in *"--unix-socket ${liveSocket} project state"*) test -e "${liveSocket}.stopped" || exit 0 ;; esac
    printf 'dial unix socket: connect: connection refused\\n' >&2
    exit 1 ;;
  *" down") ${stoppable ? 'touch "$3.stopped"' : ":"} ;;
esac
printf 'process-compose site=%s args=%s\\n' "$DOTCORE_SITE_DIR" "$*" >> "$TEST_CALLS"
`);
  await setLiveSocket(path.join(root, "nothing-live"));

  const env = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    DOTCORE_MACHINE_HOST: "127.0.0.1",
    DOTCORE_SITE_DIR: site,
    DOTCORE_CATALOG_DIR: path.join(root, "catalog"),
    DOTCORE_WORKER_DIR: workerDir,
    DOTCORE_WORKER_PRIVATE_PORT: "61053",
    DOTCORE_PRIVATE_ADDR: "127.0.0.1:50053",
    PC_SOCKET_PATH: path.join(root, "pc.sock"),
    TEST_CALLS: callsFile,
    TEST_JOINED: path.join(root, "joined"),
    TEST_NODES: path.join(root, "nodes"),
    TEST_REMOTE_COMMAND: path.join(root, "remote-command"),
  };
  delete env.DOTCORE_YES;
  delete env.DOTCORE_REMOTE_CONFIG;
  delete env.DOTCORE_SKIP_RUNTIME_CLEANUP;

  const exists = (file) => stat(file).then(() => true, () => false);
  return {
    env,
    root,
    site,
    workerDir,
    setLiveSocket,
    exists,
    run: (...args) => spawnSync("just", args, { cwd: root, encoding: "utf8", env }),
    // Runs the recipe in a pseudo-terminal that types `answer`; the terminal merges stderr into stdout.
    runInTerminal: (answer, ...args) => spawnSync("script", terminalArgs(["just", ...args]), { cwd: root, encoding: "utf8", env, input: answer }),
    calls: async () => (await readFile(callsFile, "utf8").catch(() => "")).split("\n").filter(Boolean),
    record: () => readFile(`${site}.cluster`, "utf8").catch(() => null),
    writeRecord: async (text) => {
      await mkdir(path.dirname(site), { recursive: true });
      await writeFile(`${site}.cluster`, text);
    },
    remoteWords: async () => {
      const command = await readFile(env.TEST_REMOTE_COMMAND, "utf8");
      const parsed = spawnSync("sh", ["-c", `eval "set -- $1"; for a in "$@"; do printf '%s\\0' "$a"; done`, "sh", command], { encoding: "utf8" });
      return parsed.stdout.split("\0").slice(0, -1);
    },
    reset: () => rm(callsFile, { force: true }),
  };
}

// Whether util-linux `script` can give a recipe a pseudo-terminal. The BSD `script` on macOS refuses the socket
// node passes as its input, so the terminal cases run on Linux only.
function terminalTool() {
  const version = spawnSync("script", ["--version"], { encoding: "utf8" });
  return !version.error && `${version.stdout}${version.stderr}`.includes("util-linux");
}

function terminalArgs(command) {
  return ["-qec", command.map(shellQuote).join(" "), "/dev/null"];
}

function shellQuote(word) {
  return `'${word.replace(/'/g, "'\\''")}'`;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}
