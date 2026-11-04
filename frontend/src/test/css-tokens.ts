/**
 * Test-only helpers for asserting against the real `src/index.css` token layer.
 *
 * Reads the stylesheet source rather than a hand-copied duplicate, so token and contrast
 * assertions stay accurate as the palette changes. Shared by the checks that keep the token
 * invariants (the retained-`--console-*` set and the `@theme inline` resolution guarantee) in force.
 */

import { readFileSync } from "node:fs";
import { resolve } from "node:path";

// Resolved from the Vitest working directory (the frontend package root) rather than
// `import.meta.url`, which Vitest's module transform does not always expose as a `file:` URL.
const INDEX_CSS_PATH = resolve(process.cwd(), "src/index.css");

export function readIndexCss(): string {
  return readFileSync(INDEX_CSS_PATH, "utf8");
}

/** Extracts the body of a top-level `:root { ... }`, `.dark { ... }`, or `@theme inline { ... }` block. */
export function extractTopLevelBlock(
  css: string,
  selector: ":root" | ".dark" | "@theme inline",
): string {
  const escaped = selector.replace(/[.]/g, "\\.");
  const match = new RegExp(`(?:^|\\n)${escaped}\\s*\\{([^}]*)\\}`).exec(css);
  if (!match) {
    throw new Error(`No top-level ${selector} block found in index.css`);
  }
  return match[1];
}

/** Custom-property names declared (`--foo: ...;`) within a block body. */
export function extractDeclaredProperties(blockBody: string): Set<string> {
  return new Set([...blockBody.matchAll(/(--[\w-]+)\s*:/g)].map(([, name]) => name));
}

/** Custom-property names referenced via `var(--foo` anywhere in the given CSS text. */
export function extractReferencedProperties(css: string): Set<string> {
  return new Set([...css.matchAll(/var\((--[\w-]+)/g)].map(([, name]) => name));
}

/** The declared value of a custom property within a block body, e.g. `oklch(0.5 0 0)`. */
export function extractPropertyValue(blockBody: string, name: string): string {
  const match = new RegExp(`${name}\\s*:\\s*([^;]+);`).exec(blockBody);
  if (!match) {
    throw new Error(`Property ${name} not declared in block`);
  }
  return match[1].trim();
}
