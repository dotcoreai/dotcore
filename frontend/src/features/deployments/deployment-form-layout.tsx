import type { ReactNode } from "react";
import { useId } from "react";

import { cn } from "@/lib/utils";

interface DeploymentFormProps {
  children: ReactNode;
  className?: string;
}

/** Groups deployment fields in the shared form-first layout. */
export function DeploymentForm({ children, className }: DeploymentFormProps) {
  return (
    <div className={cn("grid gap-6", className)} data-testid="deployment-form">
      {children}
    </div>
  );
}

/**
 * Names a wizard step's pane for assistive technology and receives focus when the step opens. It is
 * visually hidden because the pane's first field already labels the step on screen.
 */
export function DeploymentStepHeading({ children }: { children: ReactNode }) {
  return (
    <h2 className="sr-only" data-deployment-step-heading="" tabIndex={-1}>
      {children}
    </h2>
  );
}

interface DeploymentFormSectionProps {
  children: ReactNode;
  className?: string;
  focusTarget?: boolean;
  title: string;
}

/** Groups related deployment inputs under one shared visual and semantic heading. */
export function DeploymentFormSection({
  children,
  className,
  focusTarget = false,
  title,
}: DeploymentFormSectionProps) {
  const headingId = useId();

  return (
    <section
      aria-labelledby={headingId}
      className={cn("grid gap-3", className)}
      data-testid="deployment-form-section"
    >
      <h2
        className="rounded-sm text-sm font-semibold outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
        data-deployment-step-heading={focusTarget ? "" : undefined}
        id={headingId}
        tabIndex={focusTarget ? -1 : undefined}
      >
        {title}
      </h2>
      {children}
    </section>
  );
}
