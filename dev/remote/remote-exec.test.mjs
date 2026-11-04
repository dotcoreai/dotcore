//! Verifies the remote execution tool: configuration, naming, command construction, and the
//! sync, run, lock and listing behavior against a loopback transport that stands in for ssh.
//!
//! The loopback uses real jj and git repositories in a temporary directory, with a local shell
//! instead of ssh and small `direnv` and `just` stand-ins, so no remote host is needed.

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmod, copyFile, mkdir, mkdtemp, readFile, realpath, rm, stat, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  ExitMarkerFilter,
  TOOL_FAILURE_EXIT,
  bootstrapCommand,
  gitSshCommand,
  main,
  parseConfig,
  remoteWorkspaceName,
  resolveHost,
  spawnStreaming,
  sshOptions,
} from "./remote-exec.mjs";

const toolSource = path.join(path.dirname(fileURLToPath(import.meta.url)), "remote-exec.mjs");
const machine = "test-machine";
const haveJj = spawnSync("jj", ["--version"]).status === 0;
const needsJj = { skip: haveJj ? false : "jj is not installed" };

const CONFIG = {
  groups: {
    gb10: {
      user: "dev",
      key: "~/.ssh/spark",
      dir: "/home/dev/dotcore",
      hosts: { one: "one.example.net", two: { address: "two.example.net", dir: "/srv/dotcore", key: "/keys/two" } },
    },
    apple: { dir: "/Users/dev/dotcore", hosts: { mac: "mac.example.net" } },
  },
};

test("config resolves group defaults, host overrides and the first host of a group", () => {
  const config = parseConfig(JSON.stringify(CONFIG));

  assert.deepEqual(resolveHost(config, "gb10"), {
    group: "gb10", name: "one", address: "one.example.net", user: "dev", key: "~/.ssh/spark", dir: "/home/dev/dotcore",
  });
  assert.deepEqual(resolveHost(config, "gb10:two"), {
    group: "gb10", name: "two", address: "two.example.net", user: "dev", key: "/keys/two", dir: "/srv/dotcore",
  });
  assert.deepEqual(resolveHost(config, "apple"), {
    group: "apple", name: "mac", address: "mac.example.net", user: undefined, key: undefined, dir: "/Users/dev/dotcore",
  });
});

test("config errors name the problem and the allowed values", () => {
  const config = parseConfig(JSON.stringify(CONFIG));
  assert.throws(() => resolveHost(config, "nope"), /unknown host group in "nope"; configured: gb10, apple/);
  assert.throws(() => resolveHost(config, "gb10:three"), /unknown host "three" in group gb10; configured: one, two/);
  assert.throws(() => parseConfig("{"), /not valid json/);
  assert.throws(() => parseConfig(JSON.stringify({ groups: { a: { hosts: { h: "x" }, kee: 1, dir: "/d" } } })), /unknown keys: kee \(allowed: user, key, dir, hosts\)/);
  assert.throws(() => parseConfig(JSON.stringify({ groups: { a: { hosts: { h: "x" } } } })), /dir \(absolute path without spaces\)/);
  assert.throws(() => parseConfig(JSON.stringify({ groups: { a: { dir: "/a b", hosts: { h: "x" } } } })), /dir/);
});

test("remote workspace names are marked, sanitized and distinct per root and machine", () => {
  const name = remoteWorkspaceName("my/ws name", "/work/a", machine);
  assert.match(name, /^remote-my-ws-name-[0-9a-f]{8}$/);
  assert.notEqual(name, remoteWorkspaceName("my/ws name", "/work/b", machine));
  assert.notEqual(name, remoteWorkspaceName("my/ws name", "/work/a", "other"));
  assert.equal(name, remoteWorkspaceName("my/ws name", "/work/a", machine));
});

test("ssh and git share one option set, and the key is optional", () => {
  const withKey = { user: "dev", key: "~/.ssh/spark" };
  const env = { HOME: "/home/me" };
  assert.deepEqual(sshOptions(withKey, env), [
    "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ForwardAgent=no", "-i", "/home/me/.ssh/spark", "-o", "IdentitiesOnly=yes", "-l", "dev",
  ]);
  assert.equal(
    gitSshCommand(withKey, env),
    "ssh -o BatchMode=yes -o ConnectTimeout=10 -o ForwardAgent=no -i /home/me/.ssh/spark -o IdentitiesOnly=yes -l dev",
  );
  assert.deepEqual(sshOptions({}, env), ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ForwardAgent=no"]);
});

test("the bootstrap command survives any login shell quoting", () => {
  const command = bootstrapCommand({ dir: "/home/dev/dotcore", sha: "a".repeat(40), payload: { args: ["it's", "a\\b"] } });
  const script = command.slice("sh -c '".length, -1);
  assert.ok(command.startsWith("sh -c '") && command.endsWith("'"));
  assert.ok(!script.includes("'") && !script.includes("\\"), script);
  assert.match(script, /git -C \/home\/dev\/dotcore cat-file blob a{40}:dev\/remote\/remote-exec\.mjs/);
  assert.match(script, /export DIRENV_LOG_FORMAT=; direnv exec \/home\/dev\/dotcore node/);
});

test("the exit marker is removed from output split at any point", () => {
  const output = "line one\nno newline at end";
  const stream = `${output}\n@@remote-exec-exit@@42\n`;
  for (let cut = 1; cut < stream.length; cut += 1) {
    let seen = "";
    const filter = new ExitMarkerFilter((text) => (seen += text));
    filter.push(stream.slice(0, cut));
    filter.push(stream.slice(cut));
    filter.end();
    assert.equal(seen, output, `cut at ${cut}`);
    assert.equal(filter.code, 42);
  }

  let seen = "";
  const filter = new ExitMarkerFilter((text) => (seen += text));
  filter.push("partial @@rem");
  filter.end();
  assert.equal(seen, "partial @@rem");
  assert.equal(filter.code, null);
});

test("a transport failure without the marker is reported apart from a command status", async () => {
  const io = collectingIo({ env: {}, transport: {} });
  io.env = { DOTCORE_REMOTE_CONFIG: "/nonexistent/config.json" };
  assert.equal(await main(["run", "gb10", "x"], io), TOOL_FAILURE_EXIT);
  assert.match(io.err, /cannot read DOTCORE_REMOTE_CONFIG file/);
  assert.equal(await main(["bogus"], io), TOOL_FAILURE_EXIT);
  assert.match(io.err, /usage:/);
});

test("the tool runs as the entry script when reached through a symlinked path", async (t) => {
  const root = await realpath(await mkdtemp(path.join(os.tmpdir(), "dotcore-remote-exec-link-")));
  t.after(() => rm(root, { force: true, recursive: true }));
  await symlink(path.dirname(toolSource), path.join(root, "link"));

  const result = spawnSync("node", [path.join(root, "link", "remote-exec.mjs"), "list"], { encoding: "utf8", env: { ...process.env, DOTCORE_REMOTE_CONFIG: "" } });
  assert.equal(result.status, 1);
  assert.match(result.stdout, /remote execution is not configured/);
});

test("list without configuration explains the setup and fails", async () => {
  const io = collectingIo({ env: {}, transport: {} });
  assert.equal(await main(["list"], io), 1);
  assert.match(io.out, /remote execution is not configured: set DOTCORE_REMOTE_CONFIG/);
  assert.match(io.out, /"gb10-host-1": "gb10-host-1\.example\.net"/);

  const json = collectingIo({ env: {}, transport: {} });
  assert.equal(await main(["list", "--json"], json), 1);
  assert.deepEqual(JSON.parse(json.out), { configured: false, hosts: [] });
});

test("list reports a usable host and the first failing check of an unusable one", needsJj, async (t) => {
  const f = await fixture(t, { extraHost: true });
  const io = f.io();

  assert.equal(await main(["list"], io), 0, io.err);
  assert.equal(io.out, `loop:one  usable\nloop:missing  unusable: ${f.remote}-missing is not a jj repository with jj on the remote PATH\n`);

  const json = f.io();
  await main(["list", "--json"], json);
  const hosts = JSON.parse(json.out).hosts;
  assert.deepEqual(hosts.map((h) => [h.host, h.usable]), [["one", true], ["missing", false]]);
  assert.deepEqual(hosts[0].checks, { repository: "ok", ignored: "ok", shell: "ok" });
});

test("list checks one host at a time, in configuration order", needsJj, async (t) => {
  const contacted = [];
  let active = 0;
  let maxActive = 0;
  const transport = {
    exec: async (host, command, handlers) => {
      contacted.push(host.name);
      active += 1;
      maxActive = Math.max(maxActive, active);
      await new Promise((resolve) => setTimeout(resolve, 20));
      active -= 1;
      handlers.onStderr("ssh authentication failed\n");
      return { code: 255 };
    },
  };
  const f = await fixture(t, { extraHost: true, transport });
  const io = f.io();

  assert.equal(await main(["list"], io), 1);
  assert.deepEqual(contacted, ["one", "missing"]);
  assert.equal(maxActive, 1);
});

test("list fails when no host is usable", needsJj, async (t) => {
  const f = await fixture(t, { onlyMissing: true });
  const io = f.io();
  assert.equal(await main(["list"], io), 1);
  assert.match(io.out, /loop:missing {2}unusable/);
});

test("sync delivers committed and uncommitted work and leaves local and remote state alone", needsJj, async (t) => {
  const f = await fixture(t);
  await writeFile(path.join(f.local, "hello.txt"), "edited\n");
  await writeFile(path.join(f.local, "new.txt"), "brand new\n");

  const localBefore = jj(f.local, "log", "-r", "all()", "--no-graph", "-T", 'commit_id ++ "\\n"');
  const remoteDefault = () => jj(f.remote, "log", "-r", "default@", "--no-graph", "-T", "commit_id");
  const defaultBefore = remoteDefault();

  const io = f.io();
  assert.equal(await main(["sync", "loop"], io), 0, io.err);

  const workspaceDir = await f.workspaceDir();
  assert.equal(await readFile(path.join(workspaceDir, "hello.txt"), "utf8"), "edited\n");
  assert.equal(await readFile(path.join(workspaceDir, "new.txt"), "utf8"), "brand new\n");
  assert.match(io.out, new RegExp(`^synced [0-9a-f]{12} to loop:one at ${workspaceDir}\\n$`));

  assert.equal(jj(f.local, "log", "-r", "all()", "--no-graph", "-T", 'commit_id ++ "\\n"'), localBefore);
  assert.equal(remoteDefault(), defaultBefore);
  assert.equal(jj(f.remote, "bookmark", "list"), "", "the pushed branch is removed");
  assert.equal(await f.lockExists(), false);
});

test("a second sync replaces the files and keeps remote edits in history", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();

  await writeFile(path.join(workspaceDir, "remote-edit.txt"), "made on the remote\n");
  await writeFile(path.join(f.local, "hello.txt"), "second\n");
  assert.equal(await main(["sync", "loop"], f.io()), 0);

  assert.equal(await readFile(path.join(workspaceDir, "hello.txt"), "utf8"), "second\n");
  await assert.rejects(stat(path.join(workspaceDir, "remote-edit.txt")));
  const kept = jj(f.remote, "log", "-r", 'files("remote-edit.txt")', "--no-graph", "-T", 'commit_id ++ "\\n"').split("\n").filter(Boolean);
  assert.equal(kept.length, 1);
  assert.equal(jj(f.remote, "file", "show", "-r", kept[0], "remote-edit.txt"), "made on the remote");
});

