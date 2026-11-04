//! Discovers workspaces and publishes stable workspace-local development environment values.
//!
//! Development slots are allocated under a process-owned lock in the main repository's `.dotcore`
//! directory. Both lock paths, `.workspace-environment-slot-allocation-v2.lock` and the
//! `.workspace-environment-slot-allocation.lock` hard link to it, stay in place by design: removing or
//! replacing either one lets two workspaces take the same slot. Waiting on the lock is bounded, and a
//! timeout leaves the workspace unassigned. The handoff is covered by tests on Linux; macOS uses the
//! same locking path without a runtime test.

import { spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { constants } from "node:fs";
import { link, lstat, mkdir, open, readFile, realpath, rename, stat, unlink } from "node:fs/promises";
import net from "node:net";
import { performance } from "node:perf_hooks";
import path from "node:path";
import { pathToFileURL } from "node:url";

/**
 * Ordered source of truth for variables the development-environment helper derives.
 * Known inputs select `.env.local > inherited shell > .env.base`; extra local
 * variables pass through unchanged.
 */
export const ENVIRONMENT_VARIABLE_SCHEMA = Object.freeze({
  DOTCORE_MACHINE_HOST: {
    kind: "base-supported",
    deriveInput: "host",
    source: ".env.base",
    precedence: ".env.local > inherited shell > .env.base",
    purpose: "IPv4 address or DNS host name used to bind development services and derive endpoints.",
    validation: "Four decimal 0-255 octets, or a lowercase RFC 1123 name without trailing dot and with a non-numeric last label; 0.0.0.0 is rejected.",
    consumer: "Workspace environment consumers.",
  },
  DOTCORE_DEV_INDEX: {
    kind: "local-only",
    deriveInput: "index",
    source: ".env.local",
    precedence: ".env.local only",
    purpose: "Persistent workspace slot used to derive service and control ports.",
    validation: "Two digits; main root uses 00 and discovered roots are unique.",
    consumer: "Helper port derivation.",
  },
  DOTCORE_SITE_DIR: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated workspace-root value.",
    purpose: "Absolute root of the writable local workspace data directory.",
    validation: "Absolute normalized path to <workspace>/.dotcore/site.",
    consumer: "Workspace environment consumers.",
  },
  DOTCORE_CATALOG_DIR: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated workspace-root value.",
    purpose: "Absolute root of the immutable master catalog source.",
    validation: "Absolute normalized path to <workspace>/catalog.",
    consumer: "Backend catalog loader.",
  },
  DOTCORE_CACHE_DIR: {
    kind: "base-supported",
    deriveInput: "cacheRoot",
    source: ".env.base",
    precedence: ".env.local > inherited shell > .env.base > XDG_CACHE_HOME > HOME fallback",
    purpose: "Shared writable root of every Dotcore cache.",
    validation: "Absolute normalized non-root path.",
    consumer: "Workspace environment consumers.",
  },
  DOTCORE_BACKEND_RUN_MODE: {
    kind: "base-supported",
    deriveInput: "backendRunMode",
    source: ".env.base",
    precedence: ".env.local > inherited shell > .env.base > default dev",
    purpose: "Selects the Cargo development, Cargo release, or Nix backend command.",
    validation: "Exactly dev, dev-release, or nix; defaults to dev.",
    consumer: "Workspace command consumers.",
  },
  DOTCORE_FRONTEND_RUN_MODE: {
    kind: "base-supported",
    deriveInput: "frontendRunMode",
    source: ".env.base",
    precedence: ".env.local > inherited shell > .env.base > default dev",
    purpose: "Selects the Vite development or Nix static frontend command.",
    validation: "Exactly dev or nix; defaults to dev.",
    consumer: "Workspace command consumers.",
  },
  HF_XET_CACHE: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated cache path.",
    purpose: "hf-hub Xet transfer cache for catalog model artifacts.",
    validation: "Absolute normalized path below DOTCORE_CACHE_DIR.",
    consumer: "hf-hub Xet transfer implementation.",
  },
  DOTCORE_FRONTEND_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated slot value.",
    purpose: "Frontend development-server port.",
    validation: "21000 + slot * 10 + 0.",
    consumer: "Frontend Vite server.",
  },
  DOTCORE_MANAGEMENT_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated slot value.",
    purpose: "Management API listener port.",
    validation: "21000 + slot * 10 + 1.",
    consumer: "Management API listener.",
  },
  DOTCORE_INFERENCE_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated slot value.",
    purpose: "Inference router listener port.",
    validation: "21000 + slot * 10 + 2.",
    consumer: "Inference router listener.",
  },
  PC_SOCKET_PATH: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated workspace and slot value.",
    purpose: "Workspace-scoped Unix control socket.",
    validation: "Absolute path below /tmp with a deterministic filesystem-safe workspace namespace.",
    consumer: "dev/Justfile up and pc recipes.",
  },
  DOTCORE_FRONTEND_ORIGIN: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Frontend HTTP origin.",
    validation: "http://{host}:{frontendPort}.",
    consumer: "Frontend Vite server and backend CORS configuration.",
  },
  DOTCORE_MANAGEMENT_ADDR: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Management API listener address.",
    validation: "{host}:{managementPort}.",
    consumer: "Management API listener.",
  },
  DOTCORE_MANAGEMENT_ORIGIN: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Management HTTP endpoint reported by the helper.",
    validation: "http://{host}:{managementPort}.",
    consumer: "Helper status output; no direct runtime consumer.",
  },
  DOTCORE_INFERENCE_ADDR: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Inference router listener address.",
    validation: "{host}:{inferencePort}.",
    consumer: "Backend inference router listener.",
  },
  DOTCORE_INFERENCE_ORIGIN: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Inference router HTTP endpoint reported by the helper.",
    validation: "http://{host}:{inferencePort}.",
    consumer: "Helper status output; no direct runtime consumer.",
  },
  DOTCORE_PRIVATE_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated slot value.",
    purpose: "Manager private mutual-TLS listener port.",
    validation: "21000 + slot * 10 + 3.",
    consumer: "Manager and worker development workflows.",
  },
  DOTCORE_PRIVATE_ADDR: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Manager private mutual-TLS listener address.",
    validation: "{host}:{privatePort}.",
    consumer: "Backend private listener.",
  },
  DOTCORE_WORKER_PRIVATE_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated port value.",
    purpose: "Port of the worker's private mutual-TLS listener, recorded in its joined node folder.",
    validation: "21000 + slot * 10 + 4.",
    consumer: "Worker development workflow.",
  },
  DOTCORE_DEV_PUBLIC_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated slot value.",
    purpose: "Development ingress port; named apart from the bundle's public port so slot 00 can keep the bundle default.",
    validation: "21000 + slot * 10 + 5.",
    consumer: "Development ingress recipe and Vite HMR client.",
  },
  DOTCORE_PUBLIC_PORT: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated slot value.",
    purpose: "Public port of a bare dotcore started from the shell, so secondary workspaces do not share the main workspace's port.",
    validation: "8080 in slot 00, otherwise the development ingress port.",
    consumer: "Packaged bundle launcher.",
  },
  DOTCORE_PUBLIC_ORIGIN: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Public ingress HTTP origin that browsers open.",
    validation: "http://{host}:{publicPort}.",
    consumer: "Frontend Vite server asset origin and helper status output.",
  },
  DOTCORE_WORKER_DIR: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated workspace-root value.",
    purpose: "Disposable joined worker node folder, outside the manager site root.",
    validation: "Absolute path to <workspace>/.dotcore/two-node-worker.",
    consumer: "Worker development workflow.",
  },
  DOTCORE_DEV_SOURCE_FLAKE: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated workspace-root value.",
    purpose: "Flake URL of this workspace, from which a hosting node pins and builds `self` Nix engine artifacts.",
    validation: "git+file://<absolute workspace root, URL-escaped>.",
    consumer: "Backend node pinning and building of development-source Nix engine artifacts.",
  },
  VITE_DOTCORE_MANAGEMENT_ORIGIN: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Management HTTP origin exposed to browser code; the console calls it through the public ingress.",
    validation: "same-origin.",
    consumer: "Frontend clients.",
  },
  VITE_DOTCORE_INFERENCE_ORIGIN: {
    kind: "derived",
    source: "helper",
    precedence: ".env.local overrides the generated host and port value.",
    purpose: "Inference router HTTP origin exposed to browser code; the console calls it through the public ingress.",
    validation: "same-origin.",
    consumer: "Frontend static connection configuration.",
  },
  JUST_TEMPDIR: {
    kind: "base-supported",
    deriveInput: "justTempdir",
    source: ".env.base",
    precedence: ".env.local > inherited shell > .env.base",
    purpose: "Overrides the workspace command temporary directory.",
    validation:
      "Absolute path to a writable-intent directory; validated as an absolute path without requiring filesystem mutation during derivation.",
    consumer: "Workspace command consumers.",
  },
  RUST_LIB_BACKTRACE: {
    kind: "base-supported",
    deriveInput: "rustLibBacktrace",
    source: ".env.base",
    precedence: ".env.local > inherited shell > .env.base",
    purpose: "Controls library backtrace capture for development Rust errors.",
    validation: "Rust library backtrace setting; the tracked development default is 1.",
    consumer: "anyhow error leaves in backend development processes.",
  },
});

