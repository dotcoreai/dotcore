import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { compile } from "@tailwindcss/node";
import { describe, expect, it } from "vitest";

import { contrastRatio } from "@/lib/color-contrast";
import { DENSITY_SIZE_NAMES } from "@/lib/utils";
import {
  extractDeclaredProperties,
  extractPropertyValue,
  extractReferencedProperties,
  extractTopLevelBlock,
  readIndexCss,
} from "@/test/css-tokens";

// The frontend package root, so @tailwindcss/node can resolve index.css's node_modules imports
// (shadcn/tailwind.css, tw-animate-css). Vitest's working directory is already the package root.
const FRONTEND_ROOT = process.cwd();

// Scoped `--console-*` custom properties with no shadcn-owned equivalent: the demo's `--ink-3`,
// used for de-emphasized card meta text, and the density scale, which shadcn has no tokens for.
// Every other console token maps onto its shadcn owner. A scoped token is allowed when it is
// commented and mapped.
const DENSITY_TOKENS = DENSITY_SIZE_NAMES.map((name) => `--console-${name}`);
const RETAINED_CONSOLE_TOKENS = new Set(["--console-subtle", ...DENSITY_TOKENS]);

// Density sizes that must reach the 44px touch floor on touch-primary devices. The compact steps
// (xs, sm) grow modestly instead.
const TOUCH_FLOOR_SIZE_NAMES = [
  "control-md",
  "control-action",
  "control-lg",
  "menu-row",
  "menu-row-sm",
];
const TOUCH_FLOOR_PX = 44;

describe("design token retained-scope invariant", () => {
  it("declares no --console- custom property beyond the retained set", () => {
    const css = readIndexCss();
    const declared = new Set(
      [
        ...extractDeclaredProperties(extractTopLevelBlock(css, ":root")),
        ...extractDeclaredProperties(extractTopLevelBlock(css, ".dark")),
      ].filter((name) => name.startsWith("--console-")),
    );

    expect(declared).toEqual(RETAINED_CONSOLE_TOKENS);
  });

  it("references no --console- custom property beyond the retained set anywhere in the stylesheet", () => {
    const css = readIndexCss();
    const referenced = new Set(
      [...extractReferencedProperties(css)].filter((name) => name.startsWith("--console-")),
    );

    expect(referenced).toEqual(RETAINED_CONSOLE_TOKENS);
  });
});

