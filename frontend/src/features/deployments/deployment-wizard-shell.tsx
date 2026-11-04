import type { ReactNode, RefObject } from "react";
import { useEffect, useRef } from "react";

import { Button } from "@/components/ui/button";
import { Notice } from "@/components/ui/notice";
import { cn } from "@/lib/utils";

import { DeploymentDialogFrame } from "./deployment-dialog-frame";

interface DeploymentWizardShellProps {
  /**
   * Message for the polite live region, such as a change to the step list. The region renders
   * whenever this is defined; pass an empty string to mount it before the first message.
   */
  announcement?: string;
  children: ReactNode;
  currentStep: string;
  /** Controls at the footer's start, such as a Customize button or an Advanced toggle. */
  footerStart?: ReactNode;
  onOpenChange: (open: boolean) => void;
  onPrimary: () => void;
  /** Called by a reachable step header or by Back with the destination step name. */
  onStepChange: (step: string) => void;
  open: boolean;
  openerRef?: RefObject<HTMLElement | null>;
  pending?: boolean;
  /** Content shown on every step, fixed above the footer while the step content scrolls. */
  persistentContent?: ReactNode;
  primaryDisabled?: boolean;
  primaryLabel: string;
  resourceColorKey: string;
  resourceName: string;
  steps: readonly string[];
  /** Submission error shown above the step content; the dialog decides on which steps. */
  submitError?: string;
  title: string;
  visitedSteps?: ReadonlySet<string>;
}

/**
 * Provides shared presentation and navigation for deployment wizards: step headers, footer layout,
 * focus on step change, and announcements. Dialogs own their steps, content, validation, and which
 * step receives submit errors. A single step renders without step headers or Back.
 */
export function DeploymentWizardShell({
  announcement,
  children,
  currentStep,
  footerStart,
  onOpenChange,
  onPrimary,
  onStepChange,
  open,
  openerRef,
  pending = false,
  persistentContent,
  primaryDisabled = false,
  primaryLabel,
  resourceColorKey,
  resourceName,
  steps,
  submitError,
  title,
  visitedSteps,
}: DeploymentWizardShellProps) {
  const currentIndex = steps.indexOf(currentStep);
  const previousStep = currentIndex > 0 ? steps[currentIndex - 1] : undefined;
  const bodyRef = useRef<HTMLDivElement>(null);
  const previousStepRef = useRef(currentStep);

  useEffect(() => {
    if (!open) {
      previousStepRef.current = currentStep;
      return;
    }
    if (previousStepRef.current === currentStep) return;
    previousStepRef.current = currentStep;
    const heading = bodyRef.current?.querySelector<HTMLElement>("[data-deployment-step-heading]");
    heading?.focus({ preventScroll: true });
    heading?.scrollIntoView?.({ block: "nearest" });
  }, [currentStep, open]);

  return (
    <DeploymentDialogFrame
      actions={
        <>
          {previousStep ? (
            <Button
              disabled={pending}
              onClick={() => onStepChange(previousStep)}
              size="lg"
              type="button"
              variant="outline"
            >
              Back
            </Button>
          ) : null}
          <Button disabled={pending || primaryDisabled} size="lg" type="submit">
            {primaryLabel}
          </Button>
        </>
      }
      bodyRef={bodyRef}
      footerStart={footerStart}
      onOpenChange={onOpenChange}
      onSubmit={onPrimary}
      open={open}
      openerRef={openerRef}
      pending={pending}
      persistentContent={persistentContent}
      progress={
        steps.length > 1 ? (
          <DeploymentProgress
            currentIndex={currentIndex}
            disabled={pending}
            onStepChange={onStepChange}
            steps={steps}
            visitedSteps={visitedSteps}
          />
        ) : undefined
      }
      resourceColorKey={resourceColorKey}
      resourceName={resourceName}
      submitDisabled={primaryDisabled}
      title={title}
    >
      {announcement !== undefined ? (
        <p aria-live="polite" className="sr-only" role="status">
          {announcement}
        </p>
      ) : null}
      {submitError ? (
        <Notice className="mb-4" role="alert" tone="destructive">
          {submitError}
        </Notice>
      ) : null}
      {children}
    </DeploymentDialogFrame>
  );
}

function DeploymentProgress({
  currentIndex,
  disabled,
  onStepChange,
  steps,
  visitedSteps,
}: {
  currentIndex: number;
  disabled: boolean;
  onStepChange: (step: string) => void;
  steps: readonly string[];
  visitedSteps?: ReadonlySet<string>;
}) {
  return (
    <ol
      aria-label="Deployment progress"
      className="mt-1 block gap-2 sm:mt-2 sm:grid"
      style={{ gridTemplateColumns: `repeat(${steps.length}, minmax(0, 1fr))` }}
    >
      {steps.map((step, index) => {
        const current = index === currentIndex;
        // Earlier steps are always reachable, as going back skips nothing; later ones only once visited.
        const reachable = current || index < currentIndex || Boolean(visitedSteps?.has(step));
        return (
          <li className={cn("hidden sm:block", current && "block")} key={step}>
            <button
              aria-current={current ? "step" : undefined}
              className={cn(
                "relative block w-full touch-target border-t-2 pt-2 text-left text-xs text-muted-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:cursor-default disabled:opacity-60",
                index <= currentIndex && "border-primary text-foreground",
                reachable && !current && "cursor-pointer hover:text-primary",
                current && "cursor-default",
              )}
              disabled={disabled || !reachable}
              onClick={() => {
                if (!current) onStepChange(step);
              }}
              type="button"
            >
              <span className="hidden sm:inline" data-step-label="">
                {index + 1}. {step}
              </span>
              {current ? (
                <span className="sm:hidden">
                  Step {index + 1} of {steps.length} — {step}
                </span>
              ) : null}
            </button>
          </li>
        );
      })}
    </ol>
  );
}
