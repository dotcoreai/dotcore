import { AdvancedToggle } from "./advanced-toggle";
import type { DeploymentWizardShellProps } from "./deployment-wizard-shell";
import type { DeployWizard } from "./use-deploy-wizard";

/** The last step's action, idle and while the dialog's opener works on it. */
export interface DeploySubmitLabels {
  pending: string;
  submit: string;
}

/**
 * The shell props a deploy wizard drives: its steps and their navigation, the Advanced toggle, the
 * announcement and the primary action, which reads Next until the last step.
 */
export function deployWizardShellProps<Step extends string>(
  wizard: DeployWizard<Step>,
  {
    labels = deploySubmitLabels,
    pending,
  }: {
    labels?: DeploySubmitLabels;
    pending: boolean;
  },
): Pick<
  DeploymentWizardShellProps,
  | "announcement"
  | "currentStep"
  | "footerStart"
  | "onPrimary"
  | "onStepChange"
  | "primaryLabel"
  | "steps"
  | "visitedSteps"
> {
  const lastStep = wizard.step === wizard.steps[wizard.steps.length - 1];

  return {
    announcement: wizard.announcement,
    currentStep: wizard.step,
    footerStart: (
      <AdvancedToggle
        checked={wizard.advanced}
        disabled={pending}
        onChange={wizard.toggleAdvanced}
      />
    ),
    onPrimary: wizard.primary,
    // The shell names steps by their labels; only the wizard's own steps are reachable.
    onStepChange: (name) => {
      const step = wizard.steps.find((candidate) => candidate === name);
      if (step) wizard.changeStep(step);
    },
    primaryLabel: lastStep ? (pending ? labels.pending : labels.submit) : "Next",
    steps: wizard.steps,
    visitedSteps: wizard.visited,
  };
}

const deploySubmitLabels: DeploySubmitLabels = { pending: "Deploying…", submit: "Deploy" };
