import { ChevronRightIcon } from "lucide-react";
import { type ReactNode, useId } from "react";

import {
  type SeriesColor,
  seriesColorValue,
} from "@/components/metrics/time-series-chart/chart-data";
import { ResourceMarker } from "@/components/ui/resource-marker";
import { cn } from "@/lib/utils";
import { useResourceColors } from "@/theme/resource-color-context";

/**
 * A list of metric rows whose labels, bars and values line up in three shared columns. The label
 * column has a fixed width, so bars line up across lists in the same panel, or fits its longest
 * label for a list standing alone. The value column is wide enough for common readouts, so bars
 * keep their width as live values change. Rows and their expanded bodies are its direct children.
 */
export function MetricRows({
  children,
  className,
  fitLabels = false,
}: {
  children: ReactNode;
  className?: string;
  /** Whether the label column fits its longest label instead of the fixed width. */
  fitLabels?: boolean;
}) {
  return (
    <div
      className={cn(
        "grid items-center gap-x-3 gap-y-1.5 text-xs",
        fitLabels
          ? "grid-cols-[auto_minmax(0,1fr)_minmax(6.5rem,auto)]"
          : "grid-cols-[8.5rem_minmax(0,1fr)_minmax(6.5rem,auto)]",
        className,
      )}
      data-slot="metric-rows"
    >
      {children}
    </div>
  );
}

/**
 * One metric as `label | bar | value`, with its history below while expanded. The expand control
 * is a button holding the chevron and the label; the bar and the value sit beside it, so they can
 * carry their own tooltips. A click elsewhere on the row also toggles it, since pointer users aim
 * at the bar. A row without an expanded body is a plain line. The value is text and carries the
 * reading; the bar only illustrates it.
 */
export function MetricRow({
  bar,
  children,
  expanded,
  label,
  onExpandedChange,
  value,
}: {
  bar: ReactNode;
  /** The expanded body, such as the metric's history chart; a row without one doesn't expand. */
  children?: ReactNode;
  expanded: boolean;
  label: string;
  onExpandedChange: (expanded: boolean) => void;
  value: ReactNode;
}) {
  const bodyId = useId();
  const expandable = children !== undefined && children !== null;
  const labelText = <span className="truncate text-muted-foreground">{label}</span>;

  if (!expandable) {
    return (
      <div className="col-span-3 grid grid-cols-subgrid items-center" data-slot="metric-row">
        {/* The empty chevron slot keeps the label aligned with expandable rows. */}
        <div
          className="flex min-h-control-xs min-w-0 items-center gap-1 pl-4"
          data-slot="metric-row-label"
          title={label}
        >
          {labelText}
        </div>
        <div className="min-w-0">{bar}</div>
        <div className="justify-self-end whitespace-nowrap tabular-nums">{value}</div>
      </div>
    );
  }

  return (
    <>
      {/* The button is the keyboard path; the pseudo-element stretches the click area over half
          the row gap on each side, behind the row's own content. */}
      {/* eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-static-element-interactions */}
      <div
        className="relative isolate col-span-3 grid grid-cols-subgrid items-center after:absolute after:inset-x-0 after:-inset-y-0.75 after:-z-1 hover:after:rounded-md hover:after:bg-muted/60"
        data-expanded={expanded}
        data-slot="metric-row"
        onClick={(event) => {
          // Clicks inside portaled tooltips bubble here through React, so only the row's own
          // elements count, and the button toggles the row itself. A keyboard press on another
          // control in the row, such as a readout, is that control's own action.
          const target = event.target;
          if (
            event.detail === 0 ||
            !(target instanceof Element) ||
            !event.currentTarget.contains(target) ||
            target.closest("[data-slot=metric-row-toggle]")
          ) {
            return;
          }
          onExpandedChange(!expanded);
        }}
      >
        <button
          aria-controls={expanded ? bodyId : undefined}
          aria-expanded={expanded}
          className="flex min-h-control-xs min-w-0 items-center gap-1 rounded-sm text-left outline-none focus-visible:ring-2 focus-visible:ring-ring/50"
          data-slot="metric-row-toggle"
          onClick={() => onExpandedChange(!expanded)}
          title={label}
          type="button"
        >
          <ChevronRightIcon
            aria-hidden="true"
            className={cn(
              "size-3 shrink-0 text-muted-foreground transition-transform motion-reduce:transition-none",
              expanded && "rotate-90",
            )}
          />
          {labelText}
        </button>
        <div className="min-w-0">{bar}</div>
        <div className="justify-self-end whitespace-nowrap tabular-nums">{value}</div>
      </div>
      {expanded ? (
        <div className="col-span-3 pt-1 pb-2" data-slot="metric-row-body" id={bodyId}>
          {children}
        </div>
      ) : null}
    </>
  );
}