test("remote commands leave no divergent commit or orphaned head on the host across local rewrites", needsJj, async (t) => {
  const f = await fixture(t);
  await writeFile(path.join(f.local, "stack.txt"), "stack\n");
  jj(f.local, "commit", "-m", "stack");
  const rewrite = async (round) => {
    await writeFile(path.join(f.local, "hello.txt"), `round ${round}\n`);
    jj(f.local, "describe", "-r", "@-", "-m", `stack v${round}`);
  };

  assert.equal(await main(["sync", "loop"], f.io()), 0);
  await rewrite(1);
  assert.equal(await main(["exec", "loop", "true"], f.io()), 0);
  assert.equal(await main(["status", "loop"], f.io()), 0);
  await rewrite(2);
  assert.equal(await main(["run", "loop", "x"], f.io()), 0);
  assert.equal(await main(["logs", "loop"], f.io()), 0);
  await rewrite(3);
  assert.equal(await main(["exec", "loop", "true"], f.io()), 0);
  assert.equal(await main(["sync", "loop"], f.io()), 0);

  const workspace = path.basename(await f.workspaceDir());
  assert.deepEqual(hostCommits(f, "heads(all()) ~ ::(bookmarks() | working_copies())"), []);
  assert.deepEqual(hostCommits(f, "divergent()"), []);
  assert.deepEqual(hostCommits(f, "mutable() ~ ::working_copies()"), []);
  assert.deepEqual(hostCommits(f, `${workspace}@-`), [jj(f.local, "log", "-r", "@", "--no-graph", "-T", "commit_id")]);
  assert.equal(hostJj(f, "bookmark", "list"), "");
});

test("remote commands leave the host owner's working copy unsnapshotted after the first sync", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  await writeFile(path.join(f.remote, "owner-edit.txt"), "not snapshotted\n");
  const ownerCommit = () => hostCommits(f, "default@");
  const before = ownerCommit();

  await writeFile(path.join(f.local, "hello.txt"), "second\n");
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  assert.equal(await main(["exec", "loop", "true"], f.io()), 0);
  assert.equal(await main(["clean", "loop"], f.io()), 0);

  assert.deepEqual(ownerCommit(), before);
  assert.equal(hostJj(f, "diff", "-r", "default@", "--summary"), "");
});

test("remote edits move onto each new sync and onto the fork point at clean, leaving no old chain", needsJj, async (t) => {
  const f = await fixture(t);
  const before = hostCommits(f, "all()");
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();
  const workspace = path.basename(workspaceDir);
  await writeFile(path.join(workspaceDir, "remote-edit.txt"), "made on the remote\n");

  for (const round of [1, 2]) {
    await writeFile(path.join(f.local, "hello.txt"), `round ${round}\n`);
    jj(f.local, "describe", "-r", "@-", "-m", `local base v${round}`);
    assert.equal(await main(["sync", "loop"], f.io()), 0);
    assert.deepEqual(hostCommits(f, 'parents(files("remote-edit.txt"))'), hostCommits(f, `${workspace}@-`), `round ${round}`);
    assert.deepEqual(hostCommits(f, "divergent()"), [], `round ${round}`);
  }

  assert.equal(await main(["clean", "loop"], f.io()), 0);
  const edit = hostCommits(f, 'files("remote-edit.txt")');
  assert.equal(edit.length, 1);
  assert.deepEqual(hostCommits(f, `parents(${edit[0]})`), hostCommits(f, "root()"));
  assert.deepEqual(hostCommits(f, `all() ~ ${edit[0]}`).sort(), before.sort());
});

test("clean after a pull restores the host's visible commits", needsJj, async (t) => {
  const f = await fixture(t);
  const before = hostCommits(f, "all()");
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  await writeFile(path.join(f.local, "hello.txt"), "second\n");
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  assert.equal(await main(["pull", "loop"], f.io()), 0);
  assert.equal(await main(["clean", "loop"], f.io()), 0);

  assert.deepEqual(hostCommits(f, "all()").sort(), before.sort());
  assert.equal(spawnSync("git", ["-C", f.remote, "for-each-ref", "refs/remote-exec", "refs/heads/remote-exec"], { encoding: "utf8" }).stdout, "");
});