export const ENVIRONMENT_VARIABLES = Object.freeze(Object.keys(ENVIRONMENT_VARIABLE_SCHEMA));

const BASE_INPUT_VARIABLES = new Set(
  ENVIRONMENT_VARIABLES.filter(
    (variable) => ENVIRONMENT_VARIABLE_SCHEMA[variable].kind === "base-supported",
  ),
);
const LOCAL_INPUT_VARIABLES = new Set(
  ENVIRONMENT_VARIABLES.filter((variable) => ENVIRONMENT_VARIABLE_SCHEMA[variable].kind !== "derived"),
);
const ENVIRONMENT_ARTIFACT = path.join(".direnv", "dotcore-development.env");
const JJ_ROOT_TEMPLATE = 'json(self.root()) ++ "\\n"';
const DOTENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;
const DOTENV_UNQUOTED_VALUE = /^[A-Za-z0-9_./:@%+=,-]*$/;
const TWO_DIGIT_INDEX = /^\d{2}$/;
const SLOT_PORT_BASE = 21000;
const SLOT_PORT_STRIDE = 10;
// Public port default of the packaged bundle.
const BUNDLE_PUBLIC_PORT = 8080;
const IPV4_OCTET = /^(0|[1-9]\d{0,2})$/;
const DEFAULT_RUN_MODE = "dev";
const BACKEND_RUN_MODES = new Set(["dev", "dev-release", "nix"]);
const FRONTEND_RUN_MODES = new Set(["dev", "nix"]);
// Browser code resolves this to its own origin, so the console reaches every API through the public ingress.
const SAME_ORIGIN = "same-origin";
const DEFAULT_FILE_SYSTEM = { link, open, unlink };
const DEFAULT_SLOT_LOCK_FILE_SYSTEM = { link, lstat, mkdir, open, realpath };
const SLOT_LOCK_WAIT_MILLISECONDS = 5000;
const SLOT_LOCK_HELPER_TIMEOUT_MILLISECONDS = 2000;
const DISCOVERY_TIMEOUT_MILLISECONDS = 5000;
const WORKER_LOOPBACK_HOST = "127.0.0.2";
const LOOPBACK_PROBE_TIMEOUT_MS = 1000;
const DEFAULT_ARTIFACT_FILE_SYSTEM = { mkdir, open, readFile, rename, unlink };
const DEFAULT_RUNTIME_CONFIG_FILE_SYSTEM = { mkdir, open, readFile, rename, unlink };

