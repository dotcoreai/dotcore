import { createContext, type ReactNode, useContext } from "react";

import { SegmentedControl, type SegmentedControlOption } from "@/components/ui/segmented-control";
import { cn } from "@/lib/utils";

interface MetricCardGridContext {
  onSelectedChange: (id: string | undefined) => void;
  selected: string | undefined;
}

const GridContext = createContext<MetricCardGridContext | undefined>(undefined);

const COLUMN_CLASSES = {
  2: "grid-cols-2",
  3: "grid-cols-3",
  4: "grid-cols-4",
} as const;

/**
 * A grid of metric cards of which at most one is selected. Choosing the selected card again clears
 * the selection. Cards are its direct children and name themselves by id; every row takes the
 * height of the tallest, so all cards share one height.
 */
export function MetricCardGrid({
  children,
  className,
  columns = 3,
  onSelectedChange,
  selected,
}: {
  children: ReactNode;
  className?: string;
  columns?: keyof typeof COLUMN_CLASSES;
  onSelectedChange: (id: string | undefined) => void;
  /** The id of the selected card, if any. */
  selected: string | undefined;
}) {
  return (
    <GridContext.Provider value={{ onSelectedChange, selected }}>
      <div
        className={cn("grid auto-rows-fr gap-2", COLUMN_CLASSES[columns], className)}
        data-slot="metric-card-grid"
      >
        {children}
      </div>
    </GridContext.Provider>
  );
}

/**
 * One metric as a toggle button: a label, a value with its unit, a line below and an optional
 * sparkline. The line keeps its height when empty and the sparkline sits at the bottom, so cards of
 * a grid share one height and their sparklines line up. A pending card shows "—" as its value.
 */
export function MetricCard({
  id,
  label,
  line,
  pending = false,
  selectable = true,
  sparkline,
  unit,
  value,
}: {
  id: string;
  label: string;
  /** A short line below the value, such as an average or a peak; empty when absent. */
  line?: string;
  /** Whether the reading is still unknown; the value shows "—". */
  pending?: boolean;
  /** Whether the card can be selected; a card that cannot is a plain panel outside the grid's selection. */
  selectable?: boolean;
  sparkline?: ReactNode;
  unit?: string;
  /** The formatted value. */
  value: string;
}) {
  const grid = useContext(GridContext);
  const selected = grid?.selected === id;

  const content = (
    <>
      <span
        className="text-[11px] leading-tight text-muted-foreground"
        data-slot="metric-card-label"
        title={label}
      >
        {label}
      </span>
      <span
        className={cn(
          "text-lg font-semibold tabular-nums",
          pending && "font-medium text-muted-foreground",
        )}
        data-slot="metric-card-value"
      >
        {pending ? "—" : value}
        {unit && !pending ? (
          <>
            {" "}
            <small className="text-[11px] font-normal whitespace-nowrap text-muted-foreground">
              {unit}
            </small>
          </>
        ) : null}
      </span>
      <span
        className="min-h-[1lh] truncate text-[11px] text-muted-foreground"
        data-slot="metric-card-line"
        title={line}
      >
        {pending ? undefined : line}
      </span>
      {sparkline ? <div className="mt-auto">{sparkline}</div> : null}
    </>
  );
  const className =
    "flex min-w-0 flex-col gap-0.5 rounded-lg border bg-foreground/4 px-2.5 py-2 text-left";

  if (!selectable) {
    return (
      <div className={className} data-slot="metric-card">
        {content}
      </div>
    );
  }

  return (
    <button
      aria-pressed={selected}
      className={cn(
        className,
        "cursor-pointer outline-none hover:border-chart-1/50 focus-visible:ring-2 focus-visible:ring-ring/50 data-selected:border-chart-1 data-selected:bg-chart-1/12",
      )}
      data-selected={selected}
      data-slot="metric-card"
      onClick={() => grid?.onSelectedChange(selected ? undefined : id)}
      type="button"
    >
      {content}
    </button>
  );
}

/**
 * The detail of the selected card: its title, an optional switch splitting the chart, and the
 * chart itself as children.
 */
export function MetricCardDetail<Split extends string>({
  children,
  className,
  onSplitChange,
  split,
  splitLabel = "Split by",
  splitOptions,
  title,
}: {
  /** The chart. */
  children: ReactNode;
  className?: string;
  onSplitChange?: (split: Split) => void;
  split?: Split;
  /** The accessible name of the split switch. */
  splitLabel?: string;
  /** The ways to split the chart; the switch shows only with two or more. */
  splitOptions?: readonly SegmentedControlOption<Split>[];
  title: string;
}) {
  const splittable = splitOptions !== undefined && splitOptions.length > 1 && onSplitChange;

  return (
    <section className={cn("flex flex-col gap-1", className)} data-slot="metric-card-detail">
      <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-1">
        <h4 className="text-[13px] font-semibold">{title}</h4>
        {splittable ? (
          <SegmentedControl
            aria-label={splitLabel}
            onValueChange={onSplitChange}
            options={splitOptions}
            value={split}
          />
        ) : null}
      </div>
      {children}
    </section>
  );
}
