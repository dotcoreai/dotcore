//! Runs development commands on configured remote hosts against the current jj workspace.
//!
//! The local side snapshots the workspace, pushes that commit over ssh into the remote repository's
//! git store, and starts the remote half of this same file from the pushed commit, so the remote logic
//! always matches the code being synced. The remote half serializes runs per remote workspace, makes
//! the matching jj workspace current, runs the command inside the development shell, and reports the
//! command's exit code through an in-band marker that tells it apart from transport failures.
//! Remote workspaces are disposable and never touch the remote's own checkout, and nothing is pushed
//! to a shared remote.

import { execFileSync, spawn, spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import {
  closeSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  openSync,
  readdirSync,
  readFileSync,
  realpathSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import { readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { StringDecoder } from "node:string_decoder";
import { fileURLToPath, pathToFileURL } from "node:url";

/** Exit status for failures of the tool itself, as opposed to the remote command's own status. */
export const TOOL_FAILURE_EXIT = 125;

const CONFIG_ENV = "DOTCORE_REMOTE_CONFIG";
// Where this file lives in the repository; the remote half runs it from the pushed commit.
const TOOL_PATH = "dev/remote/remote-exec.mjs";
const EXIT_MARKER = "@@remote-exec-exit@@";
const EXIT_NEEDLE = `\n${EXIT_MARKER}`;
const KILL_GRACE_MS = 5000;
const DAY_MS = 86_400_000;
const DEFAULT_GC_DAYS = 7;
const LOCK_START_GRACE_MS = 30_000;
const BRANCH_GRACE_MS = 600_000;
const GC_WORKSPACE = /^remote-.+-[0-9a-f]{8}$/;
// The host-wide lock among the per-workspace ones; workspace names never start with a dot.
const HOST_LOCK = ".host";
const HOST_LOCK_WAIT_MS = 60_000;
// Cleanup after a failed operation waits less, since the failure may have been this very wait.
const FAILURE_LOCK_WAIT_MS = 5_000;
// Host-side record of the operations already swept, the pushed commits still visible, and each workspace's
// sync and edits.
const SWEEP_STATE = path.join(".workspaces", ".remote-sweep.json");
// How jj describes deleting a pushed branch; the sweep reads pushed commits from these operations.
const SWEPT_DELETION = "delete bookmark remote-exec/";
// The joined worker node file inside a workspace, at the default worker folder the workspace environment derives.
const WORKER_NODE_FILE = path.join(".dotcore", "two-node-worker", "worker.yaml");
const SAFE_NAME = /^[A-Za-z0-9][A-Za-z0-9_-]*$/;
const SAFE_ADDRESS = /^[A-Za-z0-9][A-Za-z0-9.-]*$/;
const SAFE_USER = /^[A-Za-z0-9_][A-Za-z0-9._-]*$/;
const SAFE_DIRECTORY = /^\/[A-Za-z0-9_.@+/-]+$/;

const USAGE = `usage:
  remote list [--json]
  remote hosts                     print the configured hosts as <group>:<host>, without connecting
  remote sync <host>
  remote run <host> [--detach] [--break-lock] [--] <just args...>
  remote status <host>
  remote exec <host> <shell command...>
  remote logs <host> [run id] [--tail <lines>]
  remote tail <host> [run id]      follow a run's output until it ends
  remote wait <host> [run id]      wait for a run, exit with its status
  remote kill <host> [run id]
  remote pull <host>               fetch the remote workspace commit and show its diff
  remote gc [<host>] [--older-than <days>] [--yes]    without a host, cleans the checkout you are in
  remote clean <host>              remove this workspace's remote copy, its runs and branches
  remote resolve <host>            print the host as <group>:<host>
<host> is <group> or <group>:<host>; see \`remote list\` for the configured hosts.`;

/** Failure reported to the user as a message, without a stack trace. */
export class ToolError extends Error {}

/**
 * Entry point of the local half: dispatches `list`, `sync` and `run`.
 * Returns the process exit status; output goes through `io`.
 */
export async function main(argv, io = defaultIo()) {
  try {
    const [command, ...rest] = argv;
    if (command === "remote-run") {
      return await runRemote(rest, io);
    }
    if (command === "list") {
      return await listCommand(rest, io);
    }
    if (command === "sync") {
      return await syncCommand(rest, io);
    }
    if (command === "run") {
      return await runCommand(rest, io);
    }
    if (command === "supervise") {
      return await superviseRun(rest, io);
    }
    const simple = {
      status: statusCommand,
      exec: execCommand,
      logs: logsCommand,
      gc: gcCommand,
      clean: cleanCommand,
      pull: pullCommand,
      resolve: resolveCommand,
      hosts: hostsCommand,
    };
    if (simple[command]) {
      return await simple[command](rest, io);
    }
    if (["tail", "wait", "kill"].includes(command)) {
      return await runIdCommand(command, rest, io);
    }
    throw new ToolError(USAGE);
  } catch (error) {
    if (!(error instanceof ToolError)) {
      throw error;
    }
    io.stderr(`${error.message}\n`);
    return TOOL_FAILURE_EXIT;
  }
}

/**
 * Parses and validates the host configuration. Group values are defaults that a host entry may
 * override; unknown keys are rejected so typos do not silently do nothing.
 */
export function parseConfig(text, source = "configuration") {
  let raw;
  try {
    raw = JSON.parse(text);
  } catch (error) {
    throw new ToolError(`${source} is not valid json: ${error.message}`);
  }
  rejectUnknownKeys(raw, ["groups"], source);
  if (!isObject(raw.groups) || Object.keys(raw.groups).length === 0) {
    throw new ToolError(`${source} needs a non-empty "groups" object`);
  }

  const groups = {};
  for (const [groupName, group] of Object.entries(raw.groups)) {
    requireMatch(groupName, SAFE_NAME, `${source}: group name`);
    rejectUnknownKeys(group, ["user", "key", "dir", "hosts"], `${source}: group ${groupName}`);
    if (!isObject(group.hosts) || Object.keys(group.hosts).length === 0) {
      throw new ToolError(`${source}: group ${groupName} needs a non-empty "hosts" object`);
    }

    groups[groupName] = Object.entries(group.hosts).map(([hostName, entry]) => {
      requireMatch(hostName, SAFE_NAME, `${source}: host name in group ${groupName}`);
      const where = `${source}: host ${groupName}:${hostName}`;
      const host = typeof entry === "string" ? { address: entry } : entry;
      rejectUnknownKeys(host, ["address", "user", "key", "dir"], where);

      const resolved = {
        group: groupName,
        name: hostName,
        address: host.address,
        user: host.user ?? group.user,
        key: host.key ?? group.key,
        dir: host.dir ?? group.dir,
      };
      requireMatch(resolved.address, SAFE_ADDRESS, `${where}: address`);
      requireMatch(resolved.dir, SAFE_DIRECTORY, `${where}: dir (absolute path without spaces)`);
      if (resolved.user !== undefined) {
        requireMatch(resolved.user, SAFE_USER, `${where}: user`);
      }
      if (resolved.key !== undefined && typeof resolved.key !== "string") {
        throw new ToolError(`${where}: key must be a path`);
      }
      return resolved;
    });
  }
  return { groups };
}

/** Loads the configuration file named by the environment, or explains how to create one. */
export async function loadConfig(env, readText = (file) => readFile(file, "utf8")) {
  const file = env[CONFIG_ENV];
  if (!file) {
    throw new ToolError(notConfiguredMessage());
  }
  let text;
  try {
    text = await readText(expandHome(file, env));
  } catch (error) {
    throw new ToolError(`cannot read ${CONFIG_ENV} file ${file}: ${error.message}`);
  }
  return parseConfig(text, file);
}

/** Resolves `<group>` or `<group>:<host>` to one configured host; a bare group uses its first host. */
export function resolveHost(config, spec) {
  const [groupName, hostName, ...extra] = String(spec ?? "").split(":");
  const hosts = config.groups[groupName];
  if (!hosts || extra.length > 0) {
    throw new ToolError(`unknown host group in "${spec}"; configured: ${Object.keys(config.groups).join(", ")}`);
  }
  if (hostName === undefined) {
    return hosts[0];
  }
  const host = hosts.find((candidate) => candidate.name === hostName);
  if (!host) {
    throw new ToolError(`unknown host "${hostName}" in group ${groupName}; configured: ${hosts.map((h) => h.name).join(", ")}`);
  }
  return host;
}

/**
 * Names the remote workspace for a local one. The hash of the local root and machine keeps two clones
 * or developers with the same workspace name apart, and it marks the workspace as created by this tool.
 */
export function remoteWorkspaceName(localName, localRoot, machine) {
  const hash = createHash("sha256").update(`${machine}\n${localRoot}`).digest("hex").slice(0, 8);
  const readable = localName.replace(/[^A-Za-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "workspace";
  return `remote-${readable}-${hash}`;
}

/** Arguments that make ssh non-interactive and use the configured identity, if any. */
export function sshOptions(host, env = process.env) {
  // A forwarded agent is never needed remotely, and forwarding it back to the host holding it loops.
  const options = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ForwardAgent=no"];
  if (host.key) {
    options.push("-i", expandHome(host.key, env), "-o", "IdentitiesOnly=yes");
  }
  if (host.user) {
    options.push("-l", host.user);
  }
  return options;
}

/** The ssh command line git uses for pushes; built from the same options as direct ssh calls. */
export function gitSshCommand(host, env = process.env) {
  return ["ssh", ...sshOptions(host, env)].map(shellQuote).join(" ");
}

/**
 * Builds the single command string the remote login shell receives. It is one single-quoted `sh -c`
 * argument without quotes or backslashes inside, so fish, zsh and bash all pass it through verbatim.
 * The script reads the remote half of this file from the pushed commit and runs it in the checkout's
 * development shell.
 */
export function bootstrapCommand({ dir, sha, payload }) {
  const encoded = Buffer.from(JSON.stringify(payload)).toString("base64");
  const script = [
    "set -eu",
    "t=$(mktemp -d)",
    'trap "rm -rf $t" EXIT',
    `git -C ${dir} cat-file blob ${sha}:${TOOL_PATH} > $t/remote-exec.mjs`,
    // An empty log format drops direnv's loading lines here and in the run it spawns; errors still print.
    "export DIRENV_LOG_FORMAT=",
    `direnv exec ${dir} node $t/remote-exec.mjs remote-run ${encoded}`,
  ].join("; ");
  return `sh -c '${script}'`;
}

/** Transport that reaches hosts over ssh and pushes with git over the same ssh options. */
export function createSshTransport(env = process.env) {
  return {
    async push(host, { gitDir, sha, refs }) {
      const url = `ssh://${host.user ? `${host.user}@` : ""}${host.address}${host.dir}/.git`;
      const result = spawnSync("git", ["--git-dir", gitDir, "push", "--force", url, ...refs.map((ref) => `${sha}:${ref}`)], {
        encoding: "utf8",
        env: { ...env, GIT_SSH_COMMAND: gitSshCommand(host, env) },
      });
      return { code: result.status ?? TOOL_FAILURE_EXIT, stderr: `${result.stderr ?? ""}${result.error?.message ?? ""}` };
    },

    async fetch(host, { gitDir, remoteRef, localRef }) {
      const url = `ssh://${host.user ? `${host.user}@` : ""}${host.address}${host.dir}/.git`;
      const result = spawnSync("git", ["--git-dir", gitDir, "fetch", "--no-tags", url, `+${remoteRef}:${localRef}`], {
        encoding: "utf8",
        env: { ...env, GIT_SSH_COMMAND: gitSshCommand(host, env) },
      });
      return { code: result.status ?? TOOL_FAILURE_EXIT, stderr: `${result.stderr ?? ""}${result.error?.message ?? ""}` };
    },

    exec(host, command, handlers) {
      return spawnStreaming("ssh", [...sshOptions(host, env), host.address, command], handlers, env);
    },
  };
}

/**
 * Spawns a process, forwards its output to the handlers and keeps its stdin open until it exits, which
 * is how the remote half learns the local side is gone. Aborting through the signal ends the process.
 */
export function spawnStreaming(command, args, { onStdout, onStderr, signal }, env = process.env) {
  return new Promise((resolve) => {
    const child = spawn(command, args, { env, stdio: ["pipe", "pipe", "pipe"], signal });
    const out = new StringDecoder("utf8");
    const err = new StringDecoder("utf8");
    child.stdout.on("data", (chunk) => onStdout(out.write(chunk)));
    child.stderr.on("data", (chunk) => onStderr(err.write(chunk)));
    child.stdin.on("error", () => {});
    child.on("error", (error) => {
      onStderr(`${error.message}\n`);
      resolve({ code: TOOL_FAILURE_EXIT });
    });
    child.on("close", (code, killedBy) => {
      onStdout(out.end());
      onStderr(err.end());
      resolve({ code: code ?? (killedBy ? 128 : TOOL_FAILURE_EXIT) });
    });
  });
}

async function listCommand(args, io) {
  const json = args.includes("--json");
  const unknown = args.filter((arg) => arg !== "--json");
  if (unknown.length > 0) {
    throw new ToolError(USAGE);
  }

  let config;
  try {
    config = await loadConfig(io.env);
  } catch (error) {
    if (error instanceof ToolError && !io.env[CONFIG_ENV]) {
      io.stdout(`${json ? JSON.stringify({ configured: false, hosts: [] }) : error.message}\n`);
      return 1;
    }
    throw error;
  }

  const hosts = Object.values(config.groups).flat();
  // One host at a time: concurrent connections send concurrent signing requests, which an agent that
  // prompts for each approval cannot serve.
  const results = [];
  for (const host of hosts) {
    results.push(await checkHost(host, io));
  }
  if (json) {
    io.stdout(`${JSON.stringify({ configured: true, hosts: results }, null, 2)}\n`);
  } else {
    for (const result of results) {
      io.stdout(`${result.group}:${result.host}  ${result.usable ? "usable" : `unusable: ${result.problem}`}\n`);
    }
  }
  return results.some((result) => result.usable) ? 0 : 1;
}

async function checkHost(host, io) {
  const base = { group: host.group, host: host.name, address: host.address };
  const script = [
    `if jj -R ${host.dir} --ignore-working-copy workspace list >/dev/null 2>&1; then echo repository:ok; else echo repository:fail; fi`,
    `if git -C ${host.dir} check-ignore -q .workspaces/remote-probe/x; then echo ignored:ok; else echo ignored:fail; fi`,
    `if direnv exec ${host.dir} sh -c "command -v node >/dev/null && command -v just >/dev/null" >/dev/null 2>&1; then echo shell:ok; else echo shell:fail; fi`,
  ].join("; ");

  const outputs = { out: "", err: "" };
  const result = await io.transport.exec(host, `sh -c '${script}'`, {
    onStdout: (text) => (outputs.out += text),
    onStderr: (text) => (outputs.err += text),
  });

  if (result.code !== 0) {
    return { ...base, usable: false, problem: transportProblem(outputs.err, result.code) };
  }
  const checks = Object.fromEntries(outputs.out.split("\n").filter(Boolean).map((line) => line.split(":")));
  const hints = {
    repository: `${host.dir} is not a jj repository with jj on the remote PATH`,
    ignored: "the remote must ignore .workspaces globally (add it to the remote user's global gitignore)",
    shell: `direnv exec in ${host.dir} must provide node and just (run direnv allow there)`,
  };
  const failed = Object.keys(hints).find((check) => checks[check] !== "ok");
  return { ...base, usable: failed === undefined, problem: failed ? hints[failed] : undefined, checks };
}

async function syncCommand(args, io) {
  const [spec] = positional(args, {}, 1, 1);
  const result = await deliver(spec, { op: "sync" }, io);
  if (result.code === 0) {
    io.stdout(`synced ${result.remote.sha.slice(0, 12)} to ${result.host.group}:${result.host.name} at ${result.remote.workspaceDir}\n`);
  }
  return result.code;
}

async function runCommand(args, io) {
  const [spec, ...after] = args;
  if (!spec) {
    throw new ToolError(USAGE);
  }
  // Only the tool's own options are read; the first other word starts the just arguments.
  const options = {};
  let index = 0;
  while (index < after.length && ["--detach", "--break-lock"].includes(after[index])) {
    options[after[index]] = true;
    index += 1;
  }
  if (after[index] === "--") {
    index += 1;
  }
  const justArgs = after.slice(index);
  if (justArgs.length === 0) {
    throw new ToolError(USAGE);
  }
  const op = { op: "run", args: justArgs, breakLock: Boolean(options["--break-lock"]), detach: Boolean(options["--detach"]) };
  return (await deliver(spec, op, io)).code;
}

async function statusCommand(args, io) {
  const [spec] = positional(args, {}, 1, 1);
  return (await deliver(spec, { op: "status" }, io)).code;
}

async function execCommand(args, io) {
  const [spec, ...words] = args;
  if (!spec || words.length === 0) {
    throw new ToolError(USAGE);
  }
  return (await deliver(spec, { op: "exec", command: words.join(" ") }, io)).code;
}

async function logsCommand(args, io) {
  const { options, words } = parseOptions(args, { "--tail": "value" });
  const [spec, id = null] = positional(words, {}, 1, 2);
  const tail = options["--tail"] === undefined ? null : wholeNumber(options["--tail"], "--tail", 1);
  return (await deliver(spec, { op: "logs", id, tail }, io)).code;
}

/** Commands that act on one recorded run, the latest one unless an id is given. */
async function runIdCommand(op, args, io) {
  const [spec, id = null] = positional(args, {}, 1, 2);
  return (await deliver(spec, { op, id }, io)).code;
}

async function gcCommand(args, io) {
  const { options, words } = parseOptions(args, { "--older-than": "value", "--yes": "flag" });
  const olderThanDays = options["--older-than"] === undefined ? DEFAULT_GC_DAYS : wholeNumber(options["--older-than"], "--older-than", 0);
  const yes = Boolean(options["--yes"]);

  const [spec] = positional(words, {}, 0, 1);
  if (spec === undefined) {
    return gcLocal({ olderThanDays, yes }, io);
  }
  return (await deliver(spec, { op: "gc", olderThanDays, yes }, io)).code;
}

/** Cleans the checkout the tool is run in, for use on the host itself without configuration or ssh. */
async function gcLocal({ olderThanDays, yes }, io) {
  const jj = (...args) => {
    const result = spawnSync("jj", args, { cwd: io.cwd, encoding: "utf8" });
    if (result.status !== 0) {
      throw new ToolError(`jj ${args[0]} failed: ${(result.stderr || result.error?.message || "").trim()}`);
    }
    return result.stdout.trim();
  };
  // The main checkout owns the remote workspaces; the repository's git store may live inside .jj, so it cannot locate it.
  let dir;
  try {
    dir = jj("workspace", "root", "--name", "default");
  } catch {
    dir = jj("workspace", "root");
    while (path.basename(path.dirname(dir)) === ".workspaces") {
      dir = path.dirname(path.dirname(dir));
    }
  }
  if (!existsSync(path.join(dir, ".jj", "repo"))) {
    throw new ToolError(`cannot find the main checkout of this repository (looked at ${dir}); run gc from inside it or one of its workspaces`);
  }
  io.stdout(`cleaning the local checkout ${dir}\n`);
  const note = (message) => io.stderr(`remote-exec: ${message}\n`);
  const ctx = { dir, io, note, payload: { olderThanDays, yes, bookmark: null }, keep: [jj("workspace", "root")] };
  return opGc(ctx);
}

async function cleanCommand(args, io) {
  const [spec] = positional(args, {}, 1, 1);
  return (await deliver(spec, { op: "clean" }, io)).code;
}

/** Prints the configured host a spec names, so callers can tell `gb10` and `gb10:<first host>` apart from other hosts. */
async function resolveCommand(args, io) {
  const [spec] = positional(args, {}, 1, 1);
  const host = resolveHost(await loadConfig(io.env), spec);
  io.stdout(`${host.group}:${host.name}\n`);
  return 0;
}

/** Prints every configured host as `<group>:<host>` in configuration order, without connecting to any. */
async function hostsCommand(args, io) {
  positional(args, {}, 0, 0);
  const config = await loadConfig(io.env);
  for (const host of Object.values(config.groups).flat()) {
    io.stdout(`${host.group}:${host.name}\n`);
  }
  return 0;
}

async function pullCommand(args, io) {
  const [spec] = positional(args, {}, 1, 1);
  let output = "";
  const result = await deliver(spec, { op: "pull" }, io, (text) => (output += text));
  if (result.code !== 0) {
    return result.code;
  }

  const match = /^pull-ref ([0-9a-f]{40}) ([0-9a-f]{40})$/m.exec(output);
  if (!match) {
    throw new ToolError("the remote did not report a commit to pull");
  }
  const [, sha, parent] = match;
  const local = localState(io.cwd);
  const localRef = `refs/remote-exec/${result.host.group}/${result.host.name}/${result.workspace}`;
  // Fetched by id, so the host needs no branch for it: jj keeps every commit it writes reachable through its own refs.
  const fetched = await io.transport.fetch(result.host, { gitDir: local.gitDir, remoteRef: sha, localRef });
  if (fetched.code !== 0) {
    throw new ToolError(`fetch from ${result.host.group}:${result.host.name} failed: ${fetched.stderr.trim() || `exit ${fetched.code}`}`);
  }

  const diff = spawnSync("git", ["--git-dir", local.gitDir, "diff", "--no-color", parent, sha], { encoding: "utf8" });
  if (diff.status !== 0) {
    throw new ToolError(`cannot diff the pulled commit against ${parent.slice(0, 12)}: ${diff.stderr.trim()}`);
  }
  io.stdout(`pulled ${sha.slice(0, 12)} into ${localRef}\n`);
  io.stdout(diff.stdout === "" ? "no changes in the remote workspace\n" : diff.stdout);
  return 0;
}

/** Splits words into the allowed options (anywhere among them) and the remaining positional words. */
function parseOptions(args, allowed) {
  const options = {};
  const words = [];
  for (let index = 0; index < args.length; index += 1) {
    const word = args[index];
    if (!word.startsWith("--")) {
      words.push(word);
      continue;
    }
    const kind = allowed[word];
    if (!kind) {
      throw new ToolError(`unknown option ${word}\n${USAGE}`);
    }
    if (kind === "value") {
      if (args[index + 1] === undefined) {
        throw new ToolError(`${word} needs a value`);
      }
      options[word] = args[index + 1];
      index += 1;
    } else {
      options[word] = true;
    }
  }
  return { options, words };
}

/** Checks the positional word count, rejecting options the command does not take. */
function positional(args, allowed, min, max) {
  const { words } = parseOptions(args, allowed);
  if (words.length < min || words.length > max) {
    throw new ToolError(USAGE);
  }
  return words;
}

function wholeNumber(text, name, minimum) {
  const value = Number(text);
  if (!Number.isInteger(value) || value < minimum) {
    throw new ToolError(`${name} needs a whole number of at least ${minimum}, got "${text}"`);
  }
  return value;
}

/**
 * Pushes the local workspace snapshot to the host and runs one remote operation against its matching
 * workspace. Output goes to `onOutput` (the terminal by default); the returned code is the operation's.
 */
async function deliver(spec, op, io, onOutput = io.stdout) {
  const config = await loadConfig(io.env);
  const host = resolveHost(config, spec);
  const local = localState(io.cwd);

  const runId = randomUUID().replace(/-/g, "").slice(0, 12);
  const workspace = remoteWorkspaceName(local.workspaceName, local.root, io.machine);
  const remote = { sha: local.sha, workspaceDir: `${host.dir}/.workspaces/${workspace}` };

  // Every operation reads the tool from the pushed commit through a ref jj never imports. Only operations that
  // move the workspace also push a branch, because jj sees a commit only once a branch carries it.
  const bookmark = ["sync", "run"].includes(op.op) ? `remote-exec/${workspace}/${runId}` : null;
  const refs = [bootRef(workspace), ...(bookmark ? [`refs/heads/${bookmark}`] : [])];
  const pushed = await io.transport.push(host, { gitDir: local.gitDir, sha: local.sha, refs });
  if (pushed.code !== 0) {
    throw new ToolError(`push to ${host.group}:${host.name} failed: ${pushed.stderr.trim() || `exit ${pushed.code}`}`);
  }

  const payload = { dir: host.dir, workspace, sha: local.sha, bookmark, runId, ...op };
  const filter = new ExitMarkerFilter(onOutput);
  let errors = "";
  const controller = new AbortController();
  const stop = () => controller.abort();
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);

  let result;
  try {
    result = await io.transport.exec(host, bootstrapCommand({ dir: host.dir, sha: local.sha, payload }), {
      onStdout: (text) => filter.push(text),
      onStderr: (text) => {
        errors += text;
        io.stderr(text);
      },
      signal: controller.signal,
    });
  } finally {
    process.off("SIGINT", stop);
    process.off("SIGTERM", stop);
  }
  filter.end();

  if (filter.code !== null) {
    return { code: filter.code, host, remote, workspace };
  }
  // No marker: the remote half never finished, so the status is not the command's.
  io.stderr(`remote-exec: ${transportProblem(errors, result.code)} (exit ${result.code}, not from the command)\n`);
  return { code: TOOL_FAILURE_EXIT, host, remote, workspace };
}

/** Reads the local workspace identity and snapshots it into a commit. */
function localState(cwd) {
  const jj = (...args) => {
    const result = spawnSync("jj", args, { cwd, encoding: "utf8" });
    if (result.status !== 0) {
      throw new ToolError(`jj ${args[0]} failed: ${(result.stderr || result.error?.message || "").trim()}`);
    }
    return result.stdout.trim();
  };

  const root = jj("workspace", "root");
  const [conflict, sha] = jj("log", "-r", "@", "--no-graph", "-T", 'conflict ++ "|" ++ commit_id').split("|");
  if (conflict === "true") {
    throw new ToolError("the working copy commit has conflicts; resolve them before syncing");
  }
  const owners = jj("workspace", "list", "-T", 'name ++ "|" ++ target.commit_id() ++ "\n"')
    .split("\n")
    .map((line) => line.split("|"))
    .filter(([, commit]) => commit === sha)
    .map(([name]) => name);
  const workspaceName = owners.length === 1 ? owners[0] : owners.find((name) => name === path.basename(root));
  if (!workspaceName) {
    throw new ToolError(`cannot tell which workspace owns the working copy commit (${owners.join(", ") || "none"})`);
  }
  return { root, sha, workspaceName, gitDir: jj("git", "root") };
}

/** Removes the exit marker line from a stream of remote output while forwarding everything else. */
export class ExitMarkerFilter {
  constructor(write) {
    this.write = write;
    this.pending = "";
    this.tail = null;
    this.code = null;
  }

  push(text) {
    if (this.tail !== null) {
      this.tail += text;
      return;
    }
    this.pending += text;
    const at = this.pending.indexOf(EXIT_NEEDLE);
    if (at >= 0) {
      this.write(this.pending.slice(0, at));
      this.tail = this.pending.slice(at + EXIT_NEEDLE.length);
      this.pending = "";
      return;
    }
    // Hold back a suffix that could still grow into the marker.
    let keep = Math.min(this.pending.length, EXIT_NEEDLE.length - 1);
    while (keep > 0 && !EXIT_NEEDLE.startsWith(this.pending.slice(this.pending.length - keep))) {
      keep -= 1;
    }
    this.write(this.pending.slice(0, this.pending.length - keep));
    this.pending = this.pending.slice(this.pending.length - keep);
  }

  end() {
    if (this.tail === null) {
      this.write(this.pending);
      this.pending = "";
      return;
    }
    const match = /^(\d+)\n/.exec(this.tail);
    this.code = match ? Number(match[1]) : null;
  }
}

function transportProblem(stderr, code) {
  if (/host key verification failed/i.test(stderr)) {
    return "ssh does not trust the host key yet; run ssh to the host once by hand and accept it";
  }
  if (/permission denied/i.test(stderr)) {
    return "ssh authentication failed; check the configured user and key";
  }
  if (/could not resolve|timed out|no route|connection refused/i.test(stderr)) {
    return `cannot reach the host: ${stderr.trim().split("\n").pop()}`;
  }
  return `remote half failed with exit ${code}: ${stderr.trim().split("\n").pop() || "no output"}`;
}

/**
 * Remote half. Dispatches one operation against the remote workspace, then reports its exit code
 * in-band. Losing stdin means the local side is gone, so a running command is stopped and its lock
 * released; the pushed branch that carried this commit, if any, is always removed.
 */
async function runRemote([encoded], io) {
  const payload = decodePayload(encoded);
  const ctx = remoteContext(payload, io);
  const operation = REMOTE_OPERATIONS[payload.op];
  if (!operation) {
    throw new ToolError(`unknown remote operation "${payload.op}"`);
  }

  const abort = () => {
    ctx.aborted = true;
    ctx.onAbort?.();
  };
  process.stdin.on("end", abort);
  process.stdin.on("error", abort);
  process.stdin.resume();
  process.once("SIGTERM", abort);
  process.once("SIGHUP", abort);
  // A dropped connection fails writes to the output streams; handling it keeps the cleanup below running.
  process.stdout.on("error", abort);
  process.stderr.on("error", abort);

  try {
    const code = await operation(ctx);
    io.stdout(`${EXIT_NEEDLE}${code}\n`);
    return 0;
  } finally {
    if (payload.bookmark) {
      try {
        const waitMs = Math.min(FAILURE_LOCK_WAIT_MS, hookNumber("REMOTE_EXEC_HOST_LOCK_WAIT_MS", HOST_LOCK_WAIT_MS));
        withHostLock(ctx, () => dropBookmark(ctx.dir, payload.bookmark), waitMs);
      } catch {
        // Without the lock only git forgets the branch; jj's next import drops it and the commit nobody else needs.
        try {
          deleteGitRef(ctx.dir, `refs/heads/${payload.bookmark}`);
        } catch {
          // gc removes a branch left behind here.
        }
      }
    }
    process.stdin.destroy();
  }
}

/** Detached half of a background run: owns the command, its log, its exit record and the lock. */
async function superviseRun([encoded], io) {
  let child = null;
  let stopped = false;
  const stop = () => {
    stopped = true;
    if (child) {
      killGroup(child.pid);
    }
  };
  // Registered first so a kill that arrives while the supervisor starts still ends the run cleanly.
  process.once("SIGTERM", stop);
  process.once("SIGHUP", stop);

  const ctx = remoteContext(decodePayload(encoded), io);
  const runDir = path.join(ctx.runsDir, ctx.runId);
  writeFileSync(path.join(runDir, "pid"), String(process.pid));

  try {
    const code = await spawnGroup(ctx, ["just", ...ctx.payload.args], ["ignore", "inherit", "inherit"], (spawned) => {
      child = spawned;
      if (stopped) {
        killGroup(child.pid);
      }
    });
    writeFileAtomic(path.join(runDir, "exit"), String(code));
    return code;
  } finally {
    releaseLock(ctx.lockDir, ctx.runId);
  }
}

const REMOTE_OPERATIONS = {
  sync: opSync,
  run: opRun,
  status: opStatus,
  exec: opExec,
  logs: opLogs,
  tail: opTail,
  wait: opWait,
  kill: opKill,
  gc: opGc,
  clean: opClean,
  pull: opPull,
};

async function opSync(ctx) {
  acquireLock(ctx.lockDir, lockHolder(ctx.runId), ctx.payload.breakLock);
  try {
    prepareWorkspace(ctx);
    return 0;
  } finally {
    releaseLock(ctx.lockDir, ctx.runId);
  }
}

async function opRun(ctx) {
  const { args, detach, breakLock } = ctx.payload;
  acquireLock(ctx.lockDir, lockHolder(ctx.runId), breakLock);
  let handedOver = false;
  try {
    prepareWorkspace(ctx);
    const runDir = createRunRecord(ctx);
    ctx.note(`run ${ctx.runId}: just ${args.join(" ")}`);

    if (detach) {
      startSupervisor(ctx, runDir);
      handedOver = true;
      ctx.io.stdout(`started run ${ctx.runId}\n`);
      return 0;
    }

    const log = openSync(path.join(runDir, "log"), "a");
    try {
      const code = await spawnGroup(ctx, ["just", ...args], ["ignore", "pipe", "pipe"], (child) => {
        writeFileSync(path.join(runDir, "pid"), String(child.pid));
        child.stdout.on("data", (chunk) => {
          writeSync(log, chunk);
          ctx.io.stdout(chunk);
        });
        child.stderr.on("data", (chunk) => {
          writeSync(log, chunk);
          ctx.io.stderr(chunk);
        });
      });
      writeFileAtomic(path.join(runDir, "exit"), String(code));
      if (ctx.aborted) {
        ctx.note("local side went away; command stopped");
        return TOOL_FAILURE_EXIT;
      }
      return code;
    } finally {
      closeSync(log);
    }
  } finally {
    if (!handedOver) {
      releaseLock(ctx.lockDir, ctx.runId);
    }
  }
}

async function opStatus(ctx) {
  const lines = [];
  if (!existsSync(ctx.workspaceDir)) {
    lines.push(`workspace: ${ctx.workspaceDir} (missing; run sync first)`);
  } else {
    const jj = (...args) => runTool("jj", args, ctx.workspaceDir).trim();
    const changes = jj("diff", "--summary");
    lines.push(
      `workspace: ${ctx.workspaceDir} (present)`,
      `synced commit: ${jj("log", "-r", "@-", "--no-graph", "-T", 'commit_id.short(12) ++ " " ++ description.first_line()')}`,
      `changes: ${changes === "" ? "none" : `\n${changes.split("\n").map((line) => `  ${line}`).join("\n")}`}`,
    );
  }

  const holder = readHolder(ctx.lockDir);
  lines.push(`lock: ${holder && lockAlive(holder) ? `held by run ${holder.runId} (pid ${holder.pid})` : "free"}`);

  const runs = listRuns(ctx);
  lines.push(runs.length === 0 ? "runs: none" : "runs:");
  for (const run of runs) {
    lines.push(`  ${run.id}  ${run.state}  ${run.startedAt}  just ${run.args.join(" ")}`);
  }
  ctx.io.stdout(`${lines.join("\n")}\n`);
  return 0;
}

async function opExec(ctx) {
  requireWorkspace(ctx);
  return spawnGroup(ctx, ["sh", "-c", ctx.payload.command], ["ignore", "inherit", "inherit"]);
}

async function opLogs(ctx) {
  const text = readFileSync(path.join(ctx.runsDir, resolveRun(ctx), "log"), "utf8");
  const { tail } = ctx.payload;
  if (tail === null) {
    ctx.io.stdout(text);
    return 0;
  }
  const lines = text.split("\n");
  const kept = lines.slice(-(text.endsWith("\n") ? tail + 1 : tail));
  ctx.io.stdout(kept.join("\n"));
  return 0;
}

async function opTail(ctx) {
  return followRun(ctx, true);
}

async function opWait(ctx) {
  return followRun(ctx, false);
}

async function opKill(ctx) {
  const id = resolveRun(ctx);
  const holder = readHolder(ctx.lockDir);
  if (!holder || holder.runId !== id) {
    throw new ToolError(`run ${id} is not running`);
  }
  try {
    process.kill(holder.pid, "SIGTERM");
  } catch (error) {
    if (error.code !== "ESRCH") {
      throw new ToolError(`cannot signal run ${id} (pid ${holder.pid}): ${error.code ?? error.message}`);
    }
  }

  const deadline = Date.now() + KILL_GRACE_MS * 2;
  const holdsLock = () => readHolder(ctx.lockDir)?.runId === id;
  while (holdsLock() && processAlive(holder.pid) && Date.now() < deadline) {
    await sleep(ctx.pollMs);
  }
  if (holdsLock() && !processAlive(holder.pid)) {
    // The holder died without its cleanup, so nothing else will record the run's end or free the lock.
    writeFileAtomic(path.join(ctx.runsDir, id, "lost"), "the run's holder died before recording an exit status\n");
    releaseLock(ctx.lockDir, id);
    ctx.io.stdout(`run ${id} had already died without recording an exit status; marked it lost and released its lock\n`);
    return 0;
  }
  if (holdsLock()) {
    throw new ToolError(`run ${id} did not stop within ${(KILL_GRACE_MS * 2) / 1000} seconds`);
  }
  ctx.io.stdout(`stopped run ${id}\n`);
  return 0;
}

async function opGc(ctx) {
  const { olderThanDays, yes } = ctx.payload;
  const { io } = ctx;
  const jj = (...args) => repoJj(ctx.dir, ...args);
  const lockDirOf = (workspace) => path.join(ctx.dir, ".workspaces", ".remote-locks", workspace);
  const olderThanMs = olderThanDays * DAY_MS;
  const items = [];

  // A run pushes its branch before it takes the lock, so a young branch can belong to a run that is starting.
  const graceMs = hookNumber("REMOTE_EXEC_BRANCH_GRACE_MS", BRANCH_GRACE_MS);
  const gitDir = jj("git", "root");
  withHostLock(ctx, () => jj("git", "import"));
  const bookmarks = jj("bookmark", "list", "glob:remote-exec/*", "-T", 'name ++ "\\n"').split("\n").filter(Boolean);
  for (const name of bookmarks.filter((candidate) => candidate !== ctx.payload.bookmark)) {
    const parts = name.split("/");
    const workspace = parts[1] === "pull" ? parts[2] : parts[1];
    if (!lockHeld(lockDirOf(workspace)) && Date.now() - branchTime(ctx.dir, gitDir, name) >= graceMs) {
      items.push({ kind: "bookmark", name, remove: () => withHostLock(ctx, () => dropBookmark(ctx.dir, name, true)) });
    }
  }

  const known = jj("workspace", "list", "-T", 'name ++ "\\n"').split("\n").filter(Boolean);
  for (const name of known.filter((candidate) => GC_WORKSPACE.test(candidate))) {
    const workspaceDir = path.join(ctx.dir, ".workspaces", name);
    if (lockHeld(lockDirOf(name)) || !existsSync(workspaceDir) || ctx.keep?.includes(workspaceDir)) {
      continue;
    }
    // Removing the folder of a joined worker would orphan its workloads; only its node leave cleans them up.
    if (existsSync(path.join(workspaceDir, WORKER_NODE_FILE))) {
      io.stdout(`skipped workspace ${name}: it holds a joined worker node; clean it with just dev::cluster::clean first\n`);
      continue;
    }
    const last = lastActivity(ctx.dir, name);
    if (last === null) {
      io.stdout(`skipped workspace ${name}: neither its working copy state nor its run records tell when it was last used\n`);
      continue;
    }
    const idleMs = Date.now() - last;
    if (idleMs < olderThanMs) {
      continue;
    }
    items.push({
      kind: "workspace",
      name,
      detail: `idle ${Math.floor(idleMs / DAY_MS)} days`,
      remove: () => removeWorkspace(ctx, name, lockDirOf(name), olderThanMs),
    });
  }

  let failures = 0;
  for (const item of items) {
    const detail = item.detail ? ` (${item.detail})` : "";
    if (!yes) {
      io.stdout(`would remove ${item.kind} ${item.name}${detail}\n`);
      continue;
    }
    try {
      const keptBecause = item.remove();
      io.stdout(keptBecause ? `kept ${item.kind} ${item.name}: ${keptBecause}\n` : `removed ${item.kind} ${item.name}${detail}\n`);
    } catch (error) {
      failures += 1;
      io.stderr(`remote-exec: cannot remove ${item.kind} ${item.name}: ${error.message}\n`);
    }
  }
  if (yes) {
    withHostLock(ctx, () => {
      const state = readSweepState(ctx.dir);
      sweepPushed(ctx, state);
      writeSweepState(ctx.dir, state);
    });
  }
  if (items.length === 0) {
    io.stdout("nothing to remove\n");
  } else if (!yes) {
    io.stdout(`${items.length} item(s) found; rerun with --yes to remove them\n`);
  }
  if (failures > 0) {
    io.stderr(`remote-exec: ${failures} item(s) could not be removed\n`);
    return 1;
  }
  return 0;
}

async function opPull(ctx) {
  requireWorkspace(ctx);
  runTool("jj", ["status"], ctx.workspaceDir);
  const [sha, parent] = runTool("jj", ["log", "-r", "@", "--no-graph", "-T", 'commit_id ++ " " ++ parents.map(|p| p.commit_id()).join(",")'], ctx.workspaceDir).trim().split(" ");
  ctx.io.stdout(`pull-ref ${sha} ${parent}\n`);
  return 0;
}

/** Streams a run's log from the start until it finishes; returns the run's own exit code. */
async function followRun(ctx, show) {
  const id = resolveRun(ctx);
  const runDir = path.join(ctx.runsDir, id);
  let offset = 0;
  for (;;) {
    const code = readExit(runDir);
    const finished = code !== null;
    const data = readFileSync(path.join(runDir, "log")).subarray(offset);
    offset += data.length;
    if (show && data.length > 0) {
      ctx.io.stdout(data);
    }
    if (finished && data.length === 0) {
      return code;
    }
    if (!finished && existsSync(path.join(runDir, "lost")) && data.length === 0) {
      ctx.note(`run ${id} was lost: its holder died before recording an exit status`);
      return TOOL_FAILURE_EXIT;
    }
    const holder = readHolder(ctx.lockDir);
    if (!finished && !(holder?.runId === id && lockAlive(holder)) && data.length === 0) {
      ctx.note(`run ${id} ended without recording an exit status`);
      return TOOL_FAILURE_EXIT;
    }
    if (ctx.aborted) {
      return TOOL_FAILURE_EXIT;
    }
    await sleep(ctx.pollMs);
  }
}

/**
 * Runs a command in the workspace's development shell in its own process group so the whole tree can
 * be stopped. The group is signalled when the local side disappears.
 */
function spawnGroup(ctx, command, stdio, onSpawn = () => {}) {
  return new Promise((resolve) => {
    const child = spawn("direnv", ["exec", ctx.workspaceDir, ...command], { cwd: ctx.workspaceDir, detached: true, stdio });
    ctx.onAbort = () => killGroup(child.pid);
    child.on("error", (error) => {
      ctx.note(`cannot start the command: ${error.message}`);
      resolve(TOOL_FAILURE_EXIT);
    });
    child.on("close", (status, signal) => resolve(status ?? 128 + (os.constants.signals[signal] ?? 0)));
    onSpawn(child);
    if (ctx.aborted) {
      killGroup(child.pid);
    }
  });
}

/**
 * Moves the remote workspace onto the pushed commit. Commits made in the workspace move along with it, and
 * pushed commits nothing needs any more are swept, so the host's history keeps only the current sync.
 */
function prepareWorkspace(ctx) {
  const { dir, workspace, workspaceDir } = ctx;
  const { sha, bookmark } = ctx.payload;
  withHostLock(ctx, () => {
    const state = readSweepState(dir);
    repoJj(dir, "git", "import");
    const known = repoJj(dir, "workspace", "list", "-T", 'name ++ "\\n"').split("\n");
    if (known.includes(workspace)) {
      runTool("jj", ["status"], workspaceDir);
      const { synced, head } = workspaceCommits(dir, workspace, state);
      runTool("jj", ["new", sha], workspaceDir);
      carryEdits(ctx, state, workspace, { synced, head, onto: sha });
    } else {
      // The one command that cannot skip the host checkout's snapshot.
      runTool("jj", ["workspace", "add", "--name", workspace, "-r", sha, workspaceDir], dir);
    }
    state.synced[workspace] = sha;
    // The pushed branch only exists so jj can see the commit; the workspace change keeps it reachable.
    dropBookmark(dir, bookmark, true);
    sweepPushed(ctx, state);
    writeSweepState(dir, state);
  });
  // Synced code can change the environment file, which direnv trusts by content.
  runTool("direnv", ["allow", workspaceDir], dir);
  ctx.note(`workspace ${workspaceDir} at ${sha.slice(0, 12)}`);
}

/**
 * Removes a pushed branch from git and jj. The import first makes sure jj knows the branch, so its deletion is
 * recorded in the operation log, where the sweep finds the commit it carried.
 */
function dropBookmark(dir, bookmark, strict = false) {
  try {
    repoJj(dir, "git", "import");
    repoJj(dir, "bookmark", "delete", bookmark);
    deleteGitRef(dir, `refs/heads/${bookmark}`);
  } catch (error) {
    if (strict) {
      throw error;
    }
  }
}

/**
 * Forgets a remote workspace, then moves its edits off the commit it last synced, so neither its empty change
 * nor its edits keep the pushed chain visible. The edits stay in history on the sync's immutable fork point.
 */
function forgetWorkspace(ctx, workspace, state) {
  const { dir } = ctx;
  const { synced, head } = workspaceCommits(dir, workspace, state);
  repoJj(dir, "workspace", "forget", workspace);
  // A forgotten workspace's change stays visible, even when it holds nothing.
  const leftover = `present(${head}) & empty() & description(exact:"")`;
  if (selects(dir, leftover)) {
    repoJj(dir, "abandon", leftover);
  }
  carryEdits(ctx, state, workspace, { synced, head, onto: `heads(::present(${synced}) & immutable())` });
  delete state.synced[workspace];
  delete state.edits[workspace];
}

/** The commit a remote workspace last synced and its current change, read before the workspace moves. */
function workspaceCommits(dir, workspace, state) {
  const commitOf = (revset) => repoJj(dir, "log", "-r", revset, "--no-graph", "-T", 'commit_id ++ "\\n"').split("\n")[0];
  return { synced: state.synced[workspace] ?? commitOf(`${workspace}@-`), head: commitOf(`${workspace}@`) };
}

/**
 * Rebases a remote workspace's own edits onto a new base: the commits between its synced commit and its former
 * change, and the edits carried at earlier syncs. Nothing else built on the synced commit moves, and immutable
 * or checked-out commits stay. A failed rebase only leaves the old chain visible, so it is reported without
 * failing the operation.
 */
function carryEdits(ctx, state, workspace, { synced, head, onto }) {
  const { dir } = ctx;
  const own = [`(::(${visibleCommits([head])}) ~ ::present(${synced}))`, ...(state.edits[workspace] ?? []).map((id) => `change_id(${id})`)];
  const roots = `roots((${own.join(" | ")}) & mutable() ~ ::working_copies())`;
  try {
    const edits = repoJj(dir, "log", "-r", roots, "--no-graph", "-T", 'change_id ++ "\\n"').split("\n").filter(Boolean);
    if (edits.length > 0) {
      repoJj(dir, "rebase", "-s", roots, "-o", onto);
    }
    state.edits[workspace] = edits;
  } catch (error) {
    if (!(error instanceof ToolError)) {
      throw error;
    }
    ctx.note(`could not move the remote workspace's edits off ${synced.slice(0, 12)}: ${error.message}`);
  }
}

/**
 * Abandons the pushed commits that nothing needs any more, then records which pushed commits are still visible.
 * Every pushed commit was once the target of a remote-exec branch, so the deletions in the operation log name
 * them all, including those of older versions of this tool and of other workspaces. A pushed commit stays
 * while another head, a bookmark, a tag or a working copy reaches it. A failed sweep only leaves leftovers, so
 * it is reported without failing the operation.
 */
function sweepPushed(ctx, state) {
  const { dir } = ctx;
  try {
    const deletions = repoJj(dir, "op", "log", "--no-graph", "-T", `if(description.starts_with("${SWEPT_DELETION}"), id.short(32) ++ " " ++ description.first_line() ++ "\\n")`)
      .split("\n")
      .filter(Boolean)
      .map((line) => ({ op: line.slice(0, line.indexOf(" ")), names: line.slice(line.indexOf(" ") + 1 + "delete bookmark ".length).split(", ") }));
    const scanned = new Set(state.scannedOps);
    const pushed = new Set(state.pushed);
    for (const { op, names } of deletions.filter((deletion) => !scanned.has(deletion.op))) {
      // A pull branch of older versions followed the workspace's change, which is not a pushed commit.
      const branches = names.filter((name) => name.startsWith("remote-exec/") && !name.startsWith("remote-exec/pull/"));
      if (branches.length === 0) {
        continue;
      }
      try {
        // A deletion is never a merge, so the operation before it is its single parent.
        const revset = branches.map((name) => `bookmarks(exact:"${name}")`).join(" | ");
        const targets = runTool("jj", ["--at-op", `${op}-`, "log", "-r", revset, "--no-graph", "-T", 'commit_id ++ "\\n"'], dir);
        for (const sha of targets.split("\n").filter(Boolean)) {
          pushed.add(sha);
        }
      } catch (error) {
        if (!(error instanceof ToolError)) {
          throw error;
        }
        ctx.note(`could not read the commit operation ${op.slice(0, 12)} deleted a branch from: ${error.message}`);
      }
    }
    state.scannedOps = deletions.map(({ op }) => op);

    const candidates = visibleCommits([...pushed]);
    // Visible heads only: naming a hidden pushed commit makes it part of all(), where it would count as a head.
    const kept = `::(visible_heads() ~ ::(${candidates})) | ::(bookmarks() | remote_bookmarks() | tags() | working_copies())`;
    const unneeded = `(::(${candidates}) ~ (${kept})) & mutable()`;
    if (selects(dir, unneeded)) {
      repoJj(dir, "abandon", unneeded);
    }
    state.pushed = repoJj(dir, "log", "-r", candidates, "--no-graph", "-T", 'commit_id ++ "\\n"').split("\n").filter(Boolean);
  } catch (error) {
    if (!(error instanceof ToolError)) {
      throw error;
    }
    ctx.note(`could not sweep pushed commits that are no longer needed: ${error.message}`);
  }
}

/** The host's sweep state, or an empty one: the first sweep then scans the whole operation log. */
function readSweepState(dir) {
  let saved = {};
  try {
    saved = JSON.parse(readFileSync(path.join(dir, SWEEP_STATE), "utf8"));
  } catch {
    // Missing or unreadable state only costs a full scan.
  }
  return { scannedOps: saved.scannedOps ?? [], pushed: saved.pushed ?? [], synced: saved.synced ?? {}, edits: saved.edits ?? {} };
}

function writeSweepState(dir, state) {
  mkdirSync(path.dirname(path.join(dir, SWEEP_STATE)), { recursive: true });
  writeFileAtomic(path.join(dir, SWEEP_STATE), JSON.stringify(state));
}

/**
 * Runs an action while holding the host-wide lock that serializes changes to the host's commit graph across
 * remote workspaces. Concurrent jj operations would otherwise merge, and rebase what one sweep abandoned under
 * a commit another sync just imported. The lock is held for seconds, so a busy one is waited for.
 */
function withHostLock(ctx, action, waitMs = hookNumber("REMOTE_EXEC_HOST_LOCK_WAIT_MS", HOST_LOCK_WAIT_MS)) {
  const lockDir = path.join(ctx.dir, ".workspaces", ".remote-locks", HOST_LOCK);
  const holder = lockHolder(`host-${ctx.runId ?? randomUUID().slice(0, 12)}`);
  const deadline = Date.now() + waitMs;
  for (;;) {
    try {
      acquireLock(lockDir, holder, false);
      break;
    } catch (error) {
      if (!(error instanceof ToolError)) {
        throw error;
      }
      if (Date.now() >= deadline) {
        throw new ToolError(`the host repository stayed locked by another remote-exec operation (${lockDir})`);
      }
      sleepSync(100);
    }
  }
  try {
    return action();
  } finally {
    releaseLock(lockDir, holder.runId);
  }
}

/** A revset of the given commits that are still visible; hidden or unknown ids select nothing. */
function visibleCommits(ids) {
  return ids.length === 0 ? "none()" : `(${ids.map((id) => `present(${id})`).join(" | ")}) & ::visible_heads()`;
}

/** Whether a revset selects any visible commit in the host checkout. */
function selects(dir, revset) {
  return repoJj(dir, "log", "-r", revset, "--no-graph", "-T", '"x"') !== "";
}

/** Runs jj in the host checkout without snapshotting the host owner's working copy. */
function repoJj(dir, ...args) {
  return runTool("jj", ["--ignore-working-copy", ...args], dir).trim();
}

/** Deletes a git ref of the host repository, whose git store may live inside its .jj folder. */
function deleteGitRef(dir, ref) {
  runTool("git", ["--git-dir", repoJj(dir, "git", "root"), "update-ref", "-d", ref], dir);
}

/** The ref every operation pushes so the bootstrap can read the tool, outside the refs jj imports. */
function bootRef(workspace) {
  return `refs/remote-exec/${workspace}/boot`;
}

function createRunRecord(ctx) {
  const runDir = path.join(ctx.runsDir, ctx.runId);
  mkdirSync(runDir, { recursive: true });
  writeFileSync(path.join(runDir, "cmd.json"), JSON.stringify({ args: ctx.payload.args, sha: ctx.payload.sha, startedAt: new Date().toISOString() }));
  writeFileSync(path.join(runDir, "log"), "");

  const stale = readdirSync(ctx.runsDir)
    .map((id) => ({ id, time: statSync(path.join(ctx.runsDir, id)).mtimeMs }))
    .sort((a, b) => b.time - a.time)
    .slice(Number(process.env.REMOTE_EXEC_MAX_RUNS) || 20);
  for (const { id } of stale) {
    rmSync(path.join(ctx.runsDir, id), { recursive: true, force: true });
  }
  return runDir;
}

function startSupervisor(ctx, runDir) {
  // The runner copy outlives the temporary directory the bootstrap removes when it exits.
  const runner = path.join(runDir, "runner.mjs");
  copyFileSync(fileURLToPath(import.meta.url), runner);
  const log = openSync(path.join(runDir, "log"), "a");
  const child = spawn(process.execPath, [runner, "supervise", Buffer.from(JSON.stringify(ctx.payload)).toString("base64")], {
    detached: true,
    stdio: ["ignore", log, log],
  });
  child.unref();
  closeSync(log);

  // The lock now belongs to the supervisor, unless it already finished and released it.
  const holder = readHolder(ctx.lockDir);
  if (holder?.runId !== ctx.runId) {
    return;
  }
  try {
    writeHolder(ctx.lockDir, { ...holder, pid: child.pid });
  } catch (error) {
    if (error.code !== "ENOENT") {
      throw error;
    }
  }
}

function listRuns(ctx) {
  if (!existsSync(ctx.runsDir)) {
    return [];
  }
  const holder = readHolder(ctx.lockDir);
  return readdirSync(ctx.runsDir)
    .map((id) => {
      const runDir = path.join(ctx.runsDir, id);
      const command = JSON.parse(readFileSync(path.join(runDir, "cmd.json"), "utf8"));
      const code = readExit(runDir);
      const running = holder?.runId === id && lockAlive(holder);
      const state = code !== null ? `exit ${code}` : running ? "running" : "lost";
      return { id, state, startedAt: command.startedAt, args: command.args };
    })
    .sort((a, b) => a.startedAt.localeCompare(b.startedAt));
}

function resolveRun(ctx) {
  if (ctx.payload.id) {
    if (!existsSync(path.join(ctx.runsDir, ctx.payload.id))) {
      throw new ToolError(`no run ${ctx.payload.id} is recorded for this workspace`);
    }
    return ctx.payload.id;
  }
  const runs = listRuns(ctx);
  if (runs.length === 0) {
    throw new ToolError("no runs are recorded for this workspace");
  }
  return runs[runs.length - 1].id;
}

function requireWorkspace(ctx) {
  if (!existsSync(ctx.workspaceDir)) {
    throw new ToolError(`remote workspace ${ctx.workspaceDir} does not exist; run sync first`);
  }
}

/** When a workspace was last used, or null when nothing records it. */
function lastActivity(dir, workspace) {
  const candidates = [path.join(dir, ".workspaces", workspace, ".jj", "working_copy"), path.join(dir, ".workspaces", ".remote-runs", workspace)];
  const times = candidates.filter((file) => existsSync(file)).map((file) => statSync(file).mtimeMs);
  return times.length === 0 ? null : Math.max(...times);
}

/**
 * Removes this workspace's remote copy regardless of age: its folder, run records, refs and the pushed commits
 * only it needed. Unsaved remote edits stay in the repository's history. A joined worker must leave first, and
 * a run holding the lock keeps it.
 */
async function opClean(ctx) {
  const { workspace, workspaceDir } = ctx;
  // Removing the folder of a joined worker would orphan its workloads; only its node leave cleans them up.
  if (existsSync(path.join(workspaceDir, WORKER_NODE_FILE))) {
    throw new ToolError(`workspace ${workspace} holds a joined worker node; remove it with just dev::cluster::clean first`);
  }
  acquireLock(ctx.lockDir, lockHolder(ctx.runId), false);
  try {
    withHostLock(ctx, () => {
      const state = readSweepState(ctx.dir);
      if (existsSync(workspaceDir)) {
        runTool("jj", ["status"], workspaceDir);
      }
      // Older versions of this tool also left a pull branch per workspace, which would keep its change in place.
      const bookmarks = repoJj(ctx.dir, "bookmark", "list", `glob:remote-exec/${workspace}/*`, `glob:remote-exec/pull/${workspace}`, "-T", 'name ++ "\\n"');
      for (const name of bookmarks.split("\n").filter(Boolean)) {
        dropBookmark(ctx.dir, name, true);
      }
      if (repoJj(ctx.dir, "workspace", "list", "-T", 'name ++ "\\n"').split("\n").includes(workspace)) {
        forgetWorkspace(ctx, workspace, state);
      }
      rmSync(workspaceDir, { recursive: true, force: true });
      rmSync(ctx.runsDir, { recursive: true, force: true });
      deleteGitRef(ctx.dir, bootRef(workspace));
      sweepPushed(ctx, state);
      writeSweepState(ctx.dir, state);
    });
    ctx.io.stdout(`removed workspace ${workspace}\n`);
    return 0;
  } finally {
    releaseLock(ctx.lockDir, ctx.runId);
  }
}

/**
 * Removes an idle remote workspace with its run records. It holds the workspace lock meanwhile, so a run
 * that started after the listing is left alone; returns why the workspace was kept, or null once removed.
 */
function removeWorkspace(ctx, name, lockDir, olderThanMs) {
  const holder = lockHolder(`gc-${randomUUID().slice(0, 8)}`);
  try {
    acquireLock(lockDir, holder, false);
  } catch (error) {
    if (error instanceof ToolError) {
      return "a run took its lock after the listing";
    }
    throw error;
  }

  try {
    const last = lastActivity(ctx.dir, name);
    if (last === null || Date.now() - last < olderThanMs) {
      return "it was used after the listing";
    }
    const workspaceDir = path.join(ctx.dir, ".workspaces", name);
    withHostLock(ctx, () => {
      const state = readSweepState(ctx.dir);
      // Snapshot first so unsaved remote edits are kept in the repository's history.
      runTool("jj", ["status"], workspaceDir);
      forgetWorkspace(ctx, name, state);
      rmSync(workspaceDir, { recursive: true, force: true });
      rmSync(path.join(ctx.dir, ".workspaces", ".remote-runs", name), { recursive: true, force: true });
      deleteGitRef(ctx.dir, bootRef(name));
      writeSweepState(ctx.dir, state);
    });
    return null;
  } finally {
    releaseLock(lockDir, holder.runId);
  }
}

/**
 * When a branch was last pushed: its ref or reflog file time, falling back to its commit time once
 * git has packed the ref.
 */
function branchTime(dir, gitDir, name) {
  const files = [path.join(gitDir, "refs", "heads", name), path.join(gitDir, "logs", "refs", "heads", name)];
  const times = files.filter((file) => existsSync(file)).map((file) => statSync(file).mtimeMs);
  try {
    times.push(Number(runTool("git", ["log", "-1", "--format=%ct", `refs/heads/${name}`], dir).trim()) * 1000);
  } catch {
    // A branch jj knows but git no longer has is dated by its files alone.
  }
  return times.length === 0 ? 0 : Math.max(...times);
}

function remoteContext(payload, io) {
  const { dir, workspace, runId } = payload;
  return {
    payload,
    io,
    dir,
    workspace,
    runId,
    workspaceDir: `${dir}/.workspaces/${workspace}`,
    lockDir: `${dir}/.workspaces/.remote-locks/${workspace}`,
    runsDir: `${dir}/.workspaces/.remote-runs/${workspace}`,
    pollMs: Number(process.env.REMOTE_EXEC_POLL_MS) || 500,
    note: (message) => io.stderr(`remote-exec: ${message}\n`),
    aborted: false,
    onAbort: null,
  };
}

function decodePayload(encoded) {
  return JSON.parse(Buffer.from(encoded, "base64").toString("utf8"));
}

function lockHolder(runId) {
  return { pid: process.pid, host: os.hostname(), runId, startedAt: new Date().toISOString() };
}

function lockAlive(holder) {
  return holder.host !== os.hostname() || processAlive(holder.pid);
}

function runTool(command, args, cwd) {
  try {
    return execFileSync(command, args, { cwd, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  } catch (error) {
    throw new ToolError(`${command} ${args[0]} failed in ${cwd}: ${(error.stderr || error.message).trim()}`);
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Blocks for a while, for waits inside the synchronous jj steps. */
function sleepSync(ms) {
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
}

function acquireLock(lockDir, holder, breakLock) {
  mkdirSync(path.dirname(lockDir), { recursive: true });
  for (let attempt = 0; attempt < 3; attempt += 1) {
    try {
      mkdirSync(lockDir);
    } catch (error) {
      if (error.code !== "EEXIST") {
        throw error;
      }
      const current = readHolder(lockDir);
      if (!breakLock && lockHeld(lockDir, current)) {
        const who = current ? `run ${current.runId} (pid ${current.pid} on ${current.host}, since ${current.startedAt})` : "another run that is starting";
        throw new ToolError(`remote workspace is locked by ${who}; wait for it or rerun with --break-lock`);
      }
      removeLock(lockDir, current?.runId ?? null);
      continue;
    }
    writeHolder(lockDir, holder);
    return;
  }
  throw new ToolError("could not take the remote workspace lock");
}

function releaseLock(lockDir, runId) {
  removeLock(lockDir, runId);
}

/**
 * Whether a live run holds the lock. A lock without a readable holder belongs to a run that is still
 * writing it, until it is old enough to have been left by a run that died in between.
 */
function lockHeld(lockDir, holder = readHolder(lockDir)) {
  if (holder) {
    return lockAlive(holder);
  }
  let age;
  try {
    age = Date.now() - statSync(lockDir).mtimeMs;
  } catch {
    return false;
  }
  return age < hookNumber("REMOTE_EXEC_LOCK_GRACE_MS", LOCK_START_GRACE_MS);
}

/**
 * Removes the lock if it still belongs to the given run (null for a lock without a holder). Renaming it
 * away first means two runs replacing the same stale lock cannot delete the one the other just took.
 */
function removeLock(lockDir, runId) {
  if ((readHolder(lockDir)?.runId ?? null) !== runId) {
    return;
  }
  const removed = `${lockDir}.removed-${randomUUID()}`;
  try {
    renameSync(lockDir, removed);
  } catch (error) {
    if (error.code === "ENOENT") {
      return;
    }
    throw error;
  }
  if ((readHolder(removed)?.runId ?? null) !== runId) {
    // Another run replaced the lock between the check and the rename; put its lock back.
    try {
      renameSync(removed, lockDir);
      return;
    } catch {
      // A third run already took the lock; the moved one is gone either way.
    }
  }
  rmSync(removed, { recursive: true, force: true });
}

function readHolder(lockDir) {
  try {
    return JSON.parse(readFileSync(path.join(lockDir, "holder.json"), "utf8"));
  } catch {
    return null;
  }
}

function writeHolder(lockDir, holder) {
  writeFileAtomic(path.join(lockDir, "holder.json"), JSON.stringify(holder));
}

/** Writes a file through a rename so readers see either no file or its whole content. */
function writeFileAtomic(file, text) {
  const temporary = `${file}.${process.pid}-${randomUUID()}.tmp`;
  writeFileSync(temporary, text);
  renameSync(temporary, file);
}

/** The run's recorded exit status, or null while none is fully written. */
function readExit(runDir) {
  let text;
  try {
    text = readFileSync(path.join(runDir, "exit"), "utf8").trim();
  } catch {
    return null;
  }
  return /^\d+$/.test(text) ? Number(text) : null;
}

/** Reads a numeric test hook from the environment, keeping the default when it is unset. */
function hookNumber(name, fallback) {
  const text = process.env[name];
  return text === undefined || text === "" || Number.isNaN(Number(text)) ? fallback : Number(text);
}

function processAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return error.code === "EPERM";
  }
}

function killGroup(pid) {
  try {
    process.kill(-pid, "SIGTERM");
  } catch {
    return;
  }
  setTimeout(() => {
    try {
      process.kill(-pid, "SIGKILL");
    } catch {
      // The group already exited.
    }
  }, KILL_GRACE_MS).unref();
}

function notConfiguredMessage() {
  return [
    `remote execution is not configured: set ${CONFIG_ENV} to a json file (for example in the parent .envrc) such as:`,
    JSON.stringify({
      groups: {
        gb10: { user: "me", dir: "/home/me/dotcore", hosts: { "gb10-host-1": "gb10-host-1.example.net" } },
      },
    }, null, 2),
    "`key` is optional (otherwise ssh uses your own configuration); a host may override user, key and dir.",
    "The host needs a jj checkout at dir, a global gitignore entry for .workspaces, direnv allowed in dir, and its ssh host key accepted once.",
  ].join("\n");
}

function defaultIo() {
  return {
    env: process.env,
    cwd: process.cwd(),
    machine: os.hostname(),
    transport: createSshTransport(),
    stdout: (text) => process.stdout.write(text),
    stderr: (text) => process.stderr.write(text),
  };
}

function expandHome(file, env) {
  return file.startsWith("~/") ? path.join(env.HOME ?? os.homedir(), file.slice(2)) : file;
}

function shellQuote(value) {
  return /^[A-Za-z0-9_.,:=@%+/-]+$/.test(value) ? value : `'${value.replace(/'/g, `'\\''`)}'`;
}

function isObject(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function rejectUnknownKeys(value, allowed, where) {
  if (!isObject(value)) {
    throw new ToolError(`${where} must be an object`);
  }
  const unknown = Object.keys(value).filter((key) => !allowed.includes(key));
  if (unknown.length > 0) {
    throw new ToolError(`${where} has unknown keys: ${unknown.join(", ")} (allowed: ${allowed.join(", ")})`);
  }
}

function requireMatch(value, pattern, what) {
  if (typeof value !== "string" || !pattern.test(value)) {
    throw new ToolError(`${what} is missing or has unsupported characters: ${JSON.stringify(value)}`);
  }
}

// Node resolves symlinks in the entry path (macOS temporary directories are symlinked), so compare real paths.
if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  process.exitCode = await main(process.argv.slice(2));
}