const LOCAL_SITE_PATH = path.join(".dotcore", "site");
const PROCESS_COMPOSE_SOCKET_DIRECTORY = "/tmp";

function diagnostic(message, cause) {
  return new Error(cause ? `${message}: ${cause.message}` : message);
}

function normalizeRoot(root, source) {
  if (typeof root !== "string" || !path.isAbsolute(root)) {
    throw new Error(`${source} contained a non-absolute workspace root.`);
  }
  return path.normalize(root);
}

function deduplicateRoots(roots) {
  return [...new Set(roots)];
}

function commandOutput(command, args, { cwd, env }, description) {
  const result = spawnSync(command, args, { cwd, encoding: "utf8", env, timeout: DISCOVERY_TIMEOUT_MILLISECONDS });
  if (result.error) {
    throw diagnostic(`${description} could not start`, result.error);
  }
  if (result.status !== 0) {
    const detail = result.stderr.trim() || `exit status ${result.status}`;
    throw new Error(`${description} failed: ${detail}`);
  }
  return result.stdout;
}

async function isDirectory(filePath) {
  try {
    return (await stat(filePath)).isDirectory();
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw diagnostic(`Cannot inspect '${filePath}'`, error);
  }
}

async function hasJjMetadata(root) {
  try {
    await stat(path.join(root, ".jj"));
    return true;
  } catch (error) {
    if (error.code === "ENOENT") {
      return false;
    }
    throw diagnostic(`Cannot inspect Jujutsu metadata in '${root}'`, error);
  }
}

export function parseJjWorkspaceRoots(output) {
  const lines = output.split("\n").filter((line) => line.length > 0);
  if (lines.length === 0) {
    throw new Error("Jujutsu workspace discovery returned no JSON-lines roots.");
  }

  return lines.map((line) => {
    let root;
    try {
      root = JSON.parse(line);
    } catch (error) {
      throw diagnostic("Jujutsu workspace discovery returned malformed JSON-lines output", error);
    }
    return normalizeRoot(root, "Jujutsu workspace discovery");
  });
}

export function parseGitWorktreePorcelain(output) {
  if (!output.endsWith("\0\0")) {
    throw new Error("Git worktree discovery returned malformed output without a final record separator.");
  }
  const fields = output.split("\0");
  const roots = [];
  let record;
  let expectingWorktree = true;

  const completeRecord = () => {
    if (record === undefined) {
      return;
    }
    if (record.bare) {
      if (
        record.hasHead ||
        record.branch !== undefined ||
        record.detached ||
        record.locked ||
        record.prunable
      ) {
        throw new Error("Git worktree discovery returned an invalid bare worktree record.");
      }
      return;
    }
    if (!record.hasHead || (record.branch === undefined && !record.detached)) {
      throw new Error("Git worktree discovery returned an incomplete porcelain record.");
    }
  };

  for (const [index, field] of fields.entries()) {
    if (index === fields.length - 1) {
      if (field !== "" || !expectingWorktree) {
        throw new Error("Git worktree discovery returned malformed porcelain output.");
      }
      continue;
    }
    if (expectingWorktree) {
      if (!field.startsWith("worktree ")) {
        throw new Error("Git worktree discovery returned malformed porcelain record boundaries.");
      }
      const root = field.slice("worktree ".length);
      if (root === "") {
        throw new Error("Git worktree discovery returned a malformed worktree record.");
      }
      roots.push(normalizeRoot(root, "Git worktree discovery"));
      record = {
        bare: false,
        branch: undefined,
        detached: false,
        hasHead: false,
        locked: false,
        prunable: false,
      };
      expectingWorktree = false;
      continue;
    }
    if (field === "") {
      completeRecord();
      record = undefined;
      expectingWorktree = true;
      continue;
    }
    if (record === undefined) {
      throw new Error("Git worktree discovery returned malformed porcelain output.");
    }
    if (/^HEAD [0-9a-f]{40}(?:[0-9a-f]{24})?$/.test(field)) {
      if (record.hasHead) {
        throw new Error("Git worktree discovery returned a malformed porcelain record.");
      }
      record.hasHead = true;
      continue;
    }
    if (field === "bare") {
      if (record.bare) {
        throw new Error("Git worktree discovery returned a malformed porcelain record.");
      }
      record.bare = true;
      continue;
    }
    if (field.startsWith("branch ")) {
      if (record.branch !== undefined || record.detached || field.length === "branch ".length) {
        throw new Error("Git worktree discovery returned a malformed porcelain record.");
      }
      record.branch = field.slice("branch ".length);
      continue;
    }
    if (field === "detached") {
      if (record.branch !== undefined || record.detached) {
        throw new Error("Git worktree discovery returned a malformed porcelain record.");
      }
      record.detached = true;
      continue;
    }
    if (field === "locked" || /^locked .+$/.test(field)) {
      if (record.locked || record.prunable) {
        throw new Error("Git worktree discovery returned a malformed porcelain record.");
      }
      record.locked = true;
      continue;
    }
    if (field === "prunable" || /^prunable .+$/.test(field)) {
      if (record.prunable || record.locked) {
        throw new Error("Git worktree discovery returned a malformed porcelain record.");
      }
      record.prunable = true;
      continue;
    }
    throw new Error("Git worktree discovery returned malformed porcelain output.");
  }

  if (roots.length === 0) {
    throw new Error("Git worktree discovery returned no worktree records.");
  }
  return roots;
}

