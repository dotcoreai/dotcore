import { describe, expect, it } from "vitest";

import {
  deploymentExposure,
  deriveRouteName,
  effectiveRouteName,
  routeBlockingMessage,
  validateDeploymentExposure,
  validateRouteName,
} from "./deployment-exposure";

describe("deriveRouteName", () => {
  it.each([
    ["Gemma E2B Support Bot", "gemma-e2b-support-bot"],
    [" Provider/Model Name (v2)! ", "provider-model-name-v2"],
    ["123/model", "model-123-model"],
    [`A${"b".repeat(80)}`, `a${"b".repeat(62)}`],
    [`${"a".repeat(62)} b`, "a".repeat(62)],
    ["---", ""],
    ["", ""],
    ["日本語", ""],
    ["claude-3.5-sonnet", "claude-3.5-sonnet"],
    ["v1..beta", "v1.beta"],
    ["..foo", "foo"],
    ["...", ""],
    ["model.", "model"],
  ])("derives %j as %j", (deploymentName, expected) => {
    expect(deriveRouteName(deploymentName)).toBe(expected);
  });
});

describe("effectiveRouteName", () => {
  it("follows the deployment name until the route name is set", () => {
    expect(effectiveRouteName({ exposure: "direct" }, "My Model")).toBe("my-model");
    expect(effectiveRouteName({ exposure: "direct", routeName: " custom " }, "My Model")).toBe(
      "custom",
    );
  });
});

describe("validateRouteName", () => {
  it("accepts a valid unused name", () => {
    expect(validateRouteName("support", { existingRouteNames: ["other"] })).toBeUndefined();
    expect(
      validateRouteName("claude-3.5-sonnet", { existingRouteNames: ["support"] }),
    ).toBeUndefined();
  });

  it("requires a name", () => {
    expect(validateRouteName("")).toBe("Route name is required for a Direct deployment.");
  });

  it("rejects names outside the route pattern", () => {
    const message =
      "Route name must start with a letter and contain only lowercase letters, numbers, hyphens, and periods.";
    expect(validateRouteName("1abc")).toBe(message);
    expect(validateRouteName("Support")).toBe(message);
    expect(validateRouteName(`a${"b".repeat(63)}`)).toBe(message);
    expect(validateRouteName(".5model")).toBe(message);
    expect(validateRouteName("mod_el")).toBe(message);
  });

  it("names the colliding route", () => {
    expect(validateRouteName("support", { existingRouteNames: ["support"] })).toBe(
      'An inference route named "support" already exists.',
    );
  });

  it("asks for a route name when none can be derived", () => {
    expect(validateRouteName("", { following: true })).toBe(
      "No valid route name can be derived from the deployment name. Enter a route name.",
    );
  });
});

describe("deployment exposure values", () => {
  it("submits and validates the effective route name", () => {
    const values = { exposure: "direct" as const };

    expect(deploymentExposure(values, "My Model")).toEqual({
      kind: { case: "direct", value: { routeName: "my-model" } },
    });
    expect(validateDeploymentExposure(values, "My Model", ["my-model"])).toBe(
      'An inference route named "my-model" already exists.',
    );
    expect(validateDeploymentExposure(values, "日本語")).toBe(
      "No valid route name can be derived from the deployment name. Enter a route name.",
    );
  });

  it("does not validate a hidden deployment", () => {
    expect(validateDeploymentExposure({ exposure: "hidden" }, "日本語")).toBeUndefined();
    expect(deploymentExposure({ exposure: "hidden" }, "x")).toEqual({
      kind: { case: "hidden", value: {} },
    });
  });
});

describe("routeBlockingMessage", () => {
  const message = ({
    deploymentName,
    routeName,
    existingRouteNames = [],
    exposure = "direct",
    conflict,
  }: {
    deploymentName: string;
    routeName?: string;
    existingRouteNames?: string[];
    exposure?: "direct" | "hidden";
    conflict?: { routeName?: string };
  }) =>
    routeBlockingMessage({
      conflict,
      deploymentName,
      existingRouteNames,
      values: { exposure, routeName },
    });

  it("leaves a route following an invalid deployment name to the name error", () => {
    expect(message({ deploymentName: "", existingRouteNames: [""] })).toBeUndefined();
    expect(message({ deploymentName: "   " })).toBeUndefined();
  });

  it("explains a route that cannot be derived from a valid deployment name", () => {
    expect(message({ deploymentName: "日本語" })).toBe(
      "No valid route name can be derived from the deployment name. Enter a route name.",
    );
  });

  it("leaves a custom route's format error to blur and Deploy", () => {
    expect(message({ deploymentName: "Test Model", routeName: "Bad Route" })).toBeUndefined();
  });

  it("names both fixes for a taken route that follows the name", () => {
    expect(message({ deploymentName: "Test Model", existingRouteNames: ["test-model"] })).toBe(
      'An inference route named "test-model" already exists. Change the route or the deployment name.',
    );
  });

  it("names no rename for a taken custom route", () => {
    expect(
      message({
        deploymentName: "Test Model",
        routeName: "taken",
        existingRouteNames: ["taken"],
      }),
    ).toBe('An inference route named "taken" already exists.');
  });

  it("reports a backend conflict only while it matches the route", () => {
    expect(message({ deploymentName: "Test Model", conflict: { routeName: "test-model" } })).toBe(
      'An inference route named "test-model" already exists. Change the route or the deployment name.',
    );
    expect(
      message({
        deploymentName: "Test Model",
        routeName: "other",
        conflict: { routeName: "test-model" },
      }),
    ).toBeUndefined();
  });

  it("ignores a taken route when the deployment is hidden", () => {
    expect(
      message({
        deploymentName: "Test Model",
        existingRouteNames: ["test-model"],
        exposure: "hidden",
      }),
    ).toBeUndefined();
  });
});