test("the first sync sweeps leftovers of older tool versions and spares the host owner and other workspaces", needsJj, async (t) => {
  const f = await fixture(t);
  const secondary = path.join(path.dirname(f.local), "secondary");
  jj(f.local, "workspace", "add", secondary);
  assert.equal(await main(["sync", "loop"], f.io({ cwd: secondary })), 0);
  const other = path.basename(await f.workspaceDir(secondary));
  const otherCommits = hostCommits(f, `${other}@ | ${other}@-`);

  const ownerBase = hostCommits(f, "default@-")[0];
  jj(f.remote, "new", "-m", "owner work");
  await writeFile(path.join(f.remote, "owner.txt"), "owner\n");
  jj(f.remote, "new", ownerBase);
  const ownerWork = hostCommits(f, 'description(exact:"owner work\\n")');

  const pushed = [];
  for (const round of [1, 2, 3]) {
    await writeFile(path.join(f.local, "hello.txt"), `legacy ${round}\n`);
    jj(f.local, "describe", "-r", "@-", "-m", `local base v${round}`);
    pushed.push(legacyPush(f, `remote-exec/legacy/run${round}`));
  }
  jj(f.remote, "bookmark", "create", "owner-pick", "-r", pushed[1]);
  assert.ok(hostCommits(f, "divergent()").length > 0, "the legacy pushes left divergent copies");

  await writeFile(path.join(f.local, "hello.txt"), "new tool\n");
  assert.equal(await main(["sync", "loop"], f.io()), 0);

  assert.deepEqual(hostCommits(f, "heads(all()) ~ ::(bookmarks() | working_copies())"), ownerWork);
  assert.deepEqual(hostCommits(f, "owner-pick"), [pushed[1]]);
  assert.deepEqual(hostCommits(f, `${other}@ | ${other}@-`), otherCommits);
  assert.deepEqual(hostCommits(f, `(present(${pushed[0]}) | present(${pushed[2]})) & ::visible_heads()`), []);
  assert.deepEqual(hostCommits(f, "divergent() ~ ::(owner-pick | working_copies())"), []);
});

test("the sweep finds pushed commits on every branch of a forked operation log", needsJj, async (t) => {
  const f = await fixture(t);
  const first = legacyPush(f, "remote-exec/legacy/a", { remove: false });
  await writeFile(path.join(f.local, "hello.txt"), "second\n");
  const second = legacyPush(f, "remote-exec/legacy/b", { remove: false });
  const fork = jj(f.remote, "op", "log", "-n1", "--no-graph", "-T", "id");
  jj(f.remote, "--at-op", fork, "bookmark", "delete", "remote-exec/legacy/a");
  jj(f.remote, "--at-op", fork, "bookmark", "delete", "remote-exec/legacy/b");

  await writeFile(path.join(f.local, "hello.txt"), "new tool\n");
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  assert.deepEqual(hostCommits(f, `(present(${first}) | present(${second})) & ::visible_heads()`), []);
});

test("jj still describes a bookmark deletion the way the sweep looks for it", needsJj, async (t) => {
  const f = await fixture(t);
  legacyPush(f, "remote-exec/legacy/run1");
  assert.equal(jj(f.remote, "op", "log", "-n1", "--no-graph", "-T", "description"), "delete bookmark remote-exec/legacy/run1");
});

test("a sync that fails after its push leaves nothing behind once the next sync sweeps", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  await f.writeLock({ pid: process.pid, host: os.hostname(), runId: "alive0000000", startedAt: "now" });
  await writeFile(path.join(f.local, "hello.txt"), "refused\n");
  const refusedSha = jj(f.local, "log", "-r", "@", "--no-graph", "-T", "commit_id");
  const refused = f.io();
  assert.equal(await main(["sync", "loop"], refused), TOOL_FAILURE_EXIT);
  assert.match(refused.err, /locked by run alive0000000/);
  assert.equal(hostJj(f, "bookmark", "list"), "");

  await rm(await f.lockDir(), { recursive: true });
  await writeFile(path.join(f.local, "hello.txt"), "accepted\n");
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  assert.deepEqual(hostCommits(f, `present(${refusedSha}) & ::visible_heads()`), []);
  assert.deepEqual(hostCommits(f, "heads(all()) ~ ::(bookmarks() | working_copies())"), []);
});

test("a sync waits for the host lock and names it when it stays held", needsJj, async (t) => {
  const f = await fixture(t, { env: { REMOTE_EXEC_HOST_LOCK_WAIT_MS: "300" } });
  const hostLock = path.join(f.remote, ".workspaces", ".remote-locks", ".host");
  await mkdir(hostLock, { recursive: true });
  await writeFile(path.join(hostLock, "holder.json"), JSON.stringify({ pid: process.pid, host: os.hostname(), runId: "host-alive", startedAt: "now" }));

  const held = f.io();
  assert.equal(await main(["sync", "loop"], held), TOOL_FAILURE_EXIT);
  assert.match(held.err, /host repository stayed locked by another remote-exec operation/);
  assert.equal(hostJj(f, "bookmark", "list"), "");

  await rm(hostLock, { recursive: true });
  assert.equal(await main(["sync", "loop"], f.io()), 0);
});

test("a sync moves only the workspace's own edits, never other commits built on the synced commit", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspace = path.basename(await f.workspaceDir());
  const synced = hostCommits(f, `${workspace}@-`)[0];
  const ownerBase = hostCommits(f, "default@-")[0];
  jj(f.remote, "new", synced, "-m", "owner on synced");
  await writeFile(path.join(f.remote, "owner.txt"), "owner\n");
  jj(f.remote, "new", ownerBase);
  const owner = hostCommits(f, 'description(exact:"owner on synced\\n")');

  await writeFile(path.join(f.local, "hello.txt"), "next\n");
  const io = f.io();
  assert.equal(await main(["sync", "loop"], io), 0, io.err);
  assert.deepEqual(hostCommits(f, 'description(exact:"owner on synced\\n")'), owner);
  assert.deepEqual(hostCommits(f, `parents(${owner[0]})`), [synced]);
});

test("an immutable commit built on the synced commit blocks neither sync nor clean", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();
  const synced = hostCommits(f, `${path.basename(workspaceDir)}@-`)[0];
  const ownerBase = hostCommits(f, "default@-")[0];
  jj(f.remote, "new", synced, "-m", "tagged");
  await writeFile(path.join(f.remote, "tagged.txt"), "tagged\n");
  const tagged = jj(f.remote, "log", "-r", "@", "--no-graph", "-T", "commit_id");
  jj(f.remote, "new", ownerBase);
  spawnSync("git", ["-C", f.remote, "tag", "v1", tagged]);
  await writeFile(path.join(workspaceDir, "remote-edit.txt"), "made on the remote\n");

  for (const round of [1, 2]) {
    await writeFile(path.join(f.local, "hello.txt"), `next ${round}\n`);
    const io = f.io();
    assert.equal(await main(["sync", "loop"], io), 0, io.err);
  }
  const io = f.io();
  assert.equal(await main(["clean", "loop"], io), 0, io.err);
  assert.equal(await exists(workspaceDir), false);
  assert.deepEqual(hostCommits(f, "v1"), [tagged]);
  assert.equal(hostCommits(f, 'files("remote-edit.txt")').length, 1);
});

