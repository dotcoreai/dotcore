import { screen } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { expect } from "vitest";

import type { ModelDeployStep } from "./model-deploy-launch";

/** The step the deploy dialog shows, from its "Step n of m — Name" line. */
export function currentStep(): string | undefined {
  return /^Step \d+ of \d+ — (.+)$/.exec(
    screen.getByText(/^Step \d+ of \d+ — /).textContent ?? "",
  )?.[1];
}

/** Presses Next until the dialog shows the step, failing when a step blocks the way. */
export async function goToStep(user: UserEvent, step: ModelDeployStep) {
  for (let presses = 0; currentStep() !== step && presses < 6; presses += 1) {
    const before = currentStep();
    await user.click(screen.getByRole("button", { name: "Next" }));
    expect(currentStep(), `Next stayed on ${before}`).not.toBe(before);
  }
  expect(currentStep()).toBe(step);
}

/** Waits for the first step, turns Advanced on and moves on to the step. */
export async function openAdvancedStep(
  user: UserEvent,
  step: ModelDeployStep = "Performance & memory",
) {
  await screen.findByText(/^Step 1 of \d+ — Model & machine$/);
  await user.click(screen.getByRole("checkbox", { name: "Advanced" }));
  await goToStep(user, step);
}
