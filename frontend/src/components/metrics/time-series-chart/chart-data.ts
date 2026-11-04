import { formatBytesParts } from "@/lib/format";

const secondsIncrements = [
  ...[1e-4, 1e-3, 1e-2, 1e-1, 1, 10, 100].flatMap((base) =>
    [1, 2, 2.5, 5].map((multiple) => multiple * base),
  ),
  300,
  600,
  3600,
];

/** Unit of a chart's values; decides value text and the y-axis tick steps. */
export type ChartUnit =
  "bytes" | "celsius" | "ratio" | "requests" | "seconds" | "tokensPerSecond" | "watts";

/** Token color of a series, resolved from the console theme so charts follow light and dark. */
export type ChartColor = "chart-1" | "chart-2" | "chart-3" | "chart-4" | "chart-5" | "muted";

/**
 * The color of a line or bar: a theme token, or the identity marker color of the one resource it
 * stands for, such as a machine, so the line matches that resource's marker everywhere else.
 */
export type SeriesColor = ChartColor | { resource: string };

/**
 * How a series is drawn: a line through its points, one bar per step, or one bar per step stacked
 * on the stacked bars before it in the chart's series order.
 */
export type SeriesMark = "bars" | "line" | "stackedBars";

/** One line or set of bars on a time-series chart, one value per grid step; `null` is absent. */
export interface ChartSeries {
  color: SeriesColor;
  dashed?: boolean;
  id: string;
  label: string;
  /** Drawn as a line when unset. */
  mark?: SeriesMark;
  /** Marks points whose bucket is still open; they are drawn dashed apart from closed ones. */
  provisional?: readonly boolean[];
  values: readonly (number | null)[];
}

/**
 * A dashed, muted line for the capacity the chart's other lines are read against, such as a disk's
 * size. A single number draws a constant line over `stepCount` steps.
 */
export function capacitySeries(
  values: number | readonly (number | null)[],
  stepCount: number,
  provisional?: readonly boolean[],
): ChartSeries {
  return {
    color: "muted",
    dashed: true,
    id: "capacity",
    label: "Capacity",
    provisional,
    values: typeof values === "number" ? Array.from({ length: stepCount }, () => values) : values,
  };
}

/**
 * A CSS color for a series color: the token's custom property, or the resource's marker color
 * resolved by `colorOf`.
 */
export function seriesColorValue(color: SeriesColor, colorOf: (key: string) => string): string {
  if (typeof color === "object") return colorOf(color.resource);

  return `var(--${color === "muted" ? "muted-foreground" : color})`;
}

/**
 * uPlot column data: epoch-second x values, then one column per closed series and per overlay. A
 * stacked bar column holds the top of each bar, and its base, the value the bar rises from, sits
 * at the same position in `bases`; other columns have no base.
 */
export interface ChartData {
  bases: (number[] | null)[];
  columns: (number | null)[][];
  x: number[];
}

/** Formats one value of the unit for readouts. */
export function formatChartValue(unit: ChartUnit, value: number): string {
  const parts = formatChartValueParts(unit, value);
  if (parts.unit === undefined) return parts.value;

  return parts.unit === "%" ? `${parts.value}%` : `${parts.value} ${parts.unit}`;
}

/**
 * `formatChartValue` as its number and its unit, such as "850" and "ms", for readouts that set the
 * unit apart. A count has no unit.
 */
export function formatChartValueParts(
  unit: ChartUnit,
  value: number,
): { unit?: string; value: string } {
  if (!Number.isFinite(value)) return { value: "No data" };

  switch (unit) {
    case "bytes":
      return formatBytesParts(BigInt(Math.max(0, Math.round(value))));
    case "celsius":
      return { unit: "°C", value: formatWhole(value) };
    case "ratio":
      return { unit: "%", value: formatWhole(value * 100) };
    case "requests":
      return { value: formatCount(value) };
    case "seconds":
      return formatSeconds(value);
    case "tokensPerSecond":
      return { unit: "tokens/s", value: formatTokenRate(value) };
    case "watts":
      return { unit: "W", value: formatWhole(value) };
  }
}

/** Formats one y-axis tick; a token rate drops its suffix, which the chart title carries. */
export function formatChartAxisValue(unit: ChartUnit, value: number): string {
  return unit === "tokensPerSecond" ? formatTokenRate(value) : formatChartValue(unit, value);
}