test("clean keeps edits made after an older tool version's pull branch", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();
  const workspace = path.basename(workspaceDir);
  await writeFile(path.join(workspaceDir, "pulled.txt"), "pulled\n");
  jj(workspaceDir, "bookmark", "set", `remote-exec/pull/${workspace}`, "-r", "@");
  await writeFile(path.join(workspaceDir, "later.txt"), "unsaved later edit\n");

  const io = f.io();
  assert.equal(await main(["clean", "loop"], io), 0, io.err);
  const kept = hostCommits(f, 'files("later.txt")');
  assert.equal(kept.length, 1);
  assert.equal(hostJj(f, "file", "show", "-r", kept[0], "later.txt"), "unsaved later edit");
});

test("a sync sweeps the older leftovers of a local workspace stacked on this one instead of moving them", needsJj, async (t) => {
  const f = await fixture(t);
  const stacked = path.join(path.dirname(f.local), "stacked");
  jj(f.local, "workspace", "add", "-r", "@", stacked);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const gitDir = jj(f.local, "git", "root");
  for (const round of [1, 2]) {
    await writeFile(path.join(stacked, "stacked.txt"), `stacked ${round}\n`);
    const sha = jj(stacked, "log", "-r", "@", "--no-graph", "-T", "commit_id");
    spawnSync("git", ["--git-dir", gitDir, "push", "--force", `${f.remote}/.git`, `${sha}:refs/heads/remote-exec/legacy/s${round}`]);
    jj(f.remote, "git", "import");
    jj(f.remote, "bookmark", "delete", `remote-exec/legacy/s${round}`);
  }

  await writeFile(path.join(f.local, "hello.txt"), "next\n");
  const io = f.io();
  assert.equal(await main(["sync", "loop"], io), 0, io.err);
  assert.deepEqual(hostCommits(f, "divergent()"), []);
  assert.deepEqual(hostCommits(f, "heads(all()) ~ ::(bookmarks() | working_copies())"), []);
  assert.deepEqual(hostCommits(f, 'files("stacked.txt")'), []);
});

test("a pushed commit abandoned by hand does not keep its parent visible", needsJj, async (t) => {
  const f = await fixture(t);
  await writeFile(path.join(f.local, "stack.txt"), "stack\n");
  jj(f.local, "commit", "-m", "stack");
  const stack = jj(f.local, "log", "-r", "@-", "--no-graph", "-T", "commit_id");
  const first = legacyPush(f, "remote-exec/legacy/run1");
  jj(f.remote, "abandon", first);
  await writeFile(path.join(f.local, "hello.txt"), "second\n");
  legacyPush(f, "remote-exec/legacy/run2");

  jj(f.local, "describe", "-r", "@-", "-m", "stack v2");
  const io = f.io();
  assert.equal(await main(["sync", "loop"], io), 0, io.err);
  assert.deepEqual(hostCommits(f, `present(${stack}) & ::visible_heads()`), []);
  assert.deepEqual(hostCommits(f, "divergent()"), []);
});

test("syncing the same commit again abandons nothing", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspace = path.basename(await f.workspaceDir());
  const kept = () => hostCommits(f, `all() ~ ${workspace}@`).sort();
  const before = kept();

  const io = f.io();
  assert.equal(await main(["sync", "loop"], io), 0, io.err);
  assert.deepEqual(kept(), before);
});

test("sync works from a secondary local workspace that has no git directory of its own", needsJj, async (t) => {
  const f = await fixture(t);
  const secondary = path.join(path.dirname(f.local), "secondary");
  jj(f.local, "workspace", "add", secondary);
  await writeFile(path.join(secondary, "hello.txt"), "from secondary\n");

  const io = f.io({ cwd: secondary });
  assert.equal(await main(["sync", "loop"], io), 0, io.err);
  const workspaceDir = await f.workspaceDir(secondary);
  assert.equal(await readFile(path.join(workspaceDir, "hello.txt"), "utf8"), "from secondary\n");
  assert.notEqual(workspaceDir, await f.workspaceDir());
});

test("a conflicted working copy is refused", needsJj, async (t) => {
  const f = await fixture(t);
  const base = jj(f.local, "log", "-r", "@-", "--no-graph", "-T", "change_id");
  const side = async (name) => {
    jj(f.local, "new", base, "-m", name);
    await writeFile(path.join(f.local, "clash.txt"), `${name}\n`);
    return jj(f.local, "log", "-r", "@", "--no-graph", "-T", "change_id");
  };
  const a = await side("a");
  const b = await side("b");
  jj(f.local, "new", a, b, "-m", "merge");

  const io = f.io();
  assert.equal(await main(["sync", "loop"], io), TOOL_FAILURE_EXIT);
  assert.match(io.err, /working copy commit has conflicts/);
});

test("run streams both outputs and returns the command's own status", needsJj, async (t) => {
  const f = await fixture(t);
  const io = f.io();

  assert.equal(await main(["run", "loop", "fail", "x"], io), 7, io.err);
  assert.equal(io.out, "out-text\n");
  assert.match(io.err, /err-text\n/);

  const ok = f.io();
  assert.equal(await main(["run", "loop", "plain"], ok), 0, ok.err);
  assert.equal(ok.out, `ran in ${await f.workspaceDir()}\n`);
});

test("run hands arguments to just unchanged", needsJj, async (t) => {
  const f = await fixture(t);
  const args = ["--flag", "a b", "it's", 'dq"x', "$HOME `id`", "multi\nline", "ünï", "--help"];

  const io = f.io();
  assert.equal(await main(["run", "loop", ...args], io), 0, io.err);

  const received = (await readFile(f.argsOut, "utf8")).split("\0");
  received.pop();
  assert.deepEqual(received, args);
});

test("a second run is refused while one holds the lock, and works after it ends", needsJj, async (t) => {
  const f = await fixture(t);
  const first = main(["run", "loop", "block"], f.io());
  await waitFor(() => exists(f.started), "the blocking command to start");

  const refused = f.io();
  assert.equal(await main(["run", "loop", "plain"], refused), TOOL_FAILURE_EXIT);
  assert.match(refused.err, /remote workspace is locked by run [0-9a-f]{12} \(pid \d+ on .+, since .+\); wait for it or rerun with --break-lock/);

  await writeFile(f.release, "");
  assert.equal(await first, 0);
  assert.equal(await f.lockExists(), false);

  const after = f.io();
  assert.equal(await main(["run", "loop", "plain"], after), 0, after.err);
});

test("losing the local side stops the command and frees the lock", needsJj, async (t) => {
  const f = await fixture(t);
  const controller = new AbortController();
  const io = f.io({ signal: controller.signal });
  const run = main(["run", "loop", "block"], io);
  await waitFor(() => exists(f.started), "the blocking command to start");
  const pid = Number((await readFile(f.pidOut, "utf8")).trim());

  controller.abort();
  await run;

  await waitFor(async () => !processAlive(pid), "the command to be stopped");
  await waitFor(async () => !(await f.lockExists()), "the lock to be released");
});

test("a lock left by a dead process is taken over, and a live one needs the explicit flag", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);

  const dead = spawnSync("true");
  assert.equal(dead.status, 0);
  const deadPid = (() => { const child = spawnSync("sh", ["-c", "echo $$"], { encoding: "utf8" }); return Number(child.stdout); })();
  await f.writeLock({ pid: deadPid, host: os.hostname(), runId: "deadbeef0000", startedAt: "then" });
  const takeover = f.io();
  assert.equal(await main(["run", "loop", "plain"], takeover), 0, takeover.err);

  await f.writeLock({ pid: process.pid, host: os.hostname(), runId: "alive0000000", startedAt: "now" });
  const refused = f.io();
  assert.equal(await main(["run", "loop", "plain"], refused), TOOL_FAILURE_EXIT);
  assert.match(refused.err, /locked by run alive0000000/);

  const broken = f.io();
  assert.equal(await main(["run", "loop", "--break-lock", "plain"], broken), 0, broken.err);
  assert.equal(await f.lockExists(), false);
});