describe("shared modal and popup layer contract", () => {
  it("isolates the application root and declares ordered modal and popup tokens", () => {
    const css = readIndexCss();
    const root = extractTopLevelBlock(css, ":root");

    expect(css).toMatch(/#root\s*\{\s*isolation:\s*isolate;/);
    expect(extractPropertyValue(root, "--layer-modal")).toBe("60");
    expect(extractPropertyValue(root, "--layer-popup")).toBe("70");
    expect(Number(extractPropertyValue(root, "--layer-popup"))).toBeGreaterThan(
      Number(extractPropertyValue(root, "--layer-modal")),
    );
    expect(extractTopLevelBlock(css, "@theme inline")).toContain(
      "--z-index-modal: var(--layer-modal);",
    );
    expect(extractTopLevelBlock(css, "@theme inline")).toContain(
      "--z-index-popup: var(--layer-popup);",
    );
  });

  it("maps each body-portal primitive to its shared layer without literal z-indexes", () => {
    const sources = {
      dialog: readUiPrimitive("dialog.tsx"),
      sheet: readUiPrimitive("sheet.tsx"),
      combobox: readUiPrimitive("combobox.tsx"),
      dropdownMenu: readUiPrimitive("dropdown-menu.tsx"),
      tooltip: readUiPrimitive("tooltip.tsx"),
    };

    expect(sources.dialog).toContain('"fixed inset-0 z-modal');
    expect(sources.dialog).toContain('"fixed z-modal grid');
    expect(sources.sheet).toContain('"fixed inset-0 z-modal');
    expect(sources.sheet).toContain('"fixed z-modal flex');
    expect(sources.combobox).toContain('className="z-popup"');
    expect(sources.dropdownMenu).toContain('className="z-popup outline-none"');
    expect(sources.tooltip).toContain('className="z-popup"');

    for (const source of Object.values(sources)) {
      expect(source).not.toMatch(/(?:^|\s)z-(?:50|\[60\]|\[70\])(?=\s|")/);
    }
  });
});

describe("design token @theme inline mapping coverage", () => {
  it("maps every :root token in @theme inline", () => {
    expect(unmappedRootTokens(readIndexCss())).toEqual([]);
  });

  it("detects a :root token with no @theme inline mapping", () => {
    const cssWithOrphan = readIndexCss().replace(/:root\s*\{/, ":root {\n  --console-orphan: red;");

    expect(unmappedRootTokens(cssWithOrphan)).toEqual(["--console-orphan"]);
  });
});

describe("design token @theme inline resolution", () => {
  const NEW_STATUS_UTILITIES = [
    "bg-success",
    "text-success-foreground",
    "bg-warning",
    "text-warning-foreground",
    "bg-info",
    "bg-danger",
  ];

  it("compiles every new status-scale utility class to a real declaration, not a no-op", async () => {
    const compiler = await compile(readIndexCss(), { base: FRONTEND_ROOT, onDependency: () => {} });
    const output = compiler.build([...NEW_STATUS_UTILITIES, "bg-not-a-real-token-guard"]);

    for (const utility of NEW_STATUS_UTILITIES) {
      const rule = new RegExp(`\\.${utility.replace(/[/-]/g, "\\$&")}\\s*\\{[^}]*\\}`);
      expect(output, `expected ${utility} to compile to a rule`).toMatch(rule);
    }
    expect(output).not.toContain("bg-not-a-real-token-guard");
  });
});

describe("density tokens", () => {
  const css = readIndexCss();
  const root = extractTopLevelBlock(css, ":root");
  const theme = extractTopLevelBlock(css, "@theme inline");
  const coarse = extractCoarsePointerRootBlock(css);

  it("keeps the tailwind-merge size names equal to the --spacing-* density mappings", () => {
    const mapped = [...theme.matchAll(/--spacing-([\w-]+)\s*:\s*var\(--console-/g)].map(
      ([, name]) => name,
    );

    expect([...mapped].sort()).toEqual([...DENSITY_SIZE_NAMES].sort());
  });

  it.each(DENSITY_SIZE_NAMES)("declares, maps, and overrides %s for touch", (name) => {
    const token = `--console-${name}`;
    const mouse = lengthPx(extractPropertyValue(root, token));
    const touch = lengthPx(extractPropertyValue(coarse, token));

    expect(theme).toContain(`--spacing-${name}: var(${token});`);
    expect(touch).toBeGreaterThanOrEqual(mouse);
  });

  it.each(TOUCH_FLOOR_SIZE_NAMES)(
    "sizes %s to the touch floor on touch-primary devices",
    (name) => {
      expect(lengthPx(extractPropertyValue(coarse, `--console-${name}`))).toBeGreaterThanOrEqual(
        TOUCH_FLOOR_PX,
      );
    },
  );

  it("compiles each density size to a height, min-height, and size utility on its token", async () => {
    const utilities = DENSITY_SIZE_NAMES.flatMap((name) => [
      `h-${name}`,
      `min-h-${name}`,
      `size-${name}`,
    ]);
    const output = await compileUtilities(utilities);

    for (const name of DENSITY_SIZE_NAMES) {
      expect(output).toMatch(
        new RegExp(`\\.h-${name}\\s*\\{\\s*height:\\s*var\\(--console-${name}\\);?\\s*\\}`),
      );
      expect(output).toMatch(
        new RegExp(`\\.min-h-${name}\\s*\\{\\s*min-height:\\s*var\\(--console-${name}\\);?\\s*\\}`),
      );
      expect(output).toMatch(new RegExp(`\\.size-${name}\\s*\\{[^}]*var\\(--console-${name}\\)`));
    }
  });

  it("compiles touch-target into the utilities layer behind an any-pointer coarse query", async () => {
    const output = await compileUtilities(["touch-target"]);
    const utilitiesLayer = /@layer utilities\s*\{([\s\S]*)\}\s*$/.exec(output)?.[1] ?? "";

    expect(utilitiesLayer).toMatch(
      /@media \(any-pointer:\s*coarse\)\s*\{\s*\.touch-target::after\s*\{/,
    );
    expect(utilitiesLayer).toContain("width: max(100%, 44px)");
    expect(utilitiesLayer).toContain("height: max(100%, 44px)");
    // The consumer positions the element, so no rule targets the element itself.
    expect(utilitiesLayer).not.toMatch(/\.touch-target\s*\{/);
  });
});

const CONTRAST_PAIRS: [string, string][] = [
  ["--foreground", "--background"],
  ["--foreground", "--card"],
  ["--muted-foreground", "--muted"],
  ["--primary-foreground", "--primary"],
  ["--success-foreground", "--success"],
  ["--warning-foreground", "--warning"],
  // Danger and info reuse --destructive/--primary as their foreground; verify that reuse still
  // clears the floor against the new background chip, not just against the page background.
  ["--destructive", "--danger"],
  ["--primary", "--info"],
  ["--accent-foreground", "--accent"],
  ["--popover-foreground", "--popover"],
  ["--sidebar-foreground", "--sidebar"],
  ["--sidebar-accent-foreground", "--sidebar-accent"],
];

describe.each([
  ["light", ":root"],
  ["dark", ".dark"],
] as const)("design token %s-mode contrast floor", (_mode, selector) => {
  const css = readIndexCss();
  const block = extractTopLevelBlock(css, selector);
  const value = (name: string) => extractPropertyValue(block, name);

  it.each(CONTRAST_PAIRS)("%s on %s is at least 4.5:1", (foreground, background) => {
    expect(contrastRatio(value(foreground), value(background))).toBeGreaterThanOrEqual(4.5);
  });
});

/** Names of :root custom properties with no `var(--name` reference inside @theme inline. */
function readUiPrimitive(fileName: string): string {
  return readFileSync(resolve(FRONTEND_ROOT, "src/components/ui", fileName), "utf8");
}

async function compileUtilities(candidates: string[]): Promise<string> {
  const compiler = await compile(readIndexCss(), { base: FRONTEND_ROOT, onDependency: () => {} });

  return compiler.build(candidates);
}

/** The `:root` body inside the `@media (pointer: coarse)` override block. */
function extractCoarsePointerRootBlock(css: string): string {
  const match = /@media \(pointer:\s*coarse\)\s*\{\s*:root\s*\{([^}]*)\}/.exec(css);
  if (!match) {
    throw new Error("No @media (pointer: coarse) :root block found in index.css");
  }

  return match[1];
}

/** A density token value in px: `--spacing(n)`, or the `calc(var(--spacing) * n)` it compiles to. */
function lengthPx(value: string): number {
  const match = /^(?:--spacing\(([\d.]+)\)|calc\(var\(--spacing\)\s*\*\s*([\d.]+)\))$/.exec(value);
  if (!match) {
    throw new Error(`Unsupported density value: ${value}`);
  }

  return Number(match[1] ?? match[2]) * 4;
}

function unmappedRootTokens(css: string): string[] {
  const declared = extractDeclaredProperties(extractTopLevelBlock(css, ":root"));
  const mapped = extractReferencedProperties(extractTopLevelBlock(css, "@theme inline"));

  return [...declared].filter((name) => !mapped.has(name));
}
