import { describe, expect, it } from "vitest";

import { deriveUpstreamDeploymentName, validateUpstreamBaseUrl } from "./upstream-form";

describe("upstream deployment value derivation", () => {
  it("limits a derived deployment name to 128 Unicode characters", () => {
    expect(deriveUpstreamDeploymentName(`  ${"🙂".repeat(140)}  `)).toBe("🙂".repeat(128));
  });
});

describe("validateUpstreamBaseUrl", () => {
  it.each([
    ["https://api.example.com/v1", undefined],
    ["http://localhost:8080/v1", undefined],
    ["ftp://api.example.com", "Base URL must use HTTP or HTTPS."],
    ["https:///v1", "Base URL must include a host."],
    ["https://user:secret@api.example.com", "Base URL must not include credentials."], // trufflehog:ignore
    ["https://api.example.com/v1?region=one", "Base URL must not include a query."],
    ["https://api.example.com/v1#models", "Base URL must not include a fragment."],
    ["not a url", "Enter a valid Base URL."],
    ["", "Base URL is required."],
  ])("validates %j", (value, expected) => {
    expect(validateUpstreamBaseUrl(value)).toBe(expected);
  });
});