/**
 * A metric row with no reading, while it loads or when there is nothing to read: its label, an
 * empty bar, and "—". It holds the row's place, so the list keeps its size when a real row
 * replaces it. Given an expanded body, it expands like the real row, so its control never appears
 * later.
 */
export function PendingMetricRow({
  children,
  expanded = false,
  label,
  names = false,
  onExpandedChange = () => {},
}: {
  /** The expanded body, such as an empty history chart; without one the row doesn't expand. */
  children?: ReactNode;
  expanded?: boolean;
  label: string;
  /** Whether the real row names its parts under the bar, as `MetricSpans` does; keeps that line. */
  names?: boolean;
  onExpandedChange?: (expanded: boolean) => void;
}) {
  return (
    <MetricRow
      bar={
        <>
          <MetricBar fraction={undefined} />
          {names ? <PendingNamesLine /> : null}
        </>
      }
      expanded={expanded}
      label={label}
      onExpandedChange={onExpandedChange}
      value="—"
    >
      {children}
    </MetricRow>
  );
}

/** The empty place of the names line `MetricSpans` draws under its bars. */
export function PendingNamesLine() {
  return (
    <p aria-hidden="true" className="mt-1 text-[11px]">
      &nbsp;
    </p>
  );
}

/**
 * A plain fill bar for a reading against a capacity or reference; an unknown fill is an empty
 * track. It is hidden from assistive technology, since the row's value states the reading.
 */
export function MetricBar({
  color = "chart-1",
  fraction,
}: {
  color?: SeriesColor;
  /** Share of the bar to fill, clamped to 0 to 1; `undefined` when there is no current reading. */
  fraction: number | undefined;
}) {
  const { colorOf } = useResourceColors();
  const width = fraction === undefined ? 0 : Math.min(1, Math.max(0, fraction));

  return (
    <div
      aria-hidden="true"
      className="flex h-2.5 overflow-hidden rounded-full bg-foreground/10"
      data-slot="metric-bar"
    >
      {width > 0 ? (
        <div
          className="h-full rounded-full"
          data-slot="metric-bar-fill"
          style={{ backgroundColor: seriesColorValue(color, colorOf), width: `${width * 100}%` }}
        />
      ) : null}
    </div>
  );
}

/** One resource's part of a split bar, such as one machine's share of a row. */
export interface MetricSpan {
  /** The bar drawn for this part, such as a `MetricBar` against the part's own capacity. */
  bar: ReactNode;
  /** The resource's identity marker shown before its name. */
  colorKey?: string;
  key: string;
  name: string;
  /** Text after the name, such as the part's own reading. */
  value?: string;
  /** The part's share of the row's width, such as its capacity. */
  weight: number;
}

/**
 * A bar split into one bar per resource, each sized by its weight and named underneath with its
 * marker, so a row reads per machine while keeping its place in a list of rows.
 */
export function MetricSpans({ spans }: { spans: readonly MetricSpan[] }) {
  return (
    <div className="flex gap-1" data-slot="metric-spans">
      {spans.map((span) => (
        <div
          className="min-w-0"
          data-slot="metric-span"
          key={span.key}
          // A zero weight still keeps the part visible, at the smallest share.
          style={{ flex: `${span.weight || 1} 1 0%` }}
        >
          {span.bar}
          <p
            className="mt-1 truncate text-[11px] text-muted-foreground"
            title={span.value ? `${span.name} ${span.value}` : span.name}
          >
            {span.colorKey ? (
              <ResourceMarker className="mr-1 size-2" colorKey={span.colorKey} />
            ) : null}
            {span.name}
            {span.value ? ` ${span.value}` : null}
          </p>
        </div>
      ))}
    </div>
  );
}
