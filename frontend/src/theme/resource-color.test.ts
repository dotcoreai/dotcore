import { describe, expect, it } from "vitest";

import { contrastRatio, oklabDistance } from "./color-contrast";
import { assignResourceColors, getResourceColor } from "./resource-color";

const PALETTE_SIZE = 24;

describe("getResourceColor", () => {
  it("is deterministic: returns consistent color across multiple calls", () => {
    const ids = ["model-starter", "model-qwen", "model-deepseek", "model-phi", "model-gemma"];

    const firstPass = ids.map((id) => getResourceColor(id));
    const secondPass = ids.map((id) => getResourceColor(id));

    expect(firstPass).toEqual(secondPass);
  });

  it("keeps the established hash colors", () => {
    expect(getResourceColor("model-starter")).toBe("#63908c");
    expect(getResourceColor("model-llama")).toBe("#0593bf");
    expect(getResourceColor("model-qwen")).toBe("#0593bf");
    expect(getResourceColor("gemma-4-e2b")).toBe("#ca6c70");
  });

  it("distributes across color palette rather than collapsing to one entry", () => {
    const modelIds = [
      "model-starter",
      "model-llama",
      "model-qwen",
      "model-deepseek",
      "model-phi",
      "model-gemma",
      "model-bge",
      "model-mixtral",
      "model-retry",
      "model-loading",
      "model-failed",
      "model-alpha",
      "model-beta",
      "model-gamma",
      "model-delta",
      "model-epsilon",
      "model-zeta",
    ];

    const uniqueColors = new Set(modelIds.map((id) => getResourceColor(id)));

    // Exact value, not a fitted floor: verified independently against the published FNV-1a test
    // vector (hash("foobar") = 0xbf9cf968 = 3214735720), then applied to this ID set, rather than
    // read off whatever the implementation happened to emit. A regression to a lower-quality hash
    // (e.g. a float64-precision-losing multiply) changes this count, so the assertion has no slack
    // to hide behind.
    expect(uniqueColors.size).toBe(11);
  });

  it("does not collapse machine-generated same-prefix sequential IDs onto runs of the same color", () => {
    // Machine-generated IDs commonly share a long prefix and vary only in a trailing counter. A hash
    // with broken avalanche behavior produces runs of adjacent same-color cards for this shape,
    // which the varied-prefix ID set above cannot detect.
    const sequentialIds = Array.from(
      { length: PALETTE_SIZE },
      (_, index) => `sim-model-${String(index).padStart(8, "0")}`,
    );

    const colors = sequentialIds.map((id) => getResourceColor(id));
    let longestRun = 1;
    let currentRun = 1;
    for (let index = 1; index < colors.length; index++) {
      currentRun = colors[index] === colors[index - 1] ? currentRun + 1 : 1;
      longestRun = Math.max(longestRun, currentRun);
    }

    expect(new Set(colors).size).toBe(18);
    expect(longestRun).toBe(1);
  });

  it("keeps every palette marker above the 3:1 contrast floor on console surfaces", () => {
    const colors = paletteColors();

    expect(colors).toHaveLength(PALETTE_SIZE);
    const surfaces = [
      "oklch(1 0 0)", // :root --card, --popover
      "oklch(0.9538 0.0161 262.75)", // :root --info / selected card, --accent / highlighted item
      "oklch(0.9686 0.0041 121.56)", // :root --background
      "oklch(0.969 0.0053 106.5)", // :root --muted / combobox chip
      "oklch(0.2722 0.0119 264.36)", // .dark --card, --popover
      "oklch(0.3527 0.0608 261.37)", // .dark --info / selected card, --accent / highlighted item
      "oklch(0.2174 0.0084 264.38)", // .dark --background
      "oklch(0.324 0.0136 261.72)", // .dark --muted / combobox chip
    ];
    for (const color of colors) {
      for (const surface of surfaces) {
        expect(contrastRatio(color, surface)).toBeGreaterThanOrEqual(3);
      }
    }
  });

  it("keeps every pair of palette colors visibly apart", () => {
    const colors = paletteColors();

    let closest = Infinity;
    for (let i = 0; i < colors.length; i++) {
      for (let j = i + 1; j < colors.length; j++) {
        closest = Math.min(closest, oklabDistance(colors[i], colors[j]));
      }
    }

    // The chosen palette's closest pair sits at 0.043; a near-duplicate entry would fall below.
    expect(closest).toBeGreaterThanOrEqual(0.04);
  });

  it("returns a hex color for empty and unusual IDs", () => {
    const unusualIds = [
      "",
      "model-with-special-chars-!@#$%",
      "model-very-long-" + "x".repeat(500),
      "UPPERCASE_MODEL_ID",
      "model_with_underscores",
      "123-numeric-only",
      "🎨-emoji-model",
    ];

    for (const id of unusualIds) {
      expect(getResourceColor(id)).toMatch(/^#[0-9a-f]{6}$/i);
    }
  });
});

describe("assignResourceColors", () => {
  it("keeps a key's hash color when no earlier key uses it", () => {
    const { colors } = assignResourceColors(["model-starter", "gemma-4-e2b"]);

    expect(colors.get("model-starter")).toBe(getResourceColor("model-starter"));
    expect(colors.get("gemma-4-e2b")).toBe(getResourceColor("gemma-4-e2b"));
  });

  it("moves a key whose hash color is taken to the next free color", () => {
    // Both IDs hash to the same palette color.
    const { colors } = assignResourceColors(["model-llama", "model-qwen"]);

    expect(colors.get("model-llama")).toBe("#0593bf");
    expect(colors.get("model-qwen")).toBe("#578fa8");
  });

  it("gives every key a distinct color while the palette has a free one", () => {
    const keys = poolKeys(PALETTE_SIZE);

    const { colors } = assignResourceColors(keys);

    expect(new Set(colors.values()).size).toBe(PALETTE_SIZE);
  });

  it("repeats colors evenly once the palette is used up", () => {
    const keys = poolKeys(PALETTE_SIZE * 2 + 5);

    const { colors } = assignResourceColors(keys);

    const counts = new Map<string, number>();
    for (const color of colors.values()) counts.set(color, (counts.get(color) ?? 0) + 1);
    expect(counts.size).toBe(PALETTE_SIZE);
    expect(Math.min(...counts.values())).toBe(2);
    expect(Math.max(...counts.values())).toBe(3);
  });

  it("does not repeat a color before every color is used once", () => {
    const keys = poolKeys(PALETTE_SIZE + 1);

    const { colors } = assignResourceColors(keys);

    const firstColors = keys.slice(0, PALETTE_SIZE).map((key) => colors.get(key));
    expect(new Set(firstColors).size).toBe(PALETTE_SIZE);
  });

  it("changes no earlier key's color when a key joins the end of the pool", () => {
    const keys = poolKeys(10);

    const before = assignResourceColors(keys).colors;
    const after = assignResourceColors([...keys, "model-qwen"]).colors;

    for (const key of keys) expect(after.get(key)).toBe(before.get(key));
  });

  it("only moves keys that lost their hash color when a key leaves the pool", () => {
    const keys = ["model-llama", "model-qwen", "gpu1", "gpu2", "route-1"];

    const before = assignResourceColors(keys).colors;
    const after = assignResourceColors(keys.filter((key) => key !== "model-llama")).colors;

    expect(before.get("model-qwen")).toBe("#578fa8");
    expect(after.get("model-qwen")).toBe("#0593bf");
    for (const key of ["gpu1", "gpu2", "route-1"]) expect(after.get(key)).toBe(before.get(key));
  });

  it("assigns each key once, ignoring repeated keys", () => {
    const { colors } = assignResourceColors(["model-llama", "model-llama", "model-qwen"]);

    expect([...colors.entries()]).toEqual([
      ["model-llama", "#0593bf"],
      ["model-qwen", "#578fa8"],
    ]);
  });

  it("offers the least used color for the next key", () => {
    expect(assignResourceColors([]).nextColor).toBe("#b27587");
    expect(assignResourceColors(["model-starter"]).nextColor).toBe("#b27587");

    const fullPool = assignResourceColors(poolKeys(PALETTE_SIZE));
    expect(fullPool.nextColor).toBe("#b27587");

    const firstTaken = assignResourceColors(["model-llama", "gpu3", "route-2"]);
    expect([...firstTaken.colors.values()]).not.toContain(firstTaken.nextColor);
  });

  it("is deterministic for the same pool", () => {
    const keys = poolKeys(30);

    expect([...assignResourceColors(keys).colors]).toEqual([...assignResourceColors(keys).colors]);
  });
});

function paletteColors(): string[] {
  return [...assignResourceColors(poolKeys(PALETTE_SIZE)).colors.values()];
}

function poolKeys(count: number): string[] {
  return Array.from({ length: count }, (_, index) => `model-site-${index + 1}`);
}
