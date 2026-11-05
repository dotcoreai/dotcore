//! Verifies `just backend::publish` dry-runs, confirms, skips published versions and waits out the
//! crates.io rate limit.
//!
//! Each test runs the real backend `Justfile` from a temporary fake repository, with a fake `cargo`
//! that publishes into a fake index and a fake `curl` that serves it, so nothing reaches crates.io.

import assert from "node:assert/strict";
import { chmod, copyFile, mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
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

/** The workspace crates the fake `cargo` reports, in its publish order. */
const crates = ["dotcore-domain", "dotcore-proto", "dotc"];

async function fixture(t) {
  const root = await mkdtemp(path.join(tmpdir(), "dotcore-publish-recipe-"));
  t.after(() => rm(root, { force: true, recursive: true }));
  await mkdir(path.join(root, "backend"));
  await mkdir(path.join(root, "dev"));
  await copyFile(path.join(repositoryRoot, "backend", "Justfile"), path.join(root, "backend", "Justfile"));
  await copyFile(path.join(repositoryRoot, "dev", "confirm.sh"), path.join(root, "dev", "confirm.sh"));

  const bin = path.join(root, "bin");
  const index = path.join(root, "index");
  const calls = path.join(root, "calls");
  const quota = path.join(root, "quota");
  await mkdir(bin);
  await mkdir(index);
  await writeFile(calls, "");
  await writeFile(quota, "");

  // The fake cargo records each publish. A real one uploads the crates not excluded, in order, as many
  // as the next line of the quota file allows, then answers like crates.io's rate limit.
  await writeFile(path.join(bin, "cargo"), `#!/usr/bin/env bash
set -eu
if [ "$1" = metadata ]; then
  echo '{"packages":[${crates.map((name) => `{"name":"${name}","version":"0.1.0"}`).join(",")}]}'
  exit 0
fi
printf 'cargo %s\\n' "$*" >> "${calls}"
case " $* " in *" --dry-run "*) exit 0 ;; esac
if [ -n "\${FAKE_PUBLISH_ERROR:-}" ]; then echo "error: $FAKE_PUBLISH_ERROR" >&2; exit 101; fi
allowed=$(head -n 1 "${quota}")
sed -i.bak 1d "${quota}"
for name in ${crates.join(" ")}; do
  case " $* " in *" --exclude $name "*) continue ;; esac
  if [ "$allowed" -eq 0 ]; then
    echo "error: failed to publish $name v0.1.0 to registry at https://crates.io" >&2
    echo "  the remote server responded with an error (status 429 Too Many Requests): You have published too many new crates in a short period of time." >&2
    exit 101
  fi
  echo '{"name":"'"$name"'","vers":"0.1.0"}' >> "${index}/$name"
  allowed=$((allowed - 1))
done
`);
  // The fake curl serves the fake index at the sparse index path of each crate name.
  await writeFile(path.join(bin, "curl"), `#!/usr/bin/env bash
set -eu
while [ $# -gt 1 ]; do
  case "$1" in -o) out="$2"; shift ;; esac
  shift
done
printf 'curl %s\\n' "$1" >> "${calls}"
name="\${1##*/}"
if [ -f "${index}/$name" ]; then cp "${index}/$name" "$out"; printf 200; else printf 404; fi
`);
  await chmod(path.join(bin, "cargo"), 0o755);
  await chmod(path.join(bin, "curl"), 0o755);

  const environment = { ...process.env, PATH: `${bin}:${process.env.PATH}`, DOTCORE_PUBLISH_RETRY_SECONDS: "0" };
  delete environment.DOTCORE_YES;
  const justArguments = (arguments_) => ["--justfile", path.join(root, "backend", "Justfile"), "publish", ...arguments_];
  const run = (arguments_, extraEnvironment = {}) => spawnSync("just", justArguments(arguments_), {
    encoding: "utf8",
    env: { ...environment, ...extraEnvironment },
  });
  const runInTerminal = (answer, arguments_ = []) => spawnSync("script", ["-qec", ["just", ...justArguments(arguments_)].map((word) => `'${word.replace(/'/g, "'\\''")}'`).join(" "), "/dev/null"], {
    encoding: "utf8",
    env: environment,
    input: answer,
  });
  const publishes = async () => (await readFile(calls, "utf8")).split("\n").filter((line) => line.startsWith("cargo "));
  const indexLookups = async () => (await readFile(calls, "utf8")).split("\n").filter((line) => line.startsWith("curl "));
  const published = async () => (await readdir(index)).sort();
  const setQuota = (...allowances) => writeFile(quota, allowances.map((allowance) => `${allowance}\n`).join(""));
  const setPublished = (name, version) => writeFile(path.join(index, name), `{"name":"${name}","vers":"${version}"}\n`);
  return { indexLookups, published, publishes, run, runInTerminal, setPublished, setQuota };
}

test("publish dry-runs every crate, then waits out the rate limit and publishes the rest", async (t) => {
  const { indexLookups, published, publishes, run, setQuota } = await fixture(t);
  await setQuota(1, 5);

  const result = run(["--allow-dirty"], { DOTCORE_YES: "1" });

  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(await publishes(), [
    "cargo publish --locked --workspace --dry-run --allow-dirty",
    "cargo publish --locked --workspace --no-verify --allow-dirty",
    "cargo publish --locked --workspace --exclude dotcore-domain --no-verify --allow-dirty",
  ]);
  assert.match(result.stderr, /crates\.io rate limit reached; retrying the remaining crates in 0s/);
  assert.deepEqual(await published(), ["dotc", "dotcore-domain", "dotcore-proto"]);
  assert.deepEqual((await indexLookups()).slice(0, 3), [
    "curl https://index.crates.io/do/tc/dotc",
    "curl https://index.crates.io/do/tc/dotcore-domain",
    "curl https://index.crates.io/do/tc/dotcore-proto",
  ]);
});

test("publish skips versions already on crates.io and uploads nothing when declined", needsTerminal, async (t) => {
  const { published, publishes, runInTerminal, setPublished } = await fixture(t);
  await setPublished("dotcore-domain", "0.1.0");
  await setPublished("dotcore-proto", "0.0.9");

  const result = runInTerminal("n\n");

  assert.equal(result.status, 130, result.stdout);
  assert.deepEqual(await publishes(), ["cargo publish --locked --workspace --exclude dotcore-domain --dry-run"]);
  assert.match(result.stdout, /crates to publish:\r?\n {2}dotc 0\.1\.0\r?\n {2}dotcore-proto 0\.1\.0\r?\nalready published, skipped:\r?\n {2}dotcore-domain 0\.1\.0/);
  assert.match(result.stdout, /publishing cancelled; nothing was changed/);
  assert.deepEqual(await published(), ["dotcore-domain", "dotcore-proto"]);
});

test("publish stops on a failure other than the rate limit", async (t) => {
  const { published, publishes, run } = await fixture(t);

  const result = run([], { DOTCORE_YES: "1", FAKE_PUBLISH_ERROR: "no token found, please run `cargo login`" });

  assert.equal(result.status, 1, result.stderr);
  assert.deepEqual(await publishes(), [
    "cargo publish --locked --workspace --dry-run",
    "cargo publish --locked --workspace --no-verify",
  ]);
  assert.doesNotMatch(result.stderr, /rate limit/);
  assert.deepEqual(await published(), []);
});

test("publish does nothing once every crate version is published", async (t) => {
  const { publishes, run, setPublished } = await fixture(t);
  for (const name of crates) {
    await setPublished(name, "0.1.0");
  }

  const result = run([]);

  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /every backend crate version is already on crates\.io; nothing to publish/);
  assert.deepEqual(await publishes(), []);
});