export async function discoverWorkspaceRoots({
  commands = { git: "git", jj: "jj" },
  cwd = process.cwd(),
  env = process.env,
} = {}) {
  const currentRoot = path.resolve(cwd);
  if (await hasJjMetadata(currentRoot)) {
    const jjOutput = commandOutput(
      commands.jj,
      ["workspace", "list", "-T", JJ_ROOT_TEMPLATE],
      { cwd: currentRoot, env },
      "Jujutsu workspace discovery",
    );
    const jjRoots = parseJjWorkspaceRoots(jjOutput);
    const mainRoots = [];
    for (const root of jjRoots) {
      if (await isDirectory(path.join(root, ".jj", "repo"))) {
        mainRoots.push(root);
      }
    }
    if (mainRoots.length !== 1) {
      throw new Error(
        "Unable to identify exactly one main Jujutsu repository root from '.jj/repo' metadata.",
      );
    }

    const mainRoot = mainRoots[0];
    const gitOutput = commandOutput(
      commands.git,
      ["-C", mainRoot, "worktree", "list", "--porcelain", "-z"],
      { cwd: currentRoot, env },
      "Git worktree discovery",
    );
    const roots = deduplicateRoots([...jjRoots, ...parseGitWorktreePorcelain(gitOutput)]);
    if (!roots.includes(currentRoot)) {
      throw new Error(`The current root '${currentRoot}' is not present in shared workspace metadata.`);
    }
    return { currentRoot, mainRoot, roots };
  }

  const gitOutput = commandOutput(
    commands.git,
    ["worktree", "list", "--porcelain", "-z"],
    { cwd: currentRoot, env },
    "Git worktree discovery",
  );
  const roots = deduplicateRoots(parseGitWorktreePorcelain(gitOutput));
  const mainRoot = roots[0];
  if (!roots.includes(currentRoot)) {
    throw new Error(`The current root '${currentRoot}' is not present in shared workspace metadata.`);
  }
  return { currentRoot, mainRoot, roots };
}

function parseDotenv(contents, filePath, { allowedKeys, forbiddenKeys } = {}) {
  const values = Object.create(null);
  for (const [lineNumber, rawLine] of contents.split(/\r?\n/).entries()) {
    const line = rawLine.trim();
    if (line === "" || line.startsWith("#")) {
      continue;
    }
    const separator = line.indexOf("=");
    if (separator === -1) {
      throw new Error(`'${filePath}' has malformed dotenv syntax on line ${lineNumber + 1}.`);
    }
    const key = line.slice(0, separator).trim();
    let value = line.slice(separator + 1).trim();
    if (!DOTENV_KEY.test(key)) {
      throw new Error(`'${filePath}' has an invalid dotenv key '${key}'.`);
    }
    if (Object.hasOwn(values, key)) {
      throw new Error(`'${filePath}' defines '${key}' more than once.`);
    }
    if ((allowedKeys !== undefined && !allowedKeys.has(key)) || forbiddenKeys?.has(key)) {
      throw new Error(`'${filePath}' cannot define '${key}'.`);
    }
    if (
      (value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))
    ) {
      value = value.slice(1, -1);
    }
    values[key] = value;
  }
  return values;
}

async function readDotenv(filePath, options = {}) {
  const { optional = false } = options;
  try {
    return parseDotenv(await readFile(filePath, "utf8"), filePath, options);
  } catch (error) {
    if (optional && error.code === "ENOENT") {
      return undefined;
    }
    if (error instanceof Error && error.message.startsWith("'")) {
      throw error;
    }
    throw diagnostic(`Cannot read '${filePath}'`, error);
  }
}

export function environmentArtifactPath(root = process.cwd()) {
  return path.join(path.resolve(root), ENVIRONMENT_ARTIFACT);
}

function serializeDotenvValue(value) {
  if (DOTENV_UNQUOTED_VALUE.test(value)) {
    return value;
  }
  if (value.includes("'")) {
    return JSON.stringify(value);
  }
  return `'${value}'`;
}

export function serializeEnvironment(environment) {
  const variables = [
    ...ENVIRONMENT_VARIABLES,
    ...Object.keys(environment)
      .filter((variable) => !ENVIRONMENT_VARIABLES.includes(variable))
      .sort(),
  ];
  return `${variables.map((variable) => {
    if (!DOTENV_KEY.test(variable)) {
      throw new Error(`Cannot serialize the invalid '${variable}' development environment variable.`);
    }
    const value = environment[variable];
    if (typeof value !== "string" || value.includes("\n") || value.includes("\r")) {
      throw new Error(`Cannot serialize the '${variable}' development environment value.`);
    }
    return `${variable}=${serializeDotenvValue(value)}`;
  }).join("\n")}\n`;
}

export async function publishEnvironmentArtifact(
  root,
  environment,
  { fileSystem = DEFAULT_ARTIFACT_FILE_SYSTEM } = {},
) {
  const artifactPath = environmentArtifactPath(root);
  const artifactContents = serializeEnvironment(environment);
  const directory = path.dirname(artifactPath);
  await fileSystem.mkdir(directory, { recursive: true });

  try {
    if ((await fileSystem.readFile(artifactPath, "utf8")) === artifactContents) {
      return { artifactPath, published: false };
    }
  } catch (error) {
    if (error.code !== "ENOENT") {
      throw diagnostic(`Cannot read generated development environment artifact '${artifactPath}'`, error);
    }
  }

  const temporaryPath = path.join(
    directory,
    `.${path.basename(artifactPath)}.${randomUUID()}.tmp`,
  );
  let handle;
  try {
    handle = await fileSystem.open(temporaryPath, "wx", 0o600);
    await handle.writeFile(artifactContents);
    await handle.sync();
    await handle.close();
    handle = undefined;
    await fileSystem.rename(temporaryPath, artifactPath);
    return { artifactPath, published: true };
  } catch (error) {
    throw diagnostic(`Cannot atomically publish '${artifactPath}'`, error);
  } finally {
    try {
      await handle?.close();
    } catch (error) {
      throw diagnostic(`Cannot close generated development environment artifact '${temporaryPath}'`, error);
    }
    try {
      await fileSystem.unlink(temporaryPath);
    } catch (error) {
      if (error.code !== "ENOENT") {
        throw diagnostic(`Cannot clean generated development environment artifact '${temporaryPath}'`, error);
      }
    }
  }
}
function parseIndex(index, root, mainRoot) {
  if (typeof index !== "string" || !TWO_DIGIT_INDEX.test(index)) {
    throw new Error(`'${root}/.env.local' must set DOTCORE_DEV_INDEX to a two-digit value from 00 to 99.`);
  }
  const numericIndex = Number(index);
  if (root === mainRoot && numericIndex !== 0) {
    throw new Error(`The main root '${root}' must reserve DOTCORE_DEV_INDEX=00.`);
  }
  if (root !== mainRoot && numericIndex === 0) {
    throw new Error(`Secondary root '${root}' cannot use DOTCORE_DEV_INDEX=00.`);
  }
  return numericIndex;
}

