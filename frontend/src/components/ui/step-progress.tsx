import { Progress as ProgressPrimitive } from "@base-ui/react/progress";

import { ProgressLabel, ProgressValue } from "@/components/ui/progress";
import { cn } from "@/lib/utils";

export interface StepProgressProps {
  className?: string;
  /** Index of the step in progress; earlier steps are done, later ones pending. */
  current: number;
  /** Measured progress of the current step. Without a percent the step shows a sliding block. */
  currentProgress?: StepProgressDetail;
  /** Step labels in order. */
  steps: readonly string[];
}

export interface StepProgressDetail {
  /** Whole percent of the current step, when its total is known. */
  percent?: number;
  /** Text appended to the "Step N of M" value. */
  text: string;
}

/**
 * A progress bar split into one segment per step, for work made of a fixed list of stages. Done
 * segments are filled, the current one fills with measured progress or slides when nothing can be
 * measured, and later ones stay empty. The label names the current step and the value counts the
 * steps.
 */
export function StepProgress({ className, current, currentProgress, steps }: StepProgressProps) {
  const label = steps[current] ?? "";
  const count = `Step ${current + 1} of ${steps.length}`;
  const summary = currentProgress ? `${count} · ${currentProgress.text}` : count;

  return (
    <ProgressPrimitive.Root
      className={cn("flex flex-wrap gap-x-3 gap-y-2", className)}
      data-slot="step-progress"
      getAriaValueText={() => `${label}, ${summary}`}
      max={steps.length}
      min={0}
      value={current}
    >
      {/* Sized like the inspector's detail rows it sits under: the step reads as a value and the
          count as a muted label. */}
      <ProgressLabel className="text-[12.5px]">{label}</ProgressLabel>
      <ProgressValue className="text-xs">{() => summary}</ProgressValue>
      <div className="flex w-full gap-[3px]" data-slot="step-progress-track">
        {steps.map((step, index) => (
          <StepSegment
            key={`${index}-${step}`}
            percent={index === current ? currentProgress?.percent : undefined}
            state={index < current ? "done" : index === current ? "current" : "pending"}
          />
        ))}
      </div>
    </ProgressPrimitive.Root>
  );
}

function StepSegment({
  percent,
  state,
}: {
  percent: number | undefined;
  state: "done" | "current" | "pending";
}) {
  return (
    <div
      className={cn(
        "relative h-1 flex-1 overflow-hidden rounded-full",
        state === "done" ? "bg-primary" : "bg-muted",
      )}
      data-slot="step-progress-segment"
      data-state={state}
    >
      {state === "current" && percent !== undefined ? (
        <div
          className="h-full rounded-full bg-primary transition-[width]"
          data-slot="step-progress-fill"
          style={{ width: `${percent}%` }}
        />
      ) : null}
      {state === "current" && percent === undefined ? (
        <div
          className="absolute inset-y-0 left-0 w-1/3 animate-slide-block rounded-full bg-primary motion-reduce:w-full motion-reduce:animate-none motion-reduce:opacity-50"
          data-slot="step-progress-block"
        />
      ) : null}
    </div>
  );
}
