import { beforeEach, describe, expect, it } from "vitest";

import { readRuntimeConfig, resolveOrigin } from "@/api/runtime-config";

describe("Dotcore runtime configuration", () => {
  beforeEach(() => {
    delete window.__DOTCORE_CONFIG__;
  });

  it("reads both validated origins from the browser global", () => {
    window.__DOTCORE_CONFIG__ = {
      managementOrigin: "https://backend.example:6207/",
      inferenceOrigin: "http://inference.example:6407",
    };

    expect(readRuntimeConfig()).toEqual({
      managementOrigin: "https://backend.example:6207",
      inferenceOrigin: "http://inference.example:6407",
    });
  });

  it("maps same-origin runtime values to the browser origin", () => {
    window.__DOTCORE_CONFIG__ = {
      managementOrigin: "same-origin",
      inferenceOrigin: "same-origin",
    };

    expect(readRuntimeConfig()).toEqual({
      managementOrigin: window.location.origin,
      inferenceOrigin: window.location.origin,
    });
    expect(
      resolveOrigin("inferenceOrigin", "https://vite.example:6407", "http://127.0.0.1:50051"),
    ).toBe(window.location.origin);
  });

  it("prefers runtime values over Vite values", () => {
    window.__DOTCORE_CONFIG__ = {
      managementOrigin: "https://runtime.example:6207",
      inferenceOrigin: "https://runtime.example:6407",
    };

    expect(
      resolveOrigin("managementOrigin", "https://vite.example:6207", "http://127.0.0.1:50051", {
        allowViteFallback: true,
      }),
    ).toBe("https://runtime.example:6207");
  });

  it("rejects a partial runtime configuration", () => {
    window.__DOTCORE_CONFIG__ = { managementOrigin: "https://backend.example:6207" };

    expect(() => readRuntimeConfig()).toThrow(
      "Invalid Dotcore runtime config: inferenceOrigin must be a non-empty HTTP(S) origin",
    );
  });

  it.each([
    ["relative URL", "backend.example:6207"],
    ["unsupported protocol", "ftp://backend.example:6207"],
    ["URL path", "https://backend.example:6207/api"],
  ])("rejects a %s", (_, managementOrigin) => {
    window.__DOTCORE_CONFIG__ = {
      managementOrigin,
      inferenceOrigin: "https://inference.example:6407",
    };

    expect(() => readRuntimeConfig()).toThrow(
      "Invalid Dotcore runtime config: managementOrigin must be a valid HTTP(S) origin",
    );
  });

  it("uses the test fallback only when it is explicitly enabled", () => {
    expect(
      resolveOrigin("managementOrigin", undefined, "http://127.0.0.1:50051", {
        allowTestFallback: true,
      }),
    ).toBe("http://127.0.0.1:50051");
    expect(() => resolveOrigin("managementOrigin", undefined, "http://127.0.0.1:50051")).toThrow(
      "Dotcore runtime config is missing managementOrigin",
    );
  });
});