const HOST_ERROR =
  "DOTCORE_MACHINE_HOST must be a specific IPv4 literal or a lowercase DNS name without trailing dot, not 0.0.0.0 or an IPv6 literal.";
const DNS_LABEL = /^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$/;

// Mirrors the backend node host type: an IPv4 literal or an RFC 1123 DNS name in canonical form.
function validateHost(host) {
  if (typeof host !== "string") {
    throw new Error(HOST_ERROR);
  }
  if (isIpv4(host)) {
    if (host === "0.0.0.0") {
      throw new Error("DOTCORE_MACHINE_HOST cannot use the wildcard address 0.0.0.0.");
    }
    return host;
  }
  if (!isDnsName(host)) {
    throw new Error(HOST_ERROR);
  }
  return host;
}

function isIpv4(host) {
  const octets = host.split(".");
  return octets.length === 4 && octets.every((octet) => IPV4_OCTET.test(octet) && Number(octet) <= 255);
}

function isDnsName(host) {
  if (host.length > 253) {
    return false;
  }
  const labels = host.split(".");
  // A numeric or 0x-hex last label would make a malformed IPv4 literal (ex: "100.64.0", or
  // "0x7f000001", which the system resolver reads as 127.0.0.1) look like a name.
  return labels.every((label) => DNS_LABEL.test(label)) && !/^(\d+|0x[0-9a-f]*)$/.test(labels[labels.length - 1]);
}

function validateRunMode(mode, variable, supportedModes) {
  const selectedMode = mode === undefined ? DEFAULT_RUN_MODE : mode;
  if (!supportedModes.has(selectedMode)) {
    throw new Error(`${variable} must be one of: ${[...supportedModes].join(", ")}.`);
  }
  return selectedMode;
}

async function readDiscoveredAssignments({ roots, mainRoot }) {
  const assignments = new Map();
  const localValues = new Map();
  for (const root of roots) {
    const localPath = path.join(root, ".env.local");
    const values = await readDotenv(localPath, { optional: true });
    if (values === undefined) {
      continue;
    }
    const index = parseIndex(values.DOTCORE_DEV_INDEX, root, mainRoot);
    if (assignments.has(index)) {
      throw new Error(
        `Duplicate DOTCORE_DEV_INDEX=${String(index).padStart(2, "0")} in '${assignments.get(index)}' and '${root}'. Resolve the duplicate manually.`,
      );
    }
    assignments.set(index, root);
    localValues.set(root, values);
  }
  return { assignments, localValues };
}

export function chooseUnusedIndex(assignments, random) {
  const availableIndexes = [];
  for (let index = 1; index <= 99; index += 1) {
    if (!assignments.has(index)) {
      availableIndexes.push(index);
    }
  }
  if (availableIndexes.length === 0) {
    throw new Error("No unused development slots remain in 01-99.");
  }
  const value = random();
  if (!Number.isFinite(value) || value < 0 || value >= 1) {
    throw new Error("Development slot randomness must return a number from 0 (inclusive) to 1 (exclusive).");
  }
  return availableIndexes[Math.floor(value * availableIndexes.length)];
}

