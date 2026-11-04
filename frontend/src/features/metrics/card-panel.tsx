import { MetricCard, MetricCardDetail } from "@/components/metrics/metric-card/metric-card";
import { Sparkline } from "@/components/metrics/sparkline/sparkline";
import { formatChartValueParts } from "@/components/metrics/time-series-chart/chart-data";
import { TimeSeriesChart } from "@/components/metrics/time-series-chart/time-series-chart";
import { cn } from "@/lib/utils";

import type {
  MetricCard as MetricCardModel,
  MetricCardChart,
  MetricCardLine,
  MetricCardUnit,
} from "./use-metric-cards";

/**
 * One card of a card panel as a toggle: its label, value and unit, the line below, and, when asked,
 * a sparkline of its per-step values. Mount it inside a `MetricCardGrid`.
 */
export function ReadingCard({
  card,
  sparkline = false,
}: {
  card: MetricCardModel;
  sparkline?: boolean;
}) {
  const { reading } = card;
  const value = reading?.value;
  const parts =
    value === null || value === undefined ? undefined : formatCardValue(card.unit, value);

  return (
    <MetricCard
      id={card.id}
      label={card.label}
      line={reading ? lineText(reading.line, reading.historyStartMs) : undefined}
      pending={reading === undefined}
      sparkline={
        sparkline ? <Sparkline values={reading?.sparkline ?? []} variant={card.mark} /> : undefined
      }
      unit={parts?.unit}
      value={parts?.value ?? "—"}
    />
  );
}

/** A way to break the selected card's chart down, offered next to the whole subject. */
export interface CardChartSplit {
  /** The accessible name of the split switch. */
  label: string;
  onChange: (splitId: string | undefined) => void;
  options: readonly { label: string; value: string }[];
  /** The chosen split; one not offered here charts the whole subject. */
  value: string | undefined;
}

/**
 * The selected card's chart below its cards, titled after the card, with a switch between the
 * whole subject ("Total") and the offered splits when given.
 */
export function CardChartDetail({
  chart,
  height,
  split,
}: {
  chart: MetricCardChart;
  height: number;
  split?: CardChartSplit;
}) {
  const offered = split?.options.some((option) => option.value === split.value);

  return (
    <MetricCardDetail
      onSplitChange={
        split
          ? (choice: string) => split.onChange(choice === total ? undefined : choice)
          : undefined
      }
      split={offered ? split?.value : total}
      splitLabel={split?.label}
      splitOptions={split ? [{ label: "Total", value: total }, ...split.options] : undefined}
      title={chart.title}
    >
      <TimeSeriesChart
        height={height}
        historyStartMs={chart.historyStartMs}
        label={`${chart.title} history`}
        series={chart.series}
        startMs={chart.startMs}
        stepCount={chart.stepCount}
        stepMs={chart.stepMs}
        unit={chart.unit}
      />
    </MetricCardDetail>
  );
}

/** Says a panel's readings stopped updating while the last ones stay in view; nothing otherwise. */
export function ReadingsStoppedNote({
  className,
  failed,
  subject,
}: {
  className?: string;
  failed: boolean;
  /** What the readings are of, as a sentence start, such as "Request". */
  subject: string;
}) {
  if (!failed) return null;

  return (
    <p className={cn("text-xs text-muted-foreground", className)} role="status">
      {subject} readings stopped updating; showing the last values read.
    </p>
  );
}

/** The split value standing for the whole subject. */
const total = "total";

/** What sits below a value: the card's own figure, else where history starts when it is partial. */
function lineText(line: MetricCardLine | undefined, historyStartMs: number | undefined) {
  if (line) return formatLine(line);
  if (historyStartMs === undefined) return undefined;

  return `since ${new Date(historyStartMs).toLocaleTimeString([], { hour: "2-digit", hourCycle: "h23", minute: "2-digit" })}`;
}

/** A line such as "42% cached" or "10 per request"; empty when there is nothing to relate it to. */
function formatLine(line: MetricCardLine): string | undefined {
  if (line.value === null) return undefined;
  const { unit, value } = formatCardValue(line.unit, line.value);

  return line.unit === "ratio" ? `${value}${unit ?? ""} ${line.label}` : `${value} ${line.label}`;
}

/**
 * A card value as its number and unit, such as "850" and "ms". Counts are compact (1.24K) to fit a
 * narrow card.
 */
function formatCardValue(unit: MetricCardUnit, value: number): { unit?: string; value: string } {
  if (unit === "count") return { value: compactCount.format(value) };
  if (unit === "tokensPerSecond" && value > 0 && value < 1) return slowTokenRate(value);

  return formatChartValueParts(unit, value);
}

const compactCount = new Intl.NumberFormat(undefined, {
  maximumSignificantDigits: 3,
  notation: "compact",
});

/**
 * A token rate under one per second in the unit that reads as a whole number: per minute, or per
 * hour when sparse. A range with a handful of tokens reads "5.48 tokens/min", not "0.0000913".
 */
function slowTokenRate(perSecond: number): { unit: string; value: string } {
  const perMinute = perSecond * 60;
  const [amount, unit] =
    perMinute >= 1 ? [perMinute, "tokens/min"] : [perSecond * 3600, "tokens/h"];

  return {
    unit,
    value: new Intl.NumberFormat(undefined, { maximumSignificantDigits: 3 }).format(amount),
  };
}
