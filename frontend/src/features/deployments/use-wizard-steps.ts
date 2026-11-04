import { useState } from "react";

/** Current and visited steps of a deployment wizard, with navigation to a named step. */
export interface WizardSteps {
  current: string;
  goTo: (step: string) => void;
  visited: ReadonlySet<string>;
}

/**
 * Tracks the current step and the visited steps of a wizard by step name, so the step list can
 * change while the dialog is open. When the step list changes, the current step is kept if it
 * still exists, otherwise `fallback` picks its replacement, and steps that appear are not visited.
 */
export function useWizardSteps(
  steps: readonly string[],
  fallback?: (missingStep: string) => string,
): WizardSteps {
  const [state, setState] = useState<WizardStepsState>(() => ({
    current: steps[0] ?? "",
    steps,
    visited: new Set(steps.slice(0, 1)),
  }));
  let effective = state;

  // Adjusting state during render applies the step-list rule before the stale step can render.
  if (!sameSteps(state.steps, steps)) {
    effective = reconcileSteps(state, steps, fallback);
    setState(effective);
  }

  return {
    current: effective.current,
    goTo: (step) =>
      setState((previous) => ({
        ...previous,
        current: step,
        visited: new Set(previous.visited).add(step),
      })),
    visited: effective.visited,
  };
}

interface WizardStepsState {
  current: string;
  steps: readonly string[];
  visited: ReadonlySet<string>;
}

function reconcileSteps(
  state: WizardStepsState,
  steps: readonly string[],
  fallback: ((missingStep: string) => string) | undefined,
): WizardStepsState {
  const fallbackStep = fallback?.(state.current);
  const current = steps.includes(state.current)
    ? state.current
    : fallbackStep && steps.includes(fallbackStep)
      ? fallbackStep
      : (steps[0] ?? "");
  const visited = new Set([...state.visited].filter((step) => steps.includes(step)));
  visited.add(current);
  return { current, steps, visited };
}

function sameSteps(left: readonly string[], right: readonly string[]): boolean {
  return left.length === right.length && left.every((step, index) => step === right[index]);
}
