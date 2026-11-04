import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";

import { describe, expect, it } from "vitest";

describe("prepaint theme bootstrap", () => {
  it.each([
    ["missing", null],
    ["malformed", "not base64"],
    ["unspecified", ""],
    ["unknown", "CGM="],
  ])("applies Dark before React for %s preferences", (_description, storedValue) => {
    const element = createDocumentElement();
    runPrepaint(storedValue, element);

    expect(element.classList.has("dark")).toBe(true);
    expect(element.style.colorScheme).toBe("dark");
  });

  it("applies Dark before React when storage cannot be read", () => {
    const element = createDocumentElement();
    runPrepaint(null, element, () => {
      throw new Error("Storage disabled");
    });

    expect(element.classList.has("dark")).toBe(true);
    expect(element.style.colorScheme).toBe("dark");
  });

  it.each([
    ["Light", "CAE=", false, "light"],
    ["Dark", "CAI=", true, "dark"],
  ])("preserves explicit %s before React", (_description, storedValue, dark, colorScheme) => {
    const element = createDocumentElement();
    runPrepaint(storedValue, element);

    expect(element.classList.has("dark")).toBe(dark);
    expect(element.style.colorScheme).toBe(colorScheme);
  });

  it("uses a parser-blocking classic script instead of render-blocking module support", () => {
    const indexHtml = readFileSync("index.html", "utf8");
    const bootstrap = readFileSync("public/prepaint-theme.js", "utf8");

    expect(indexHtml).toContain('<script src="/prepaint-theme.js"></script>');
    expect(indexHtml).not.toContain('blocking="render"');
    expect(bootstrap).toContain("dotcore.console.preferences.v1");
  });
});

function createDocumentElement() {
  const classes = new Set<string>();

  return {
    classList: {
      has: (className: string) => classes.has(className),
      toggle: (className: string, force: boolean) => {
        if (force) {
          classes.add(className);
        } else {
          classes.delete(className);
        }
      },
    },
    style: { colorScheme: "" },
  };
}

function runPrepaint(
  storedValue: string | null,
  documentElement: ReturnType<typeof createDocumentElement>,
  getItem: () => string | null = () => storedValue,
) {
  const bootstrap = readFileSync("public/prepaint-theme.js", "utf8");

  runInNewContext(bootstrap, {
    Uint8Array,
    atob,
    document: { documentElement },
    localStorage: { getItem },
  });
}
