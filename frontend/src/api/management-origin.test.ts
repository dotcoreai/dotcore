import { beforeEach, describe, expect, it } from "vitest";

import { resolveManagementOrigin, testManagementOrigin } from "@/api/management-origin";

describe("resolveManagementOrigin", () => {
  beforeEach(() => {
    delete window.__DOTCORE_CONFIG__;
  });

  it("returns the Vite origin when development fallback is enabled", () => {
    expect(resolveManagementOrigin("http://100.64.0.7:6207", { allowViteFallback: true })).toBe(
      "http://100.64.0.7:6207",
    );
  });

  it("uses loopback only when the test fallback is explicitly allowed", () => {
    expect(resolveManagementOrigin(undefined, { allowTestFallback: true })).toBe(
      testManagementOrigin,
    );
  });

  it("fails clearly when a non-test environment has no configured origin", () => {
    expect(() => resolveManagementOrigin(undefined)).toThrow(
      "Dotcore runtime config is missing managementOrigin",
    );
  });

  it("rejects an invalid Vite origin", () => {
    expect(() => resolveManagementOrigin("not a URL", { allowViteFallback: true })).toThrow(
      "Invalid Dotcore runtime config",
    );
  });
});
