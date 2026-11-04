import type { MetricStatsGrid, MetricStepStats } from "./metric-grid";

/** A gauge over a range: the mean of its readings and the highest; `null` without a reading. */
export interface GaugeRange {
  average: number | null;
  peak: number | null;
}

/** The time a grid really covers, once clipped at the start of its store incarnation. */
export interface CoveredTime {
  ms: number;
  /** Whether the incarnation began inside the range, so history starts after the range does. */
  startsInside: boolean;
}

/**
 * Sums the counter deltas of every step, the provisional one included. Zero when the counter was
 * idle, `null` when no step holds a counter reading, which is "no data" rather than zero.
 */
export function counterTotal(steps: readonly (MetricStepStats | null)[]): number | null {
  let total: number | null = null;

  for (const step of steps) {
    if (step?.kind === "counter") total = (total ?? 0) + step.delta;
  }

  return total;
}

/**
 * The mean of all samples in the range: the summed sums over the summed counts, so a busy step
 * weighs more than a quiet one. `null` when the range holds no sample.
 */
export function weightedMean(steps: readonly (MetricStepStats | null)[]): number | null {
  let count = 0;
  let sum = 0;

  for (const step of steps) {
    if (step?.kind !== "distribution") continue;
    count += step.count;
    sum += step.sum;
  }

  return count > 0 ? sum / count : null;
}

/**
 * A gauge over the range from per-step values: the average of its per-step means, and the highest
 * of its per-step highs. A coarse step holds many readings, so its mean and its high come from
 * separately reduced series; without highs the means stand in. Gaps are skipped.
 */
export function gaugeRange(
  means: readonly (number | null)[],
  highs: readonly (number | null)[] = means,
): GaugeRange {
  const present = (values: readonly (number | null)[]) =>
    values.filter((value): value is number => value !== null);
  const averages = present(means);
  const peaks = present(highs);

  return {
    average: averages.length
      ? averages.reduce((total, value) => total + value, 0) / averages.length
      : null,
    peak: peaks.length ? Math.max(...peaks) : null,
  };
}

/**
 * The range's duration as the store can vouch for it: from the later of range and incarnation
 * start, up to now. The last step is still open and ends ahead of the clock, so the range ends at
 * `nowMs` when that comes first.
 */
export function coveredTime(grid: MetricStatsGrid, nowMs = Date.now()): CoveredTime {
  const endMs = Math.min(grid.startMs + grid.stepCount * grid.stepMs, nowMs);
  const startsInside = grid.incarnation.startedAtMs > grid.startMs;
  const fromMs = startsInside ? grid.incarnation.startedAtMs : grid.startMs;

  return { ms: Math.max(0, endMs - fromMs), startsInside };
}

/**
 * One display value per step for sparklines and charts, gaps kept as `null`: the gauge reading,
 * the counter delta, or the mean of the step's samples.
 */
export function perStepValues(steps: readonly (MetricStepStats | null)[]): (number | null)[] {
  return steps.map((step) => {
    switch (step?.kind) {
      case "counter":
        return step.delta;
      case "distribution":
        return step.count > 0 ? step.sum / step.count : null;
      case "gauge":
        return step.value;
      default:
        return null;
    }
  });
}
