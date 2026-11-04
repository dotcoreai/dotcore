import { describe, expect, it } from "vitest";

import { normalizeDeploymentName, validateDeploymentName } from "./deployment-name";

describe("deployment names", () => {
  it("trims accepted names and preserves internal Unicode and punctuation", () => {
    expect(normalizeDeploymentName("  Café / GPU  ")).toBe("Café / GPU");
    expect(validateDeploymentName("  Café / GPU  ")).toBeUndefined();
  });

  it("rejects blank, control-character, and overlong names", () => {
    expect(validateDeploymentName(" \t ")).toBe("Deployment name is required.");
    expect(validateDeploymentName("valid\nname")).toBe(
      "Deployment name cannot contain control characters.",
    );
    expect(validateDeploymentName("a".repeat(129))).toBe(
      "Deployment name must be 128 characters or fewer.",
    );
  });
});