export async function createLocalFile(localPath, index, { fileSystem = DEFAULT_FILE_SYSTEM } = {}) {
  const temporaryPath = path.join(
    path.dirname(localPath),
    `.${path.basename(localPath)}.${randomUUID()}.tmp`,
  );
  let handle;
  let installing = false;
  try {
    handle = await fileSystem.open(temporaryPath, "wx", 0o600);
    await handle.writeFile(`DOTCORE_DEV_INDEX=${String(index).padStart(2, "0")}\n`);
    await handle.sync();
    await handle.close();
    handle = undefined;
    installing = true;
    await fileSystem.link(temporaryPath, localPath);
  } catch (error) {
    if (installing && error.code === "EEXIST") {
      throw new Error(`'${localPath}' was created concurrently; reload to validate its established assignment.`);
    }
    if (installing) {
      throw diagnostic(`Cannot confirm creation of '${localPath}'; assignment may already exist; reload to validate it`, error);
    }
    throw diagnostic(`Cannot atomically create '${localPath}'`, error);
  } finally {
    let cleanupError;
    try {
      await handle?.close();
    } catch (error) {
      cleanupError = diagnostic(`Cannot close temporary local environment file '${temporaryPath}'`, error);
    }
    try {
      await fileSystem.unlink(temporaryPath);
    } catch (error) {
      if (error.code !== "ENOENT") {
        cleanupError ??= diagnostic(`Cannot clean temporary local environment file '${temporaryPath}'`, error);
      }
    }
    if (cleanupError !== undefined) {
      throw installing
        ? diagnostic(`Cannot clean temporary file for '${localPath}'; assignment may already exist; reload to validate it`, cleanupError)
        : cleanupError;
    }
  }
}
async function withSlotAllocationLock(mainRoot, operation, {
  slotLockFileSystem = DEFAULT_SLOT_LOCK_FILE_SYSTEM,
  slotLockTimeoutMilliseconds = SLOT_LOCK_WAIT_MILLISECONDS,
  slotLockDelayMilliseconds = 10,
  onSlotLockContention,
  sleep = defaultSleep,
  env = process.env,
} = {}) {
  const canonicalRoot = await slotLockFileSystem.realpath(mainRoot);
  const directory = path.join(canonicalRoot, ".dotcore");
  const lockPath = path.join(directory, ".workspace-environment-slot-allocation-v2.lock");
  const legacyPath = path.join(directory, ".workspace-environment-slot-allocation.lock");
  await slotLockFileSystem.mkdir(directory, { recursive: true });
  const directoryIdentity = await slotLockFileSystem.lstat(directory, { bigint: true });
  if (!directoryIdentity.isDirectory() || directoryIdentity.isSymbolicLink()) {
    throw new Error(`Development slot lock directory '${directory}' must be a real directory.`);
  }

  let handle;
  try {
    handle = await slotLockFileSystem.open(lockPath, constants.O_CREAT | constants.O_RDWR | constants.O_NOFOLLOW, 0o600);
    await assertLockIdentity(slotLockFileSystem, directory, directoryIdentity, lockPath, handle);
    const deadline = performance.now() + slotLockTimeoutMilliseconds;
    let acquired = false;
    do {
      // The helper locks this shared open file description; the parent descriptor keeps it held.
      const result = spawnSync("flock", ["-n", "-E", "75", "3"], {
        env, encoding: "utf8",
        stdio: ["ignore", "ignore", "pipe", handle.fd],
        timeout: SLOT_LOCK_HELPER_TIMEOUT_MILLISECONDS,
      });
      if (result.error) throw diagnostic(`Cannot invoke development slot lock helper for '${lockPath}'`, result.error);
      if (result.status === 0) {
        acquired = true;
        break;
      }
      if (result.status !== 75) {
        throw new Error(`Development slot lock helper failed for '${lockPath}': ${result.stderr?.trim() || `exit status ${result.status}`}.`);
      }
      onSlotLockContention?.();
      const remaining = deadline - performance.now();
      if (remaining > 0) await sleep(Math.min(slotLockDelayMilliseconds, remaining));
    } while (performance.now() < deadline);
    if (!acquired) throw new Error(`Timed out waiting for development slot allocation lock '${lockPath}'.`);

    await assertLockIdentity(slotLockFileSystem, directory, directoryIdentity, lockPath, handle);
    await ensureLegacyMarker(slotLockFileSystem, lockPath, legacyPath, handle);
    await assertLockIdentity(slotLockFileSystem, directory, directoryIdentity, lockPath, handle);
    const result = await operation(canonicalRoot);
    try {
      await assertLockIdentity(slotLockFileSystem, directory, directoryIdentity, lockPath, handle);
      await assertLegacyMarker(slotLockFileSystem, legacyPath, handle);
    } catch (error) {
      throw diagnostic("Development slot assignment may already exist; reload to validate it", error);
    }
    return result;
  } finally {
    if (handle !== undefined) {
      try { await handle.close(); }
      catch (error) { throw diagnostic(`Cannot close development slot allocation lock '${lockPath}'; assignment may already exist; reload to validate it`, error); }
    }
  }
}

async function assertLockIdentity(fileSystem, directory, expectedDirectory, lockPath, handle) {
  const [parent, descriptor, pathname] = await Promise.all([
    fileSystem.lstat(directory, { bigint: true }),
    handle.stat({ bigint: true }),
    fileSystem.lstat(lockPath, { bigint: true }),
  ]);
  if (!parent.isDirectory() || parent.isSymbolicLink() || !sameFile(parent, expectedDirectory) ||
      !descriptor.isFile() || !pathname.isFile() || !sameFile(descriptor, pathname)) {
    throw new Error(`Development slot lock path '${lockPath}' changed identity; no new assignment is safe.`);
  }
}

async function ensureLegacyMarker(fileSystem, lockPath, legacyPath, handle) {
  // The old pathname shares the held inode, so older exclusive-create allocators cannot enter.
  try {
    await fileSystem.link(lockPath, legacyPath);
  } catch (error) {
    if (error.code !== "EEXIST") throw diagnostic(`Cannot install legacy slot lock marker '${legacyPath}'`, error);
  }
  await assertLegacyMarker(fileSystem, legacyPath, handle);
}

async function assertLegacyMarker(fileSystem, legacyPath, handle) {
  let marker;
  try { marker = await fileSystem.lstat(legacyPath, { bigint: true }); }
  catch (error) { throw diagnostic(`Legacy slot lock marker '${legacyPath}' is missing or inaccessible`, error); }
  if (!marker.isFile() || !sameFile(marker, await handle.stat({ bigint: true }))) {
    throw new Error(`Legacy slot lock '${legacyPath}' belongs to an older allocator or has changed; leave it untouched and retry only after quiescent maintenance.`);
  }
}

function sameFile(first, second) {
  return first.dev === second.dev && first.ino === second.ino;
}

/** Returns a deterministic filesystem-safe namespace for one development workspace root. */
export function workspaceNamespace(workspaceRoot = process.cwd()) {
  return createHash("sha256")
    .update(path.resolve(workspaceRoot))
    .digest("hex")
    .slice(0, 16);
}

/** Returns the Process Compose Unix control socket for one workspace slot. */
export function processComposeSocketPath(workspaceRoot = process.cwd(), environment = process.env) {
  const index = environment.DOTCORE_DEV_INDEX;
  if (typeof index !== "string" || !TWO_DIGIT_INDEX.test(index)) {
    throw new Error("Process Compose socket identity requires DOTCORE_DEV_INDEX to be a two-digit workspace slot.");
  }
  return path.join(PROCESS_COMPOSE_SOCKET_DIRECTORY, `dotcore-pc-${workspaceNamespace(workspaceRoot)}-${index}.sock`);
}
export function assertEnvironmentMatchesSchema(environment) {
  const producedVariables = Object.keys(environment);
  const missingVariables = ENVIRONMENT_VARIABLES.filter(
    (variable) => !Object.hasOwn(environment, variable),
  );
  const undeclaredVariables = producedVariables.filter(
    (variable) => !Object.hasOwn(ENVIRONMENT_VARIABLE_SCHEMA, variable),
  );
  if (missingVariables.length > 0 || undeclaredVariables.length > 0) {
    const details = [
      missingVariables.length > 0 && `missing ${missingVariables.join(", ")}`,
      undeclaredVariables.length > 0 && `undeclared ${undeclaredVariables.join(", ")}`,
    ]
      .filter(Boolean)
      .join("; ");
    throw new Error(`Derived development environment does not match the helper schema: ${details}.`);
  }
  return environment;
}