test("an ssh level failure is reported as a tool failure, not as the command's status", needsJj, async (t) => {
  const transport = {
    push: async () => ({ code: 0, stderr: "" }),
    exec: async (host, command, handlers) => {
      handlers.onStderr("Host key verification failed.\n");
      return { code: 255 };
    },
  };
  const f = await fixture(t, { transport });
  const io = f.io();
  assert.equal(await main(["run", "loop", "x"], io), TOOL_FAILURE_EXIT);
  assert.match(io.err, /ssh does not trust the host key yet.*\(exit 255, not from the command\)/);
});

function collectingIo({ env, transport, cwd = process.cwd() }) {
  const io = {
    env, cwd, machine, transport, out: "", err: "",
    stdout: (text) => (io.out += text),
    stderr: (text) => (io.err += text),
  };
  return io;
}

async function fixture(t, { extraHost = false, onlyMissing = false, transport = null, env: extraEnv = {} } = {}) {
  const root = await realpath(await mkdtemp(path.join(os.tmpdir(), "dotcore-remote-exec-")));
  t.after(() => rm(root, { force: true, recursive: true }));
  const bin = path.join(root, "bin");
  const local = path.join(root, "local");
  const remote = path.join(root, "remote");
  const paths = {
    started: path.join(root, "started"),
    release: path.join(root, "release"),
    pidOut: path.join(root, "pid"),
    argsOut: path.join(root, "args"),
  };
  await mkdir(bin);

  const config = { groups: { loop: { hosts: {} } } };
  const dir = onlyMissing ? `${remote}-missing` : remote;
  config.groups.loop.dir = dir;
  config.groups.loop.hosts[onlyMissing ? "missing" : "one"] = "localhost";
  if (extraHost) {
    config.groups.loop.hosts.missing = { address: "localhost", dir: `${remote}-missing` };
  }
  const configFile = path.join(root, "remote.json");
  await writeFile(configFile, JSON.stringify(config));
  await writeScript(path.join(bin, "direnv"), `case "$1" in
  allow) exit 0 ;;
  exec) shift; cd "$1" && shift && exec "$@" ;;
esac
exit 2`);
  await writeScript(path.join(bin, "just"), `printf '%s\\0' "$@" > "$JUST_ARGS_OUT"
case "\${1:-}" in
  fail) echo out-text; echo err-text >&2; exit 7 ;;
  block) echo block-started; echo $$ > "$JUST_PID_OUT"; : > "$JUST_STARTED"; while [ ! -e "$JUST_RELEASE" ]; do sleep 0.02; done ;;
  *) echo "ran in $(pwd)" ;;
esac`);

  // The remote repository ignores .workspaces, like the real hosts do through their global gitignore.
  jj(root, "git", "init", "--colocate", remote);
  await writeFile(path.join(remote, ".gitignore"), ".workspaces/\n");
  jj(remote, "commit", "-m", "remote base");

  jj(root, "git", "init", "--colocate", local);
  await mkdir(path.join(local, "dev"));
  await mkdir(path.join(local, "dev", "remote"));
  await copyFile(toolSource, path.join(local, "dev", "remote", "remote-exec.mjs"));
  await writeFile(path.join(local, "hello.txt"), "base\n");
  jj(local, "commit", "-m", "local base");

  const env = {
    ...process.env,
    PATH: `${bin}:${process.env.PATH}`,
    JJ_USER: "Test",
    JJ_EMAIL: "test@example.com",
    JUST_STARTED: paths.started,
    JUST_RELEASE: paths.release,
    JUST_PID_OUT: paths.pidOut,
    JUST_ARGS_OUT: paths.argsOut,
    REMOTE_EXEC_POLL_MS: "20",
    ...extraEnv,
  };
  Object.assign(process.env, { JJ_USER: env.JJ_USER, JJ_EMAIL: env.JJ_EMAIL });

  const loopback = (signal) => ({
    async push(host, { gitDir, sha, refs }) {
      const result = spawnSync("git", ["--git-dir", gitDir, "push", "--force", `${host.dir}/.git`, ...refs.map((ref) => `${sha}:${ref}`)], { encoding: "utf8", env });
      return { code: result.status, stderr: result.stderr };
    },
    async fetch(host, { gitDir, remoteRef, localRef }) {
      const result = spawnSync("git", ["--git-dir", gitDir, "fetch", "--no-tags", `${host.dir}/.git`, `+${remoteRef}:${localRef}`], { encoding: "utf8", env });
      return { code: result.status, stderr: result.stderr };
    },
    exec: (host, command, handlers) => spawnStreaming("sh", ["-c", command], { ...handlers, signal: signal ?? handlers.signal }, env),
  });

  const lockDirFor = async (name) => path.join(remote, ".workspaces", ".remote-locks", name);
  const nameFor = (cwd = local) => {
    const name = cwd === local ? "default" : path.basename(cwd);
    return remoteWorkspaceName(name, jj(cwd, "workspace", "root"), machine);
  };
  return {
    ...paths,
    local,
    remote,
    io: ({ cwd = local, signal } = {}) => collectingIo({
      env: { DOTCORE_REMOTE_CONFIG: configFile },
      transport: transport ?? loopback(signal),
      cwd,
    }),
    env,
    runIds: async () => {
      const name = nameFor();
      const dirEntries = spawnSync("ls", [path.join(remote, ".workspaces", ".remote-runs", name)], { encoding: "utf8" });
      return dirEntries.stdout.split("\n").filter(Boolean);
    },
    workspaceDir: async (cwd = local) => path.join(dir, ".workspaces", nameFor(cwd)),
    lockDir: async () => lockDirFor(nameFor()),
    lockExists: async () => exists(await lockDirFor(nameFor())),
    readLock: async () => JSON.parse(await readFile(path.join(await lockDirFor(nameFor()), "holder.json"), "utf8")),
    writeLock: async (holder) => {
      const lockDir = await lockDirFor(nameFor());
      await mkdir(lockDir, { recursive: true });
      await writeFile(path.join(lockDir, "holder.json"), JSON.stringify(holder));
    },
  };
}

async function writeScript(file, body) {
  await writeFile(file, `#!/bin/sh\n${body}\n`);
  await chmod(file, 0o755);
}

function jj(cwd, ...args) {
  const result = spawnSync("jj", args, { cwd, encoding: "utf8", env: process.env });
  assert.equal(result.status, 0, `jj ${args.join(" ")}: ${result.stderr}`);
  return result.stdout.trim();
}

/** Runs jj in the host checkout without snapshotting its working copy, after importing pushed refs. */
function hostJj(f, ...args) {
  jj(f.remote, "--ignore-working-copy", "git", "import");
  return jj(f.remote, "--ignore-working-copy", ...args);
}

/** The host's visible commit ids a revset selects. */
function hostCommits(f, revset) {
  return hostJj(f, "log", "-r", revset, "--no-graph", "-T", 'commit_id ++ "\\n"').split("\n").filter(Boolean);
}

/** Pushes the local working-copy commit the way older tool versions did: a branch jj imports, then deletes. */
function legacyPush(f, branch, { remove = true } = {}) {
  const sha = jj(f.local, "log", "-r", "@", "--no-graph", "-T", "commit_id");
  const pushed = spawnSync("git", ["--git-dir", jj(f.local, "git", "root"), "push", "--force", `${f.remote}/.git`, `${sha}:refs/heads/${branch}`], { encoding: "utf8", env: f.env });
  assert.equal(pushed.status, 0, pushed.stderr);
  jj(f.remote, "git", "import");
  if (remove) {
    jj(f.remote, "bookmark", "delete", branch);
  }
  return sha;
}

async function exists(file) {
  return stat(file).then(() => true, () => false);
}

function processAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/** Polls an observable condition until a generous deadline, reporting what was awaited on timeout. */
async function waitFor(condition, what, deadlineMs = 30000) {
  const end = Date.now() + deadlineMs;
  while (Date.now() < end) {
    if (await condition()) {
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 20));
  }
  assert.fail(`timed out waiting for ${what}`);
}

test("status reports the workspace, synced commit, remote changes, lock and recorded runs", needsJj, async (t) => {
  const f = await fixture(t);
  jj(f.local, "describe", "-m", "wip edit");
  const missing = f.io();
  assert.equal(await main(["status", "loop"], missing), 0, missing.err);
  assert.equal(missing.out, `workspace: ${await f.workspaceDir()} (missing; run sync first)\nlock: free\nruns: none\n`);

  assert.equal(await main(["run", "loop", "fail", "x"], f.io()), 7);
  await writeFile(path.join(await f.workspaceDir(), "remote-edit.txt"), "made on the remote\n");

  const io = f.io();
  assert.equal(await main(["status", "loop"], io), 0, io.err);
  const lines = io.out.split("\n");
  assert.equal(lines[0], `workspace: ${await f.workspaceDir()} (present)`);
  assert.match(lines[1], /^synced commit: [0-9a-f]{12} wip edit$/);
  assert.deepEqual(lines.slice(2, 4), ["changes: ", "  A remote-edit.txt"]);
  assert.equal(lines[4], "lock: free");
  assert.equal(lines[5], "runs:");
  assert.match(lines[6], /^ {2}[0-9a-f]{12} {2}exit 7 {2}\d{4}-\d\d-\d\dT[\d:.]+Z {2}just fail x$/);
});

test("exec runs a shell command in the remote workspace and returns its status", needsJj, async (t) => {
  const f = await fixture(t);
  const early = f.io();
  assert.equal(await main(["exec", "loop", "pwd"], early), TOOL_FAILURE_EXIT);
  assert.match(early.err, /does not exist; run sync first/);

  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const io = f.io();
  assert.equal(await main(["exec", "loop", "echo", "a   b;", "pwd"], io), 0, io.err);
  // Words are joined with spaces and parsed by the remote shell, like ssh does.
  assert.equal(io.out, `a b\n${await f.workspaceDir()}\n`);

  const failing = f.io();
  assert.equal(await main(["exec", "loop", "exit 3"], failing), 3);
});

test("logs, tail and wait read a finished run's recorded output and status", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["run", "loop", "fail", "x"], f.io()), 7);
  const [id] = await f.runIds();
  const sorted = (text) => text.split("\n").filter(Boolean).sort();

  const logs = f.io();
  assert.equal(await main(["logs", "loop"], logs), 0, logs.err);
  assert.deepEqual(sorted(logs.out), ["err-text", "out-text"]);

  const byId = f.io();
  assert.equal(await main(["logs", "loop", id], byId), 0);
  assert.equal(byId.out, logs.out);

  const last = f.io();
  assert.equal(await main(["logs", "loop", "--tail", "1"], last), 0);
  assert.equal(last.out, `${logs.out.split("\n")[1]}\n`);

  const tail = f.io();
  assert.equal(await main(["tail", "loop"], tail), 7);
  assert.equal(tail.out, logs.out);
  assert.equal(await main(["wait", "loop", id], f.io()), 7);

  const unknown = f.io();
  assert.equal(await main(["logs", "loop", "nosuchrun"], unknown), TOOL_FAILURE_EXIT);
  assert.match(unknown.err, /no run nosuchrun is recorded/);
});

test("a detached run keeps its lock until it ends and can be waited on", needsJj, async (t) => {
  const f = await fixture(t);
  const started = f.io();
  assert.equal(await main(["run", "loop", "--detach", "block"], started), 0, started.err);
  const [, id] = /^started run ([0-9a-f]{12})\n$/.exec(started.out);
  await waitFor(() => exists(f.started), "the detached command to start");

  const refused = f.io();
  assert.equal(await main(["run", "loop", "plain"], refused), TOOL_FAILURE_EXIT);
  assert.match(refused.err, new RegExp(`locked by run ${id}`));

  const status = f.io();
  await main(["status", "loop"], status);
  assert.match(status.out, new RegExp(` ${id} {2}running `));

  const waiting = main(["wait", "loop"], f.io());
  const following = f.io();
  const tail = main(["tail", "loop", id], following);
  await waitFor(() => following.out.includes("block-started"), "the follower to see output");

  await writeFile(f.release, "");
  assert.equal(await waiting, 0);
  assert.equal(await tail, 0);
  assert.equal(following.out, "block-started\n");
  await waitFor(async () => !(await f.lockExists()), "the lock to be released");
  assert.equal(await main(["run", "loop", "plain"], f.io()), 0);
});

test("kill stops a detached run, records its status and frees the lock", needsJj, async (t) => {
  const f = await fixture(t);
  const started = f.io();
  assert.equal(await main(["run", "loop", "--detach", "block"], started), 0, started.err);
  await waitFor(() => exists(f.started), "the detached command to start");
  const pid = Number((await readFile(f.pidOut, "utf8")).trim());

  const io = f.io();
  assert.equal(await main(["kill", "loop"], io), 0, io.err);
  assert.match(io.out, /^stopped run [0-9a-f]{12}\n$/);
  assert.equal(processAlive(pid), false);
  assert.equal(await f.lockExists(), false);

  const status = f.io();
  await main(["status", "loop"], status);
  assert.match(status.out, /exit 143/);

  const again = f.io();
  assert.equal(await main(["kill", "loop"], again), TOOL_FAILURE_EXIT);
  assert.match(again.err, /is not running/);
});

test("only the newest run records are kept", needsJj, async (t) => {
  const f = await fixture(t, { env: { REMOTE_EXEC_MAX_RUNS: "2" } });
  const ids = [];
  for (let run = 0; run < 3; run += 1) {
    assert.equal(await main(["run", "loop", "plain"], f.io()), 0);
    ids.push((await f.runIds()).filter((id) => !ids.includes(id))[0]);
  }
  assert.deepEqual((await f.runIds()).sort(), ids.slice(1).sort());
});

test("pull brings the remote workspace commit back as a private ref without touching the local graph", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  await writeFile(path.join(await f.workspaceDir(), "remote-edit.txt"), "made on the remote\n");
  const graph = () => jj(f.local, "log", "-r", "all()", "--no-graph", "-T", 'commit_id ++ "\\n"');
  const before = graph();
  const statusBefore = jj(f.local, "status");

  const io = f.io();
  assert.equal(await main(["pull", "loop"], io), 0, io.err);
  const [, short, ref] = /^pulled ([0-9a-f]{12}) into (refs\/remote-exec\/loop\/one\/remote-default-[0-9a-f]{8})\n/.exec(io.out);
  assert.match(io.out, /\+\+\+ b\/remote-edit\.txt\n@@ -0,0 \+1 @@\n\+made on the remote\n$/);

  const gitDir = jj(f.local, "git", "root");
  const pulled = spawnSync("git", ["--git-dir", gitDir, "rev-parse", ref], { encoding: "utf8" }).stdout.trim();
  assert.equal(pulled.slice(0, 12), short);
  assert.equal(graph(), before);
  assert.equal(jj(f.local, "status"), statusBefore);
  assert.equal(jj(f.local, "bookmark", "list"), "");
  assert.equal(hostJj(f, "bookmark", "list"), "", "the host keeps no branch for the pulled commit");
});