/** Tick spacings in the unit, so axis labels land on round numbers such as 20 GiB. */
export function chartAxisIncrements(unit: ChartUnit): number[] {
  switch (unit) {
    case "bytes":
      return Array.from({ length: 6 }, (_, power) =>
        [1, 2, 5, 10, 20, 50, 100, 200, 500].map((multiple) => multiple * 1024 ** power),
      ).flat();
    case "celsius":
      return [1, 2, 5, 10, 20, 25, 50];
    case "ratio":
      return [0.01, 0.02, 0.05, 0.1, 0.2, 0.25, 0.5, 1];
    case "requests":
      return countIncrements;
    case "seconds":
      return secondsIncrements;
    case "tokensPerSecond":
      return countIncrements;
    case "watts":
      return [1, 2, 5, 10, 20, 50, 100, 200, 500, 1000];
  }
}

/**
 * Lays the series out as uPlot columns: every closed point in its series column, and every open
 * (provisional) point in a trailing overlay column. An open line point is joined to the preceding
 * closed point so the dashed segment connects to the solid line; an open bar stands alone. Stacked
 * bars hold their running totals. Absent points stay `null`, never zero. The time axis spans the
 * longest series, and at least `stepCount` steps, so a chart without data still spans its window.
 */
export function buildChartData(
  startMs: number,
  stepMs: number,
  series: readonly ChartSeries[],
  stepCount = 0,
): ChartData {
  const length = series.reduce(
    (longest, entry) => Math.max(longest, entry.values.length),
    stepCount,
  );
  const x = Array.from({ length }, (_, index) => (startMs + index * stepMs) / 1000);
  const layers = stackLayers(series);
  const closed = series.map((entry, position) =>
    layers[position].values.map((value, index) => (entry.provisional?.[index] ? null : value)),
  );
  const open = series.map((entry, position) => {
    const values = layers[position].values;

    return values.map((value, index) => {
      if (value === null) return null;
      if (entry.provisional?.[index]) return value;
      if (entry.mark === "bars" || entry.mark === "stackedBars") return null;

      const next = entry.provisional?.[index + 1] ? values[index + 1] : null;
      return next !== null && next !== undefined ? value : null;
    });
  });
  const bases = layers.map((layer) => layer.base);

  return { bases: [...bases, ...bases], columns: [...closed, ...open], x };
}

/**
 * The values each series draws: stacked bars hold the running total of the stacked bars up to
 * and including them, with the total below them as their base; other series keep their own values.
 * An absent stacked bar adds nothing to the total.
 */
function stackLayers(
  series: readonly ChartSeries[],
): { base: number[] | null; values: readonly (number | null)[] }[] {
  const totals: number[] = [];

  return series.map((entry) => {
    if (entry.mark !== "stackedBars") return { base: null, values: entry.values };

    const base = entry.values.map((_, index) => totals[index] ?? 0);
    const values = entry.values.map((value, index) => {
      if (value === null) return null;

      totals[index] = base[index] + value;
      return totals[index];
    });
    return { base, values };
  });
}

const countIncrements = [1e-3, 1e-2, 1e-1, 1, 10, 100, 1e3, 1e4, 1e5, 1e6].flatMap((base) =>
  [1, 2, 2.5, 5].map((multiple) => multiple * base),
);

/** A count or rate with up to three significant digits, such as 0.25 or 1,240. */
function formatCount(value: number): string {
  return new Intl.NumberFormat(undefined, { maximumSignificantDigits: 3 }).format(value);
}

/**
 * A whole number, such as 46 or 1,235. Hardware readings change every second, and their fractions
 * carry no meaning while making live readouts change width.
 */
function formatWhole(value: number): string {
  return new Intl.NumberFormat(undefined, { maximumFractionDigits: 0 }).format(value);
}

/** A token rate with up to three significant digits and a k or M suffix, such as 850 or 1.2k. */
function formatTokenRate(value: number): string {
  const number = (amount: number) =>
    new Intl.NumberFormat(undefined, { maximumSignificantDigits: 3 }).format(amount);

  if (value >= 1e6) return `${number(value / 1e6)}M`;
  if (value >= 1e3) return `${number(value / 1e3)}k`;

  return number(value);
}

/** A duration in the largest fitting unit, such as 850 ms, 1.2 s or 3.5 min. */
function formatSeconds(value: number): { unit: string; value: string } {
  const number = (amount: number) =>
    new Intl.NumberFormat(undefined, { maximumSignificantDigits: 3 }).format(amount);

  if (value < 1e-3) return { unit: "µs", value: number(value * 1e6) };
  if (value < 1) return { unit: "ms", value: number(value * 1e3) };
  if (value < 60) return { unit: "s", value: number(value) };

  return { unit: "min", value: number(value / 60) };
}

/** A clock time with seconds, preceded by the day and month when `withDate`. */
export function formatTime(epochMs: number | undefined, withDate: boolean): string {
  if (epochMs === undefined) return "";

  return new Date(epochMs).toLocaleString(undefined, {
    ...(withDate ? { day: "numeric", month: "short" } : {}),
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}
