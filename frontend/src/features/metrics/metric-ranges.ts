import type { MetricResolution } from "@/gen/dotcore/metrics/v1/service_pb";

import { durationMs } from "./metric-grid";

/** A history window the console offers, served at the finest resolution that retains it. */
export interface MetricRange {
  durationMs: number;
  /** Short label such as "15m" or "6h". */
  label: string;
  stepMs: number;
}

/**
 * One range per resolution the store advertises, finest first, each as long as that resolution is
 * retained. The console never hardcodes retention; it offers what the store serves.
 */
export function metricRangesFromResolutions(
  resolutions: readonly MetricResolution[],
): MetricRange[] {
  return resolutions
    .map((resolution) => ({
      durationMs: durationMs(resolution.retention),
      stepMs: durationMs(resolution.step),
    }))
    .filter((range) => range.durationMs > 0 && range.stepMs > 0)
    .map((range) => ({ ...range, label: formatRangeLabel(range.durationMs) }));
}

/** How many steps a card range aims to span; the server rounds the step up to a tier width. */
const CARD_RANGE_STEPS = 60;

/** The card ranges of the served ranges: the same durations, at a sixtieth of each as step hint. */
export function cardRanges(ranges: readonly MetricRange[]): MetricRange[] {
  return ranges.map((range) => ({
    ...range,
    stepMs: Math.ceil(range.durationMs / CARD_RANGE_STEPS),
  }));
}

function formatRangeLabel(rangeMs: number): string {
  const minutes = Math.round(rangeMs / 60_000);
  if (minutes < 60) return `${minutes}m`;

  return `${Math.round(minutes / 60)}h`;
}
