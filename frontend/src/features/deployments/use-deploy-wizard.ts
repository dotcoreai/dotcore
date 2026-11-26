import { useEffect, useRef, useState } from "react";

import {
  type FieldControls,
  type FieldErrors,
  focusFirstInvalidField,
} from "@/lib/form-validation";

import { useAdvancedMode } from "./use-advanced-mode";
import { useWizardSteps } from "./use-wizard-steps";

/** What a deploy dialog tells its wizard about its steps and fields. */
export interface DeployWizardOptions<Step extends string> {
  /** Every step in flow order. */
  order: readonly Step[];
  /** The steps that show only in advanced mode. */
  advancedSteps: readonly Step[];
  /**
   * Whether a step outside `advancedSteps` shows in basic mode, such as a target step only when
   * several targets apply. It is read when leaving the first step and kept until the operator
   * returns there. Every step shows by default.
   */
  basicShows?: (step: Step) => boolean;
  /** Whether a step has fields; a step without them never shows. */
  hasFields: (step: Step) => boolean;
  /** The current validation errors of a step's fields. */
  errorsFor: (step: Step) => FieldErrors;
  /** The rendered control of each field, which takes focus when the field is the first invalid. */
  fieldControls: () => FieldControls;
  /** Stores the errors the dialog shows under its fields. */
  setFieldErrors: (errors: FieldErrors) => void;
  /** Called when the operator moves past the first step, such as to keep an automatic choice. */
  onLeaveFirstStep?: () => void;
  /** Called when the operator turns Advanced off, such as to reset the advanced inputs. */
  onAdvancedOff?: () => void;
  /** Called by the primary action on the last step. */
  onSubmit: () => void;
  open: boolean;
}

/** The shown steps of a deploy dialog, its navigation and its error reveal. */
export interface DeployWizard<Step extends string> {
  advanced: boolean;
  /** Message for the shell's live region, such as the step count after a toggle. */
  announcement: string;
  /** Moves to a step without validating the current one. */
  goTo: (step: Step) => void;
  /** Handles a step header or Back: moving forward first validates the current step. */
  changeStep: (step: Step) => void;
  /** Handles the primary action: Next validates the current step, the last step submits. */
  primary: () => void;
  /**
   * Shows the errors of the first step among `owners` that has any, on that step, and reports
   * whether one did. The default owners are every step with fields.
   */
  revealFirstError: (owners?: readonly Step[]) => boolean;
  /** The errors of the latest blocked navigation; a new value opens the sections they flag. */
  revealedErrors: FieldErrors;
  step: Step;
  steps: readonly Step[];
  toggleAdvanced: (advanced: boolean) => void;
  visited: ReadonlySet<Step>;
}

/**
 * Drives a deploy dialog's steps. A step shows only when it has fields; advanced steps show only in
 * advanced mode, and the other steps in basic mode by the caller's rule, decided when leaving the
 * first step so the steps ahead stay put. A blocked navigation shows the step's errors and focuses
 * the first invalid field; an error on a hidden step turns Advanced on and opens it.
 */
export function useDeployWizard<Step extends string>({
  advancedSteps,
  basicShows = showsAlways,
  errorsFor,
  fieldControls,
  hasFields,
  onAdvancedOff,
  onLeaveFirstStep,
  onSubmit,
  open,
  order,
  setFieldErrors,
}: DeployWizardOptions<Step>): DeployWizard<Step> {
  const advancedMode = useAdvancedMode(open);
  const { advanced } = advancedMode;
  // The steps basic mode shows, fixed when leaving the first step.
  const [lockedBasic, setLockedBasic] = useState<readonly Step[]>();
  const [revealedErrors, setRevealedErrors] = useState<FieldErrors>(new Map());
  const pendingFocusRef = useRef<FieldErrors | undefined>(undefined);

  const basicSteps = () => order.filter(basicShows);
  const shown = (candidate: Step, advancedOn: boolean) =>
    hasFields(candidate) &&
    (advancedOn ||
      (!advancedSteps.includes(candidate) && (lockedBasic ?? basicSteps()).includes(candidate)));
  const stepsFor = (advancedOn: boolean) =>
    order.filter((candidate) => shown(candidate, advancedOn));
  const steps = stepsFor(advanced);
  const wizard = useWizardSteps(steps, order);
  const step = wizard.current;

  const goTo = (next: Step) => {
    const firstStep = steps[0];
    const leavingFirstStep = step === firstStep && next !== firstStep;
    setLockedBasic(next === firstStep ? undefined : leavingFirstStep ? basicSteps() : lockedBasic);
    if (leavingFirstStep) onLeaveFirstStep?.();
    wizard.goTo(next);
  };
  // Focus waits for the render that shows the errors, which also opens a collapsed section or
  // group holding an invalid field.
  const exposeErrors = (errors: FieldErrors): boolean => {
    setFieldErrors(errors);
    pendingFocusRef.current = errors.size ? errors : undefined;
    if (!errors.size) return true;
    setRevealedErrors(errors);
    return false;
  };
  const revealFirstError = (owners: readonly Step[] = stepsFor(true)): boolean => {
    for (const owner of owners) {
      const errors = errorsFor(owner);
      if (!errors.size) continue;
      if (!steps.includes(owner))
        advancedMode.enable(`Advanced on to fix ${owner.toLocaleLowerCase()} settings`);
      exposeErrors(errors);
      if (owner !== step) goTo(owner);
      return true;
    }
    return false;
  };

  // Runs after the shell focuses a new step heading, so the invalid field keeps focus. The request
  // stays until a render shows one of its fields: an earlier render may still hold the step or
  // section the field isn't in yet.
  useEffect(() => {
    const errors = pendingFocusRef.current;
    if (errors && focusFirstInvalidField(errors, fieldControls())) {
      pendingFocusRef.current = undefined;
    }
  });

  return {
    advanced,
    announcement: advancedMode.announcement,
    changeStep: (next) => {
      if (steps.indexOf(next) > steps.indexOf(step) && !exposeErrors(errorsFor(step))) return;
      goTo(next);
    },
    goTo,
    primary: () => {
      if (step === steps[steps.length - 1]) {
        onSubmit();
        return;
      }
      if (exposeErrors(errorsFor(step))) goTo(steps[steps.indexOf(step) + 1]);
    },
    revealFirstError,
    revealedErrors,
    step,
    steps,
    toggleAdvanced: (next) => {
      advancedMode.toggle(next, stepsFor(next).length);
      if (!next) onAdvancedOff?.();
    },
    visited: wizard.visited,
  };
}

function showsAlways(): boolean {
  return true;
}
