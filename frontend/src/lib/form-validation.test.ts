import { describe, expect, it } from "vitest";

import { focusFirstInvalidField, toFieldErrors } from "@/lib/form-validation";

describe("form validation", () => {
  it("preserves declared field order while omitting valid fields", () => {
    const errors = toFieldErrors([
      ["route-name", "Enter a route name before saving."],
      ["deployment", undefined],
      ["fallback-group", "Choose a deployment before saving."],
    ]);

    expect([...errors]).toEqual([
      ["route-name", "Enter a route name before saving."],
      ["fallback-group", "Choose a deployment before saving."],
    ]);
  });

  it("focuses the first registered invalid field without focusing later errors", () => {
    const routeName = document.createElement("input");
    const deployment = document.createElement("input");
    document.body.append(routeName, deployment);

    try {
      const errors = toFieldErrors([
        ["route-name", "Enter a route name before saving."],
        ["deployment", "Choose a deployment before saving."],
      ]);
      const controls = new Map([
        ["route-name", routeName],
        ["deployment", deployment],
      ]);

      expect(focusFirstInvalidField(errors, controls)).toBe(true);

      expect(routeName).toHaveFocus();
      expect(deployment).not.toHaveFocus();
    } finally {
      routeName.remove();
      deployment.remove();
    }
  });

  it("reports no focus when no invalid field has a control yet", () => {
    const errors = toFieldErrors([["route-name", "Enter a route name before saving."]]);
    const focused = document.activeElement;

    expect(focusFirstInvalidField(errors, new Map([["route-name", null]]))).toBe(false);
    expect(document.activeElement).toBe(focused);
  });
});
