import { useState } from "react";

/** Current and visited steps of a deployment wizard, with navigation to a named step. */
export interface WizardSteps<Step extends string> {
  current: Step;
  goTo: (step: Step) => void;
  visited: ReadonlySet<Step>;
}

/**
 * Tracks the current step and the visited steps of a wizard by step name, so the step list can
 * change while the dialog is open. When the step list changes, the current step is kept if it
 * still exists, otherwise `fallback` picks its replacement, and steps that appear are not visited.
 */
export function useWizardSteps<Step extends string>(
  steps: readonly Step[],
  fallback?: (missingStep: Step) => Step,
): WizardSteps<Step> {
  const [state, setState] = useState<WizardStepsState<Step>>(() => ({
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

interface WizardStepsState<Step extends string> {
  current: Step;
  steps: readonly Step[];
  visited: ReadonlySet<Step>;
}

function reconcileSteps<Step extends string>(
  state: WizardStepsState<Step>,
  steps: readonly Step[],
  fallback: ((missingStep: Step) => Step) | undefined,
): WizardStepsState<Step> {
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

function sameSteps<Step extends string>(left: readonly Step[], right: readonly Step[]): boolean {
  return left.length === right.length && left.every((step, index) => step === right[index]);
}
