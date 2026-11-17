import { Check } from "lucide-react";
import type { ReactNode } from "react";

import { cn } from "@/lib/utils";

/** How far a step of the add machine flow has come. */
export type StepStatus = "active" | "done" | "todo" | "warning";

/** The numbered steps of a flow, joined by a connector line. */
export function StepList({ children }: { children: ReactNode }) {
  return <ol className="flex flex-col">{children}</ol>;
}

/**
 * One step: a numbered marker that turns into a check when done or an alert when it needs
 * attention, its title, and the body that belongs to it.
 */
export function Step({
  children,
  number,
  status,
  title,
}: {
  children?: ReactNode;
  number: number;
  status: StepStatus;
  title: string;
}) {
  return (
    <li
      aria-current={status === "active" ? "step" : undefined}
      className={cn(
        "relative grid grid-cols-[1.75rem_minmax(0,1fr)] gap-4 pb-6 last:pb-0",
        "not-last:before:absolute not-last:before:top-[1.9rem] not-last:before:bottom-1 not-last:before:left-[0.8125rem] not-last:before:w-0.5 not-last:before:bg-border",
        status === "done" && "not-last:before:bg-success-foreground/50",
      )}
      data-status={status}
    >
      <span
        className={cn(
          "grid size-7 place-items-center rounded-full border-[1.5px] text-xs font-bold",
          DOT_STYLES[status],
        )}
      >
        <StepMarker number={number} status={status} />
      </span>
      <div className="min-w-0">
        <div
          className={cn(
            "text-sm leading-7 font-semibold",
            status === "todo" && "font-medium text-muted-foreground",
          )}
        >
          {title}
        </div>
        {children ? <div className="mt-1.5 flex flex-col gap-3">{children}</div> : null}
      </div>
    </li>
  );
}

function StepMarker({ number, status }: { number: number; status: StepStatus }) {
  switch (status) {
    case "done":
      return (
        <>
          <Check aria-hidden="true" className="size-3.5" />
          <span className="sr-only">Done</span>
        </>
      );
    case "warning":
      return (
        <>
          <span aria-hidden="true">!</span>
          <span className="sr-only">Needs attention</span>
        </>
      );
    default:
      return number;
  }
}

const DOT_STYLES: Record<StepStatus, string> = {
  active: "border-primary bg-accent text-primary",
  done: "border-transparent bg-success text-success-foreground",
  todo: "border-input bg-popover text-muted-foreground",
  warning: "border-transparent bg-warning text-warning-foreground",
};
