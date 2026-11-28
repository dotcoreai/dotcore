import type { ReactNode, RefObject } from "react";
import { useEffect, useLayoutEffect, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Notice } from "@/components/ui/notice";
import { cn } from "@/lib/utils";

import { DeploymentDialogFrame } from "./deployment-dialog-frame";

export interface DeploymentWizardShellProps {
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
  /**
   * Shorter names for the step bar, by step name, so the labels fit on one line. Steps without one
   * show their own name; the step's pane and accessible name keep the full name.
   */
  stepLabels?: Readonly<Record<string, string>>;
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
  stepLabels,
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
            stepLabels={stepLabels}
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

/**
 * The step bar: every step shows its number and short name on one line. When the names don't fit
 * the bar's width, only the current step keeps its name and the others show their number.
 */
function DeploymentProgress({
  currentIndex,
  disabled,
  onStepChange,
  stepLabels,
  steps,
  visitedSteps,
}: {
  currentIndex: number;
  disabled: boolean;
  onStepChange: (step: string) => void;
  stepLabels?: Readonly<Record<string, string>>;
  steps: readonly string[];
  visitedSteps?: ReadonlySet<string>;
}) {
  const listRef = useRef<HTMLOListElement>(null);
  const compact = useLabelsOverflow(listRef, steps.length);

  return (
    <ol
      aria-label="Deployment progress"
      className="relative mt-1 block gap-2 sm:mt-2 sm:grid"
      ref={listRef}
      style={{
        gridTemplateColumns: compact
          ? steps.map((_, index) => (index === currentIndex ? "minmax(0, 1fr)" : "auto")).join(" ")
          : `repeat(${steps.length}, minmax(0, 1fr))`,
      }}
    >
      {steps.map((step, index) => {
        const current = index === currentIndex;
        // Earlier steps are always reachable, as going back skips nothing; later ones only once visited.
        const reachable = current || index < currentIndex || Boolean(visitedSteps?.has(step));
        const label = `${index + 1}. ${stepLabels?.[step] ?? step}`;
        return (
          <li className={cn("hidden sm:block", current && "block")} key={step}>
            <button
              aria-current={current ? "step" : undefined}
              className={cn(
                "relative block w-full touch-target border-t-2 pt-2 text-left text-xs whitespace-nowrap text-muted-foreground outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background disabled:cursor-default disabled:opacity-60",
                index <= currentIndex && "border-primary text-foreground",
                reachable && !current && "cursor-pointer hover:text-primary",
                current && "cursor-default",
              )}
              disabled={disabled || !reachable}
              onClick={() => {
                if (!current) onStepChange(step);
              }}
              title={step}
              type="button"
            >
              {/* The full name is the button's accessible name, whichever label is drawn. */}
              <span className="sr-only">
                {index + 1}. {step}
              </span>
              <span aria-hidden="true" className="hidden sm:inline" data-step-label="">
                {compact && !current ? index + 1 : label}
              </span>
              {current ? (
                <span aria-hidden="true" className="sm:hidden">
                  Step {index + 1} of {steps.length} — {step}
                </span>
              ) : null}
            </button>
            {/* Measures the full label whatever is shown, to tell when the names don't fit. */}
            <span
              aria-hidden="true"
              className="invisible absolute hidden text-xs whitespace-nowrap sm:inline"
              data-step-measure=""
            >
              {label}
            </span>
          </li>
        );
      })}
    </ol>
  );
}

/**
 * Whether the widest full step label is wider than its column of the list, re-measured when the
 * list resizes. Unmeasurable layouts, where every width is zero, count as fitting.
 */
function useLabelsOverflow(listRef: RefObject<HTMLOListElement | null>, stepCount: number) {
  const [overflow, setOverflow] = useState(false);

  useLayoutEffect(() => {
    const list = listRef.current;
    if (!list) return;
    const measure = () => {
      const widest = Math.max(
        0,
        ...Array.from(
          list.querySelectorAll<HTMLElement>("[data-step-measure]"),
          (label) => label.offsetWidth,
        ),
      );
      const gap = Number.parseFloat(getComputedStyle(list).columnGap) || 0;
      const column = (list.clientWidth - gap * (stepCount - 1)) / stepCount;
      setOverflow(widest > column);
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(list);
    return () => observer.disconnect();
  }, [listRef, stepCount]);

  return overflow;
}
