/** A series of grid steps: a value per step, `null` where absent, and whether each step is open. */
export interface StepSeries {
  provisional?: readonly boolean[];
  values: readonly (number | null)[];
}

/**
 * The current reading of a series: its latest bucket among the last `freshSteps` steps. Open
 * buckets are skipped unless `includeOpen`: a counter's open bucket reads as a rate over the whole
 * step and a distribution's comes from a partial sample, while a gauge's open bucket is a real
 * reading. Nothing within the window means there is no current reading, so an old value never
 * shows as current.
 */
export function currentValue(
  series: StepSeries | undefined,
  freshSteps: number,
  { includeOpen = false }: { includeOpen?: boolean } = {},
): number | undefined {
  if (!series) return undefined;

  const end = series.values.length;
  for (let index = end - 1; index >= Math.max(0, end - freshSteps); index -= 1) {
    const value = series.values[index];
    if (value !== null && (includeOpen || !series.provisional?.[index])) return value;
  }

  return undefined;
}

/** The highest closed reading of a series, the reference a reading with no capacity fills against. */
export function peakValue(series: StepSeries | undefined): number | undefined {
  let peak: number | undefined;
  for (const [index, value] of (series?.values ?? []).entries()) {
    if (value === null || series?.provisional?.[index]) continue;
    peak = peak === undefined ? value : Math.max(peak, value);
  }

  return peak;
}

/** Whether a counter series recorded anything: any bucket, open or closed, with a non-zero rate. */
export function hasActivity(series: StepSeries | undefined): boolean {
  return series?.values.some((value) => value !== null && value > 0) ?? false;
}

/** Share of a reference a reading fills, for a bar; unknown when either is missing or the reference is not positive. */
export function fillFraction(
  value: number | undefined,
  reference: number | undefined,
): number | undefined {
  if (value === undefined || reference === undefined || reference <= 0) return undefined;

  return value / reference;
}