export function deriveEnvironment({
  cacheRoot,
  backendRunMode,
  frontendRunMode,
  siteConfig,
  masterCatalog,
  host,
  index,
  justTempdir,
  rustLibBacktrace,
  workspaceRoot = path.dirname(masterCatalog),
}) {
  const validatedHost = validateHost(host);
  const validatedBackendRunMode = validateRunMode(
    backendRunMode,
    "DOTCORE_BACKEND_RUN_MODE",
    BACKEND_RUN_MODES,
  );
  const validatedFrontendRunMode = validateRunMode(
    frontendRunMode,
    "DOTCORE_FRONTEND_RUN_MODE",
    FRONTEND_RUN_MODES,
  );
  for (const [value, variable] of [
    [cacheRoot, "DOTCORE_CACHE_DIR"],
    [siteConfig, "DOTCORE_SITE_DIR"],
    [masterCatalog, "DOTCORE_CATALOG_DIR"],
  ]) {
    if (typeof value !== "string" || !path.isAbsolute(value)) {
      throw new Error(`${variable} must be an absolute path.`);
    }
    if (variable === "DOTCORE_CACHE_DIR" && path.parse(path.normalize(value)).root === path.normalize(value)) {
      throw new Error("DOTCORE_CACHE_DIR cannot be the filesystem root.");
    }
  }
  if (typeof index !== "string" || !TWO_DIGIT_INDEX.test(index)) {
    throw new Error("DOTCORE_DEV_INDEX must be a two-digit value from 00 to 99.");
  }
  if (typeof justTempdir !== "string" || !path.isAbsolute(justTempdir)) {
    throw new Error("JUST_TEMPDIR must be an absolute path to a writable-intent directory.");
  }
  const numericIndex = Number(index);
  // Each slot owns ten consecutive ports; the role offset is the same in every slot, including 00.
  const slotBase = SLOT_PORT_BASE + numericIndex * SLOT_PORT_STRIDE;
  const frontendPort = slotBase;
  const managementPort = slotBase + 1;
  const inferencePort = slotBase + 2;
  const privatePort = slotBase + 3;
  const workerPrivatePort = slotBase + 4;
  const publicPort = slotBase + 5;
  // A bare dotcore inherits the slot's other listeners, so only the main workspace keeps the bundle default.
  const bundlePublicPort = numericIndex === 0 ? BUNDLE_PUBLIC_PORT : publicPort;
  const frontendOrigin = `http://${validatedHost}:${frontendPort}`;
  const managementOrigin = `http://${validatedHost}:${managementPort}`;
  const inferenceOrigin = `http://${validatedHost}:${inferencePort}`;

  return assertEnvironmentMatchesSchema({
    DOTCORE_MACHINE_HOST: validatedHost,
    DOTCORE_DEV_INDEX: index,
    DOTCORE_SITE_DIR: path.normalize(siteConfig),
    DOTCORE_CATALOG_DIR: path.normalize(masterCatalog),
    DOTCORE_CACHE_DIR: path.normalize(cacheRoot),
    DOTCORE_BACKEND_RUN_MODE: validatedBackendRunMode,
    DOTCORE_FRONTEND_RUN_MODE: validatedFrontendRunMode,
    HF_XET_CACHE: path.join(path.normalize(cacheRoot), "xet"),
    DOTCORE_FRONTEND_PORT: String(frontendPort),
    DOTCORE_MANAGEMENT_PORT: String(managementPort),
    DOTCORE_INFERENCE_PORT: String(inferencePort),
    PC_SOCKET_PATH: processComposeSocketPath(workspaceRoot, { DOTCORE_DEV_INDEX: index }),
    DOTCORE_FRONTEND_ORIGIN: frontendOrigin,
    DOTCORE_MANAGEMENT_ADDR: `${validatedHost}:${managementPort}`,
    DOTCORE_MANAGEMENT_ORIGIN: managementOrigin,
    DOTCORE_INFERENCE_ADDR: `${validatedHost}:${inferencePort}`,
    DOTCORE_INFERENCE_ORIGIN: inferenceOrigin,
    DOTCORE_PRIVATE_PORT: String(privatePort),
    DOTCORE_PRIVATE_ADDR: `${validatedHost}:${privatePort}`,
    DOTCORE_WORKER_PRIVATE_PORT: String(workerPrivatePort),
    DOTCORE_DEV_PUBLIC_PORT: String(publicPort),
    DOTCORE_PUBLIC_PORT: String(bundlePublicPort),
    DOTCORE_PUBLIC_ORIGIN: `http://${validatedHost}:${publicPort}`,
    DOTCORE_WORKER_DIR: path.join(workspaceRoot, ".dotcore", "two-node-worker"),
    DOTCORE_DEV_SOURCE_FLAKE: `git+${pathToFileURL(path.normalize(workspaceRoot)).href}`,
    VITE_DOTCORE_MANAGEMENT_ORIGIN: SAME_ORIGIN,
    VITE_DOTCORE_INFERENCE_ORIGIN: SAME_ORIGIN,
    JUST_TEMPDIR: justTempdir,
    RUST_LIB_BACKTRACE: rustLibBacktrace,
  });
}

