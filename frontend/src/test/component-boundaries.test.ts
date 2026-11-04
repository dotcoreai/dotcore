/**
 * Static guard against hand-rolled Button clones living outside `src/components/ui/`.
 *
 * jsdom ignores every declaration inside a Tailwind cascade layer (see `card.test.tsx`), so a
 * rendered-DOM assertion can never prove a bespoke element looks like a button. This scans source
 * text instead, the same way the Card consumer-collision guard scans compiled CSS text.
 */

import { readdirSync, readFileSync } from "node:fs";
import { extname, join, relative, resolve } from "node:path";

import { describe, expect, it } from "vitest";

const SRC_ROOT = resolve(process.cwd(), "src");
const PRIMITIVE_DIR = resolve(SRC_ROOT, "components/ui");

// Button's variant fill/foreground pairs (button.tsx). A className outside components/ui/ is
// "button-shaped" only when it combines all three of a flex-centered shape, a rounded corner, and
// one of these pairs — the same triplet a consumer would write to visually clone Button by hand.
// Any one alone is common on non-button surfaces (an avatar chip, a centered icon), so none of
// them is a signal in isolation.
const BUTTON_COLOR_PAIRS: [fill: string, foreground: string][] = [
  ["bg-primary", "text-primary-foreground"],
  ["bg-secondary", "text-secondary-foreground"],
  ["border-destructive", "text-destructive"],
];

function isButtonShaped(className: string): boolean {
  const hasCenteredFlexShape =
    /\b(?:inline-flex|flex)\b/.test(className) &&
    /\bitems-center\b/.test(className) &&
    /\bjustify-center\b/.test(className);
  const hasRoundedCorner = /\brounded(?:-[\w[\]/.%-]+)?\b/.test(className);
  const hasButtonColorPair = BUTTON_COLOR_PAIRS.some(
    ([fill, foreground]) => className.includes(fill) && className.includes(foreground),
  );

  return hasCenteredFlexShape && hasRoundedCorner && hasButtonColorPair;
}

/** Every quoted string literal that looks button-shaped, as `relative/path: literal`. */
function findButtonShapedClassNames(sourceRoot: string, excludeDir: string): string[] {
  const findings: string[] = [];

  for (const file of listTsxFiles(sourceRoot)) {
    if (file.startsWith(excludeDir)) {
      continue;
    }

    const text = readFileSync(file, "utf8");
    for (const match of text.matchAll(/"([^"]*)"/g)) {
      const literal = match[1];
      if (isButtonShaped(literal)) {
        findings.push(`${relative(sourceRoot, file)}: ${literal}`);
      }
    }
  }

  return findings;
}

function listTsxFiles(dir: string): string[] {
  const files: string[] = [];

  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      files.push(...listTsxFiles(full));
    } else if (extname(entry.name) === ".tsx" && !entry.name.endsWith(".test.tsx")) {
      files.push(full);
    }
  }

  return files;
}

describe("component boundary guard", () => {
  it("finds no button-shaped className outside components/ui/", () => {
    expect(findButtonShapedClassNames(SRC_ROOT, PRIMITIVE_DIR)).toEqual([]);
  });

  it("detects a hand-rolled button clone outside components/ui/", () => {
    expect(
      isButtonShaped(
        "inline-flex items-center justify-center rounded-lg bg-primary text-primary-foreground px-2.5",
      ),
    ).toBe(true);
  });

  it("does not flag a centered, rounded, non-button-colored element", () => {
    // profile-menu.tsx composes a flex-centered, rounded chip outside components/ui/. It does not
    // pair a Button fill with its foreground, so the guard must stay silent on it.
    expect(
      isButtonShaped(
        "inline-flex h-7.5 w-7.5 items-center justify-center rounded-full bg-info text-primary",
      ),
    ).toBe(false);
  });
});
