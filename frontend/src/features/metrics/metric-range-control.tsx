import { SegmentedControl } from "@/components/ui/segmented-control";

import type { MetricCards } from "./use-metric-cards";

/**
 * A page's range choice for its metric cards, one option per range the store serves. It shows
 * nothing while fewer than two ranges are served, since there is nothing to choose.
 */
export function MetricRangeControl({
  label,
  metrics,
}: {
  /** The control's accessible name, such as the page's name and "range". */
  label: string;
  metrics: Pick<MetricCards, "range" | "ranges" | "setRange">;
}) {
  if (metrics.ranges.length < 2) return null;

  return (
    <SegmentedControl
      aria-label={label}
      onValueChange={metrics.setRange}
      options={metrics.ranges.map((range) => ({ label: range.label, value: range.label }))}
      value={metrics.range?.label}
    />
  );
}
