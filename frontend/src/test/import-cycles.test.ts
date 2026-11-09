/**
 * Static guard against import cycles inside `src/`.
 *
 * Builds one import graph over every source file (type-only imports, dynamic `import()` and
 * `vi.mock` paths included, since they are structural) and checks it twice: between files, and
 * between `features/<name>` folders, where features mostly loop through different files. Known
 * loops are allowlisted by folder; an allowlist entry with no loop left fails the test, so the
 * lists only shrink. The layer order is checked by lint.
 */

import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";

import { describe, expect, it } from "vitest";

const SRC_ROOT = resolve(process.cwd(), "src");
const GEN_ROOT = join(SRC_ROOT, "gen");

// An import statement, a re-export, a dynamic `import()` or a `vi.mock` call; the path is the
// first matching capture group.
const IMPORT_PATTERN =
  /(?:import|export)\s+(?:type\s+)?(?:[^'";]*?\s+from\s+)?["']([^"']+)["']|import\(\s*["']([^"']+)["']\s*\)|vi\.mock\(\s*["']([^"']+)["']/g;

const RESOLVE_SUFFIXES = ["", ".ts", ".tsx", "/index.ts", "/index.tsx"];

// Folders where files may import each other in a loop: the loop is kept inside one folder.
const KEPT_FILE_LOOP_FOLDERS = ["app", "features/models", "features/preferences"];

// Features that may loop among themselves until the named phase removes the loops.
const ALLOWED_FEATURE_LOOPS = [
  {
    removedBy: "phase 05 shared domain",
    features: ["applications", "inference", "machines", "memory", "models", "overview", "routes"],
  },
];

describe("import cycles", () => {
  const fileGraph = buildFileGraph();

  it("finds no unlisted import cycle between files", () => {
    const allowed = KEPT_FILE_LOOP_FOLDERS.map((folder) => [folder]);

    expect(checkLoops(fileGraph, allowed)).toEqual([]);
  });

  it("finds no unlisted import cycle between features", () => {
    const { graph, imports } = buildFeatureGraph(fileGraph);
    const allowed = ALLOWED_FEATURE_LOOPS.map((loop) => loop.features);

    expect(checkLoops(graph, allowed, (edge) => imports.get(edge)!)).toEqual([]);
  });
});

type Graph = Map<string, string[]>;

/**
 * Problems found in a graph: one line per loop outside every allowed group, printed as a node
 * chain (with `describeEdge` lines under it), and one per allowed group that holds no loop.
 * A group lists node names or folders; a loop is allowed when all its nodes are in one group, and
 * a group is stale when no loop touches it.
 */
function checkLoops(
  graph: Graph,
  allowed: string[][],
  describeEdge: (edge: string) => string = () => "",
): string[] {
  const inGroup = (group: string[], node: string) =>
    group.some((entry) => node === entry || node.startsWith(`${entry}/`));
  const loops = stronglyConnectedComponents(graph);
  const problems: string[] = [];

  for (const loop of loops.filter((l) => !allowed.some((g) => l.every((n) => inGroup(g, n))))) {
    // Start from a node outside the allowed groups, so the chain shows what is new.
    const start = loop.find((n) => !allowed.some((g) => inGroup(g, n))) ?? loop[0];
    const chain = shortestCycle(graph, loop, start);
    const hops = chain.slice(1).map((node, i) => describeEdge(`${chain[i]} -> ${node}`));
    problems.push(`import cycle: ${chain.join(" -> ")}\n${hops.join("")}`);
  }

  for (const group of allowed) {
    if (!loops.some((loop) => loop.some((node) => inGroup(group, node)))) {
      problems.push(`${group.join(", ")}: no longer a cycle, remove from allowlist`);
    }
  }

  return problems;
}

/** Maps every file under `src/` (except `gen/`) to the files it imports, as `src`-relative paths. */
function buildFileGraph(): Graph {
  const graph: Graph = new Map();

  for (const file of listSourceFiles(SRC_ROOT)) {
    const imports = new Set<string>();

    for (const match of readFileSync(file, "utf8").matchAll(IMPORT_PATTERN)) {
      const target = resolveImport(file, match[1] ?? match[2] ?? match[3]);
      if (target) {
        imports.add(relative(SRC_ROOT, target));
      }
    }

    graph.set(relative(SRC_ROOT, file), [...imports]);
  }

  return graph;
}

/**
 * Collapses the file graph to feature folders (`features/<name>/...`, nested folders included).
 * `imports` keeps one file import per feature edge, to show where a loop comes from.
 */
function buildFeatureGraph(fileGraph: Graph): { graph: Graph; imports: Map<string, string> } {
  const graph: Graph = new Map();
  const imports = new Map<string, string>();

  for (const [file, targets] of fileGraph) {
    const from = featureOf(file);
    if (!from) {
      continue;
    }

    for (const target of targets) {
      const to = featureOf(target);
      const edge = `${from} -> ${to}`;
      if (!to || to === from || imports.has(edge)) {
        continue;
      }

      imports.set(edge, `  ${edge}: ${file} -> ${target}\n`);
      graph.set(from, [...(graph.get(from) ?? []), to]);
    }
  }

  return { graph, imports };
}

function featureOf(file: string): string | undefined {
  const [folder, name] = file.split("/");

  return folder === "features" ? name : undefined;
}

/** Tarjan's algorithm; returns only components that hold a loop (two or more nodes). */
function stronglyConnectedComponents(graph: Graph): string[][] {
  const index = new Map<string, number>();
  const lowLink = new Map<string, number>();
  const stack: string[] = [];
  const components: string[][] = [];

  const visit = (node: string) => {
    index.set(node, index.size);
    lowLink.set(node, index.get(node)!);
    stack.push(node);

    for (const next of graph.get(node) ?? []) {
      if (!index.has(next)) {
        visit(next);
        lowLink.set(node, Math.min(lowLink.get(node)!, lowLink.get(next)!));
      } else if (stack.includes(next)) {
        lowLink.set(node, Math.min(lowLink.get(node)!, index.get(next)!));
      }
    }

    if (lowLink.get(node) === index.get(node)) {
      const members = stack.splice(stack.indexOf(node));
      if (members.length > 1) {
        components.push(members);
      }
    }
  };

  for (const node of graph.keys()) {
    if (!index.has(node)) {
      visit(node);
    }
  }

  return components;
}

/** The shortest import chain that leaves `start`, a node of the loop, and comes back to it. */
function shortestCycle(graph: Graph, loop: string[], start: string): string[] {
  const chains = (graph.get(start) ?? [])
    .filter((next) => loop.includes(next))
    .map((next) => [start, ...shortestPath(graph, next, start)]);

  return chains.sort((a, b) => a.length - b.length)[0];
}

/** Nodes from `from` to `to` on a shortest path (breadth-first search). */
function shortestPath(graph: Graph, from: string, to: string): string[] {
  const parents = new Map<string, string>([[from, from]]);
  const queue = [from];

  for (const node of queue) {
    for (const next of graph.get(node) ?? []) {
      if (!parents.has(next)) {
        parents.set(next, node);
        queue.push(next);
      }
    }
  }

  const path = [to];
  while (path[0] !== from) {
    path.unshift(parents.get(path[0])!);
  }

  return path;
}

function resolveImport(importer: string, specifier: string): string | undefined {
  let base: string;
  if (specifier.startsWith("@/")) {
    base = join(SRC_ROOT, specifier.slice(2));
  } else if (specifier.startsWith(".")) {
    base = resolve(dirname(importer), specifier);
  } else {
    return undefined;
  }

  const target = RESOLVE_SUFFIXES.map((suffix) => base + suffix).find(
    (candidate) => existsSync(candidate) && statSync(candidate).isFile(),
  );

  return target?.startsWith(GEN_ROOT) ? undefined : target;
}

function listSourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      return full === GEN_ROOT ? [] : listSourceFiles(full);
    }

    return /\.tsx?$/.test(entry.name) ? [full] : [];
  });
}
