import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { FieldErrors } from "@/lib/form-validation";

import { type DeployWizardOptions, useDeployWizard } from "./use-deploy-wizard";

const ORDER = ["Profile", "Target", "Settings", "Advanced", "Deploy"] as const;
type Step = (typeof ORDER)[number];

describe("useDeployWizard", () => {
  afterEach(() => {
    document.body.replaceChildren();
  });

  it("shows steps with fields, advanced steps in advanced mode and basic steps by the caller's rule", () => {
    const { result, rerender } = renderWizard({ hasFields: (step) => step !== "Settings" });
    expect(result.current.steps).toEqual(["Profile", "Deploy"]);

    rerender(options({ hasFields: (step) => step !== "Settings", severalTargets: true }));
    expect(result.current.steps).toEqual(["Profile", "Target", "Deploy"]);

    act(() => result.current.toggleAdvanced(true));
    expect(result.current.steps).toEqual(["Profile", "Target", "Advanced", "Deploy"]);
    expect(result.current.announcement).toBe("Advanced on: 4 steps");

    rerender(options({ hasFields: (step) => step !== "Advanced", severalTargets: true }));
    expect(result.current.steps).toEqual(["Profile", "Target", "Settings", "Deploy"]);
  });

  it("moves to the next shown step in order when the current step disappears", () => {
    const { result, rerender } = renderWizard();
    act(() => result.current.toggleAdvanced(true));
    act(() => result.current.goTo("Advanced"));
    expect(result.current.step).toBe("Advanced");

    rerender(options({ hasFields: (step) => step !== "Advanced" }));
    expect(result.current.step).toBe("Deploy");
    expect(result.current.steps).toEqual(["Profile", "Target", "Settings", "Deploy"]);

    act(() => result.current.goTo("Target"));
    act(() => result.current.toggleAdvanced(false));
    expect(result.current.step).toBe("Settings");
  });

  it("tells the caller when Advanced turns off, and only then", () => {
    const onAdvancedOff = vi.fn();
    const { result } = renderWizard({ onAdvancedOff });

    act(() => result.current.toggleAdvanced(true));
    expect(onAdvancedOff).not.toHaveBeenCalled();

    act(() => result.current.toggleAdvanced(false));
    expect(onAdvancedOff).toHaveBeenCalledTimes(1);
    expect(result.current.steps).toEqual(["Profile", "Settings", "Deploy"]);
  });

  it("keeps the Target step decided when leaving the first step until the operator returns", () => {
    const onLeaveFirstStep = vi.fn();
    const { result, rerender } = renderWizard({ onLeaveFirstStep, severalTargets: true });

    act(() => result.current.primary());
    expect(result.current.step).toBe("Target");
    expect(onLeaveFirstStep).toHaveBeenCalledTimes(1);

    rerender(options({ onLeaveFirstStep, severalTargets: false }));
    expect(result.current.steps).toEqual(["Profile", "Target", "Settings", "Deploy"]);

    act(() => result.current.changeStep("Profile"));
    expect(result.current.steps).toEqual(["Profile", "Settings", "Deploy"]);
    expect(onLeaveFirstStep).toHaveBeenCalledTimes(1);
  });

  it("reveals an error on a hidden step by turning Advanced on and focusing its field", () => {
    const control = document.createElement("input");
    document.body.append(control);
    const errors = new Map([["speculativeCount", "Enter a count from 1 to 8."]]);
    const setFieldErrors = vi.fn();
    const { result } = renderWizard({
      errorsFor: (step) => (step === "Advanced" ? errors : new Map()),
      fieldControls: () => new Map([["speculativeCount", control]]),
      setFieldErrors,
    });
    expect(result.current.steps).toEqual(["Profile", "Settings", "Deploy"]);

    let revealed = false;
    act(() => {
      revealed = result.current.revealFirstError();
    });

    expect(revealed).toBe(true);
    expect(result.current.advanced).toBe(true);
    expect(result.current.announcement).toBe("Advanced on to fix advanced settings");
    expect(result.current.step).toBe("Advanced");
    expect(setFieldErrors).toHaveBeenLastCalledWith(errors);
    expect(result.current.revealedErrors).toBe(errors);
    expect(document.activeElement).toBe(control);
  });

  it("blocks moving forward past a step with errors, but not going back or a clean submit", () => {
    const onSubmit = vi.fn();
    let settingsErrors: FieldErrors = new Map([["name", "Enter a name."]]);
    const setFieldErrors = vi.fn();
    const { result } = renderWizard({
      errorsFor: (step) => (step === "Settings" ? settingsErrors : new Map()),
      onSubmit,
      setFieldErrors,
    });

    act(() => result.current.primary());
    expect(result.current.step).toBe("Settings");
    act(() => result.current.primary());
    expect(result.current.step).toBe("Settings");
    act(() => result.current.changeStep("Deploy"));
    expect(result.current.step).toBe("Settings");
    expect(setFieldErrors).toHaveBeenLastCalledWith(settingsErrors);

    act(() => result.current.changeStep("Profile"));
    expect(result.current.step).toBe("Profile");

    settingsErrors = new Map();
    act(() => result.current.primary());
    act(() => result.current.primary());
    expect(result.current.step).toBe("Deploy");
    expect(onSubmit).not.toHaveBeenCalled();

    act(() => result.current.primary());
    expect(onSubmit).toHaveBeenCalledTimes(1);
  });
});

// The wizard as a dialog with a target step drives it: Target shows in basic mode only when
// several targets apply.
type WizardOverrides = Partial<DeployWizardOptions<Step>> & { severalTargets?: boolean };

function renderWizard(overrides: WizardOverrides = {}) {
  return renderHook((props: DeployWizardOptions<Step>) => useDeployWizard(props), {
    initialProps: options(overrides),
  });
}

function options({
  severalTargets = false,
  ...overrides
}: WizardOverrides = {}): DeployWizardOptions<Step> {
  return {
    advancedSteps: ["Advanced"],
    basicShows: (step) => step !== "Target" || severalTargets,
    errorsFor: () => new Map(),
    fieldControls: () => new Map(),
    hasFields: () => true,
    onSubmit: vi.fn(),
    open: true,
    order: ORDER,
    setFieldErrors: vi.fn(),
    ...overrides,
  };
}
