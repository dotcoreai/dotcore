import { describe, expect, it } from "vitest";

import { contrastRatio, parseOklch } from "@/lib/color-contrast";

describe("color contrast", () => {
  it("parses an oklch(L C H) declaration", () => {
    expect(parseOklch("oklch(0.4931 0.1832 261.87)")).toEqual({ l: 0.4931, c: 0.1832, h: 261.87 });
  });

  it("parses an achromatic oklch() value with zero chroma and hue", () => {
    expect(parseOklch("oklch(1 0 0)")).toEqual({ l: 1, c: 0, h: 0 });
  });

  it("throws for a value that is not an oklch() string", () => {
    expect(() => parseOklch("#1f58c7")).toThrow(/oklch/);
  });

  it("throws instead of returning NaN for a malformed numeric component", () => {
    // A stylesheet typo, not a missing declaration: the loose [\d.]+ pattern this module
    // previously used would match "0.18.32" and hand it to Number(), producing NaN components
    // and a NaN contrast ratio instead of this diagnostic error.
    expect(() => parseOklch("oklch(0.4931 0.18.32 261.87)")).toThrow(/oklch/);
    expect(() => parseOklch("oklch(. 0 0)")).toThrow(/oklch/);
  });

  it("rates pure black against pure white at the WCAG maximum of 21:1", () => {
    expect(contrastRatio("oklch(0 0 0)", "oklch(1 0 0)")).toBeCloseTo(21, 5);
  });

  it("rates a color against itself at the WCAG minimum of 1:1", () => {
    expect(contrastRatio("oklch(0.4931 0.1832 261.87)", "oklch(0.4931 0.1832 261.87)")).toBeCloseTo(
      1,
      5,
    );
  });

  it("is symmetric regardless of argument order", () => {
    const a = "oklch(0.226 0.0083 264.39)";
    const b = "oklch(0.9686 0.0041 121.56)";

    expect(contrastRatio(a, b)).toBeCloseTo(contrastRatio(b, a), 10);
  });

  it("throws rather than gamut-clipping a token outside sRGB", () => {
    // l=0.7 c=0.4 h=0 maps to a linear-sRGB red channel of ~1.71, well outside [0, 1]. Silently
    // clamping it would compute a contrast ratio for a color the browser never renders (a browser
    // gamut-maps by hue, not per channel), so this must be reported rather than approximated.
    expect(() => contrastRatio("oklch(0.7 0.4 0)", "oklch(1 0 0)")).toThrow(/gamut/);
  });

  it("matches the known contrast of the console light foreground against its background", () => {
    // demo --ink #1a1c20 on demo --page #f4f5f2.
    const foreground = "oklch(0.226 0.0083 264.39)";
    const background = "oklch(0.9686 0.0041 121.56)";

    expect(contrastRatio(foreground, background)).toBeGreaterThan(15);
    expect(contrastRatio(foreground, background)).toBeLessThan(16);
  });
});