test("gc lists leftovers by default and removes only idle remote workspaces and unlocked branches", needsJj, async (t) => {
  const f = await fixture(t, { env: { REMOTE_EXEC_BRANCH_GRACE_MS: "0" } });
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const idleDir = await f.workspaceDir();
  await writeFile(path.join(idleDir, "unsaved.txt"), "kept in history\n");

  const secondary = path.join(path.dirname(f.local), "secondary");
  jj(f.local, "workspace", "add", secondary);
  assert.equal(await main(["sync", "loop"], f.io({ cwd: secondary })), 0);
  const lockedName = path.basename(await f.workspaceDir(secondary));
  await mkdir(path.join(f.remote, ".workspaces", ".remote-locks", lockedName), { recursive: true });
  await writeFile(
    path.join(f.remote, ".workspaces", ".remote-locks", lockedName, "holder.json"),
    JSON.stringify({ pid: process.pid, host: os.hostname(), runId: "alive0000000", startedAt: "now" }),
  );

  jj(f.remote, "workspace", "add", "--name", "remote-handmade", path.join(f.remote, ".workspaces", "remote-handmade"));
  const staleSha = jj(f.local, "log", "-r", "@", "--no-graph", "-T", "commit_id");
  spawnSync("git", ["--git-dir", jj(f.local, "git", "root"), "push", "--force", `${f.remote}/.git`, `${staleSha}:refs/heads/remote-exec/stale/old`], { env: f.env });

  const idleName = path.basename(idleDir);
  const listing = [`bookmark remote-exec/stale/old`, `workspace ${idleName} (idle 0 days)`];
  const dry = f.io();
  assert.equal(await main(["gc", "loop", "--older-than", "0"], dry), 0, dry.err);
  assert.equal(dry.out, `${listing.map((line) => `would remove ${line}\n`).join("")}2 item(s) found; rerun with --yes to remove them\n`);
  assert.ok(await exists(idleDir));

  const applied = f.io();
  assert.equal(await main(["gc", "loop", "--older-than", "0", "--yes"], applied), 0, applied.err);
  assert.equal(applied.out, listing.map((line) => `removed ${line}\n`).join(""));

  assert.equal(await exists(idleDir), false);
  assert.deepEqual(jj(f.remote, "workspace", "list", "-T", 'name ++ "\\n"').split("\n").sort(), ["default", lockedName, "remote-handmade"].sort());
  assert.equal(jj(f.remote, "bookmark", "list"), "");
  // The removed workspace's unsaved edit stays in the repository's history.
  assert.equal(jj(f.remote, "log", "-r", 'files("unsaved.txt")', "--no-graph", "-T", 'description.first_line() ++ "x"'), "x");

  const again = f.io();
  assert.equal(await main(["gc", "loop", "--older-than", "0", "--yes"], again), 0);
  assert.equal(again.out, "nothing to remove\n");
});

test("gc keeps a workspace that was active within the threshold", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const io = f.io();
  assert.equal(await main(["gc", "loop"], io), 0, io.err);
  assert.equal(io.out, "nothing to remove\n");
});

test("gc keeps an idle workspace that holds a joined worker node", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();
  await mkdir(path.join(workspaceDir, ".dotcore", "two-node-worker"), { recursive: true });
  await writeFile(path.join(workspaceDir, ".dotcore", "two-node-worker", "worker.yaml"), "joined node\n");

  const io = f.io();
  assert.equal(await main(["gc", "loop", "--older-than", "0", "--yes"], io), 0, io.err);
  assert.equal(io.out, `skipped workspace ${path.basename(workspaceDir)}: it holds a joined worker node; clean it with just dev::cluster::clean first\nnothing to remove\n`);
  assert.ok(await exists(workspaceDir));
});

test("clean removes this workspace with its runs and branches, keeps its unsaved edits in history, and spares others", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["run", "loop", "fail"], f.io()), 7);
  const workspaceDir = await f.workspaceDir();
  const name = path.basename(workspaceDir);
  await writeFile(path.join(workspaceDir, "unsaved.txt"), "kept in history\n");
  const secondary = path.join(path.dirname(f.local), "secondary");
  jj(f.local, "workspace", "add", secondary);
  assert.equal(await main(["sync", "loop"], f.io({ cwd: secondary })), 0);
  const otherName = path.basename(await f.workspaceDir(secondary));

  const io = f.io();
  assert.equal(await main(["clean", "loop"], io), 0, io.err);
  assert.equal(io.out, `removed workspace ${name}\n`);
  assert.equal(await exists(workspaceDir), false);
  assert.equal(await exists(path.join(f.remote, ".workspaces", ".remote-runs", name)), false);
  assert.deepEqual(jj(f.remote, "workspace", "list", "-T", 'name ++ "\\n"').split("\n").sort(), ["default", otherName].sort());
  assert.equal(jj(f.remote, "bookmark", "list", `glob:remote-exec/${name}/*`), "");
  assert.equal(jj(f.remote, "log", "-r", 'files("unsaved.txt")', "--no-graph", "-T", 'description.first_line() ++ "x"'), "x");
  assert.ok(await exists(await f.workspaceDir(secondary)));
});

test("clean refuses a workspace whose lock is held or that holds a joined worker, and removes nothing", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();

  await f.writeLock({ pid: process.pid, host: os.hostname(), runId: "alive0000000", startedAt: "now" });
  const locked = f.io();
  assert.equal(await main(["clean", "loop"], locked), TOOL_FAILURE_EXIT);
  assert.ok(await exists(workspaceDir));
  await rm(await f.lockDir(), { recursive: true });

  await mkdir(path.join(workspaceDir, ".dotcore", "two-node-worker"), { recursive: true });
  await writeFile(path.join(workspaceDir, ".dotcore", "two-node-worker", "worker.yaml"), "joined node\n");
  const joined = f.io();
  assert.equal(await main(["clean", "loop"], joined), TOOL_FAILURE_EXIT);
  assert.match(joined.err, /holds a joined worker node; remove it with just dev::cluster::clean first/);
  assert.ok(await exists(path.join(workspaceDir, "hello.txt")));
});

test("resolve names the configured host, the first one for a bare group", needsJj, async (t) => {
  const f = await fixture(t, { extraHost: true });
  for (const [spec, expected] of [["loop", "loop:one\n"], ["loop:missing", "loop:missing\n"]]) {
    const io = f.io();
    assert.equal(await main(["resolve", spec], io), 0, io.err);
    assert.equal(io.out, expected);
  }
  const unknown = f.io();
  assert.equal(await main(["resolve", "nope"], unknown), TOOL_FAILURE_EXIT);
  assert.match(unknown.err, /unknown host group/);
});

test("hosts prints every configured host in order without contacting any", async (t) => {
  const root = await mkdtemp(path.join(os.tmpdir(), "dotcore-remote-hosts-"));
  t.after(() => rm(root, { force: true, recursive: true }));
  const configFile = path.join(root, "remote.json");
  await writeFile(configFile, JSON.stringify(CONFIG));
  const transport = new Proxy({}, { get: () => () => assert.fail("hosts must not use the transport") });

  const io = collectingIo({ env: { DOTCORE_REMOTE_CONFIG: configFile }, transport });
  assert.equal(await main(["hosts"], io), 0, io.err);
  assert.equal(io.out, "gb10:one\ngb10:two\napple:mac\n");

  const unconfigured = collectingIo({ env: {}, transport });
  assert.equal(await main(["hosts"], unconfigured), TOOL_FAILURE_EXIT);
  assert.match(unconfigured.err, /remote execution is not configured: set DOTCORE_REMOTE_CONFIG/);
  assert.equal(unconfigured.out, "");
});

