import { beforeEach, describe, expect, it } from "vitest";

import { resolveInferenceOrigin, testInferenceOrigin } from "@/api/inference-origin";

describe("resolveInferenceOrigin", () => {
  beforeEach(() => {
    delete window.__DOTCORE_CONFIG__;
  });

  it("returns the Vite origin when development fallback is enabled", () => {
    expect(resolveInferenceOrigin("http://100.64.0.9:6407", { allowViteFallback: true })).toBe(
      "http://100.64.0.9:6407",
    );
  });

  it("uses only an explicit test fallback when configuration is absent", () => {
    expect(resolveInferenceOrigin(undefined, { allowTestFallback: true })).toBe(
      testInferenceOrigin,
    );
  });

  it("fails clearly when a non-test environment has no configured origin", () => {
    expect(() => resolveInferenceOrigin(undefined)).toThrow(
      "Dotcore runtime config is missing inferenceOrigin",
    );
    expect(() => resolveInferenceOrigin("  ", { allowViteFallback: true })).toThrow(
      "Invalid Dotcore runtime config",
    );
  });
});