function defaultCacheRoot(xdgCacheHome, home) {
  const cacheHome = xdgCacheHome || (typeof home === "string" ? path.join(home, ".cache") : undefined);
  if (typeof cacheHome !== "string" || !path.isAbsolute(cacheHome)) {
    throw new Error("XDG_CACHE_HOME or HOME must provide an absolute path for DOTCORE_CACHE_DIR.");
  }
  return path.join(cacheHome, "dotcore");
}

function selectKnownValue(local, inherited, base, variable) {
  if (Object.hasOwn(local, variable) && local[variable] !== undefined) {
    return local[variable];
  }
  if (Object.hasOwn(inherited, variable) && inherited[variable] !== undefined) {
    return inherited[variable];
  }
  return base[variable];
}

function selectInputValues(local, inherited, base) {
  const deriveInputs = new Set();
  return Object.fromEntries(
    [...LOCAL_INPUT_VARIABLES].map((variable) => {
      const metadata = ENVIRONMENT_VARIABLE_SCHEMA[variable];
      const { deriveInput, kind } = metadata;
      if (typeof deriveInput !== "string" || deriveInput === "") {
        throw new Error(`'${variable}' is missing a helper derivation input.`);
      }
      if (deriveInputs.has(deriveInput)) {
        throw new Error(`'${variable}' duplicates helper derivation input '${deriveInput}'.`);
      }
      deriveInputs.add(deriveInput);
      switch (kind) {
        case "local-only":
          return [deriveInput, local[variable]];
        case "base-supported":
          return [deriveInput, selectKnownValue(local, inherited, base, variable)];
        default:
          throw new Error(`'${variable}' has an invalid helper input kind.`);
      }
    }),
  );
}

function localPassthroughValues(local) {
  return Object.fromEntries(
    Object.entries(local).filter(([variable]) => !LOCAL_INPUT_VARIABLES.has(variable)),
  );
}

export async function initializeEnvironment(options = {}) {
  const { cwd = process.cwd(), env = process.env, random = Math.random } = options;
  const discovery = await discoverWorkspaceRoots(options);
  const { localValues } = await readDiscoveredAssignments(discovery);
  const currentRoot = path.resolve(cwd);
  const base = await readDotenv(path.join(currentRoot, ".env.base"), {
    allowedKeys: BASE_INPUT_VARIABLES,
  });
  let currentLocal = localValues.get(currentRoot);
  let createdLocalFile = false;

  if (currentLocal === undefined) {
    await withSlotAllocationLock(discovery.mainRoot, async (lockedMainRoot) => {
      const refreshedDiscovery = await discoverWorkspaceRoots(options);
      if (await realpath(refreshedDiscovery.mainRoot) !== lockedMainRoot) {
        throw new Error("Main development repository root changed during slot allocation; no assignment was written.");
      }
      const { assignments, localValues: recheckedLocalValues } = await readDiscoveredAssignments(refreshedDiscovery);
      currentLocal = recheckedLocalValues.get(currentRoot);
      if (currentLocal !== undefined) return;
      const index = currentRoot === refreshedDiscovery.mainRoot ? 0 : chooseUnusedIndex(assignments, random);
      const localPath = path.join(currentRoot, ".env.local");
      await createLocalFile(localPath, index, options);
      currentLocal = { DOTCORE_DEV_INDEX: String(index).padStart(2, "0") };
      createdLocalFile = true;
    }, options);
  }

  const localSite = path.join(currentRoot, LOCAL_SITE_PATH);
  const inputs = selectInputValues(currentLocal, env, base);
  const environment = deriveEnvironment({
    ...inputs,
    cacheRoot:
      inputs.cacheRoot ?? defaultCacheRoot(env.XDG_CACHE_HOME, env.HOME),
    siteConfig: localSite,
    masterCatalog: path.join(currentRoot, "catalog"),
    workspaceRoot: currentRoot,
    rustLibBacktrace: inputs.rustLibBacktrace,
  });
  const completeEnvironment = { ...environment, ...localPassthroughValues(currentLocal) };
  const artifact = await publishEnvironmentArtifact(currentRoot, completeEnvironment, options);
  return { ...discovery, ...artifact, createdLocalFile, environment: completeEnvironment };
}

/**
 * Warns on macOS when the second loopback address used by the two-process backend tests is not
 * bound. Never throws: the tests skip without the alias, so activation must still succeed.
 */
export async function warnIfWorkerLoopbackMissing({
  platform = process.platform,
  probe = probeLoopbackAddress,
  write = (message) => process.stderr.write(message),
} = {}) {
  if (platform !== "darwin") return false;

  const missing = await probe(WORKER_LOOPBACK_HOST).catch(() => false);
  if (missing) {
    write(
      `warning: ${WORKER_LOOPBACK_HOST} is not bound on this host; run ` +
        `\`just dev::mac-loopback\`, or the two-process backend tests skip and the node join CLI tests fail.\n`,
    );
  }
  return missing;
}

/** Resolves true only when binding the address fails with "address not available". */
function probeLoopbackAddress(host, timeoutMs = LOOPBACK_PROBE_TIMEOUT_MS) {
  return new Promise((resolve) => {
    const server = net.createServer();
    const timer = setTimeout(() => finish(false), timeoutMs);
    const finish = (missing) => {
      clearTimeout(timer);
      server.close();
      resolve(missing);
    };
    server.once("error", (error) => finish(error.code === "EADDRNOTAVAIL"));
    server.listen(0, host, () => finish(false));
  });
}

function defaultSleep(milliseconds) {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function main() {
  const { environment } = await initializeEnvironment();
  await warnIfWorkerLoopbackMissing();
  process.stdout.write(
    `Dotcore console: ${environment.DOTCORE_PUBLIC_ORIGIN}\nDotcore management: ${environment.DOTCORE_MANAGEMENT_ORIGIN}\n`,
  );
}

if (import.meta.main) {
  main().catch((error) => {
    process.stderr.write(`${error.message}\n`);
    process.exitCode = 1;
  });
}