test("the host comes first and only the tool's own options are read before the just arguments", needsJj, async (t) => {
  const f = await fixture(t);
  const received = async () => {
    const parts = (await readFile(f.argsOut, "utf8")).split("\0");
    parts.pop();
    return parts;
  };

  assert.equal(await main(["run", "loop", "--", "--detach", "x"], f.io()), 0);
  assert.deepEqual(await received(), ["--detach", "x"]);
  assert.equal(await main(["run", "loop", "--unknown", "x"], f.io()), 0);
  assert.deepEqual(await received(), ["--unknown", "x"]);

  for (const bad of [["status", "loop", "extra"], ["logs", "loop", "--bogus"], ["gc", "loop", "--older-than", "x"], ["run", "loop"]]) {
    const io = f.io();
    assert.equal(await main(bad, io), TOOL_FAILURE_EXIT, bad.join(" "));
    assert.match(io.err, /usage:|unknown option|whole number/);
  }
});

test("gc without a host cleans the checkout it runs in without configuration, and spares the workspace it runs from", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();
  const name = path.basename(workspaceDir);
  const localIo = (cwd) => collectingIo({ env: {}, transport: {}, cwd });

  const inside = localIo(workspaceDir);
  assert.equal(await main(["gc", "--older-than", "0", "--yes"], inside), 0, inside.err);
  assert.equal(inside.out, `cleaning the local checkout ${f.remote}\nnothing to remove\n`);
  assert.ok(await exists(workspaceDir));

  const dry = localIo(f.remote);
  assert.equal(await main(["gc", "--older-than", "0"], dry), 0, dry.err);
  assert.equal(dry.out, `cleaning the local checkout ${f.remote}\nwould remove workspace ${name} (idle 0 days)\n1 item(s) found; rerun with --yes to remove them\n`);

  const applied = localIo(f.remote);
  assert.equal(await main(["gc", "--older-than", "0", "--yes"], applied), 0, applied.err);
  assert.equal(applied.out, `cleaning the local checkout ${f.remote}\nremoved workspace ${name} (idle 0 days)\n`);
  assert.equal(await exists(workspaceDir), false);
});

test("gc without a host in a non-colocated repository cleans the main checkout and spares the workspace it runs from", needsJj, async (t) => {
  const root = await realpath(await mkdtemp(path.join(os.tmpdir(), "dotcore-remote-exec-gc-")));
  t.after(() => rm(root, { force: true, recursive: true }));
  const main_ = path.join(root, "main");
  jj(root, "git", "init", "--no-colocate", main_);
  await writeFile(path.join(main_, ".gitignore"), ".workspaces/\n");
  jj(main_, "commit", "-m", "base");
  const idle = "remote-idle-00000001";
  const current = "remote-current-00000002";
  await mkdir(path.join(main_, ".workspaces"));
  for (const name of [idle, current]) {
    jj(main_, "workspace", "add", "--name", name, path.join(main_, ".workspaces", name));
  }

  const io = collectingIo({ env: {}, transport: {}, cwd: path.join(main_, ".workspaces", current) });
  assert.equal(await main(["gc", "--older-than", "0", "--yes"], io), 0, io.err);
  assert.equal(io.out, `cleaning the local checkout ${main_}\nremoved workspace ${idle} (idle 0 days)\n`);
  assert.equal(await exists(path.join(main_, ".workspaces", idle)), false);
  assert.ok(await exists(path.join(main_, ".workspaces", current)));
});

test("a lock without a holder counts as a starting run until it is old enough to be stale", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["sync", "loop"], f.io()), 0);
  const workspaceDir = await f.workspaceDir();
  const lockDir = await f.lockDir();
  await mkdir(lockDir, { recursive: true });

  const refused = f.io();
  assert.equal(await main(["run", "loop", "plain"], refused), TOOL_FAILURE_EXIT);
  assert.match(refused.err, /locked by another run that is starting; wait for it or rerun with --break-lock/);

  await writeFile(path.join(lockDir, "holder.json"), "");
  const spared = f.io();
  assert.equal(await main(["gc", "loop", "--older-than", "0", "--yes"], spared), 0, spared.err);
  assert.equal(spared.out, "nothing to remove\n");
  assert.ok(await exists(workspaceDir));

  f.env.REMOTE_EXEC_LOCK_GRACE_MS = "0";
  const takeover = f.io();
  assert.equal(await main(["run", "loop", "plain"], takeover), 0, takeover.err);
  assert.equal(await f.lockExists(), false);

  await mkdir(lockDir);
  const removed = f.io();
  assert.equal(await main(["gc", "loop", "--older-than", "0", "--yes"], removed), 0, removed.err);
  assert.equal(removed.out, `removed workspace ${path.basename(workspaceDir)} (idle 0 days)\n`);
  assert.equal(await exists(workspaceDir), false);
  assert.equal(await f.lockExists(), false);
});

test("gc spares a branch pushed moments ago by a run that has not taken its lock yet", needsJj, async (t) => {
  const f = await fixture(t);
  const sha = jj(f.local, "log", "-r", "@", "--no-graph", "-T", "commit_id");
  spawnSync("git", ["--git-dir", jj(f.local, "git", "root"), "push", "--force", `${f.remote}/.git`, `${sha}:refs/heads/remote-exec/starting/run1`], { env: f.env });
  const branches = () => jj(f.remote, "bookmark", "list", "-T", 'name ++ "\\n"');

  const spared = f.io();
  assert.equal(await main(["gc", "loop", "--yes"], spared), 0, spared.err);
  assert.equal(spared.out, "nothing to remove\n");
  assert.equal(branches(), "remote-exec/starting/run1");

  f.env.REMOTE_EXEC_BRANCH_GRACE_MS = "0";
  const removed = f.io();
  assert.equal(await main(["gc", "loop", "--yes"], removed), 0, removed.err);
  assert.equal(removed.out, "removed bookmark remote-exec/starting/run1\n");
  assert.equal(branches(), "");
});

test("kill finishes a run whose holder died, marking it lost and freeing the lock", needsJj, async (t) => {
  const f = await fixture(t);
  const started = f.io();
  assert.equal(await main(["run", "loop", "--detach", "block"], started), 0, started.err);
  const [, id] = /^started run ([0-9a-f]{12})\n$/.exec(started.out);
  await waitFor(() => exists(f.started), "the detached command to start");
  const pid = Number((await readFile(f.pidOut, "utf8")).trim());
  // The command outlives its killed supervisor; release it so it does not linger after the test.
  try {
    await killWithDeadHolder(f, id);
  } finally {
    await writeFile(f.release, "");
    await waitFor(async () => !processAlive(pid), "the command to exit");
  }
});

async function killWithDeadHolder(f, id) {
  const holder = await f.readLock();
  process.kill(holder.pid, "SIGKILL");
  await waitFor(() => !processAlive(holder.pid), "the supervisor to die");

  const io = f.io();
  assert.equal(await main(["kill", "loop"], io), 0, io.err);
  assert.equal(io.out, `run ${id} had already died without recording an exit status; marked it lost and released its lock\n`);
  assert.equal(await f.lockExists(), false);

  const waiting = f.io();
  assert.equal(await main(["wait", "loop"], waiting), TOOL_FAILURE_EXIT);
  assert.match(waiting.err, new RegExp(`run ${id} was lost: its holder died before recording an exit status`));

  const status = f.io();
  await main(["status", "loop"], status);
  assert.match(status.out, new RegExp(` ${id} {2}lost `));
}

test("an exit file without a status is not read as a finished run", needsJj, async (t) => {
  const f = await fixture(t);
  assert.equal(await main(["run", "loop", "fail", "x"], f.io()), 7);
  const [id] = await f.runIds();
  await writeFile(path.join(f.remote, ".workspaces", ".remote-runs", path.basename(await f.workspaceDir()), id, "exit"), "");

  const waiting = f.io();
  assert.equal(await main(["wait", "loop", id], waiting), TOOL_FAILURE_EXIT);
  assert.match(waiting.err, new RegExp(`run ${id} ended without recording an exit status`));
});
