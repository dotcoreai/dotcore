import {
  type ChartSeries,
  type ChartUnit,
  formatChartValue,
  formatChartValueParts,
} from "@/components/metrics/time-series-chart/chart-data";
import { machineName } from "@/features/memory/described-machines";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import { gaugeRange } from "@/features/metrics/metric-range-values";
import type {
  MetricCardReading,
  MetricCardSpec,
  MetricCardSplit,
} from "@/features/metrics/use-metric-cards";
import type { MetricAnswer, MetricHistoryQuery } from "@/features/metrics/use-metric-watch";
import { MetricAggregate, MetricReduce } from "@/gen/dotcore/metrics/v1/metric_pb";
import type { Machine } from "@/gen/dotcore/site/v1/service_pb";

import {
  type MachineFacts,
  type MachineGrids,
  type MachineMetric,
  type MachineNow,
  type MachineSignal,
  machineSignal,
  noMachineFacts,
  siteSignal,
  type Steps,
} from "./machine-site-values";

/** The split id and card ids of the machines section. */
export const machineSplitId = "machine";

/** The machine card ids in display order: each processor beside its memory, then GPU health, then storage. */
export const machineCardIds = [
  "cpu",
  "ram",
  "gpu",
  "vram",
  "gpu-power",
  "gpu-temperature",
  "storage",
] as const;

type MachineCardId = (typeof machineCardIds)[number];

/**
 * The machine metrics the overview reads, each per machine (and per GPU for memory), in the order
 * the "now" stream asks for them. GPU utilization is averaged over a machine's GPUs, temperature is
 * its hottest GPU, storage its largest filesystem; every other reading is summed. A bucket reduces
 * to its latest reading unless a reader asks otherwise.
 */
export const machineMetrics: readonly MachineMetric[] = [
  "cpu",
  "gpu",
  "power",
  "temperature",
  "ramTotal",
  "ramAvailable",
  "vramUsed",
  "vramTotal",
  "storageSize",
  "storageUsed",
];

export const machineQueries: Readonly<Record<MachineMetric, MetricHistoryQuery>> = (() => {
  const query = (
    metric: string,
    groupBy: readonly string[],
    aggregate = MetricAggregate.SUM,
  ): MetricHistoryQuery => ({ aggregate, groupBy, matchers: {}, metric });
  const node = [dimensionNames.node];
  const device = [dimensionNames.node, dimensionNames.device];

  return {
    cpu: query(metricNames.nodeHostCpuUtilization, node, MetricAggregate.AVG),
    gpu: query(metricNames.nodeAcceleratorUtilization, node, MetricAggregate.AVG),
    power: query(metricNames.nodeAcceleratorPower, node),
    ramAvailable: query(metricNames.nodeHostMemoryAvailable, node),
    ramTotal: query(metricNames.nodeHostMemoryTotal, node),
    storageSize: query(metricNames.nodeFilesystemSize, node, MetricAggregate.MAX),
    storageUsed: query(metricNames.nodeFilesystemUsed, node, MetricAggregate.MAX),
    temperature: query(metricNames.nodeAcceleratorTemperature, node, MetricAggregate.MAX),
    vramTotal: query(metricNames.nodeAcceleratorMemoryTotal, device),
    vramUsed: query(metricNames.nodeAcceleratorMemoryUsed, device),
  };
})();

/** A machine card's headline: the value and unit, and the line below. */
export interface MachineHeadline {
  line?: string;
  unit?: string;
  value: string;
}

/** What a machine card's headline reads besides its own reading over the range. */
export interface MachineHeadlineContext {
  facts: MachineFacts;
  /** Whether the machines' hardware facts are still being read. */
  factsPending: boolean;
  machineCount: number;
  /** The site's newest closed readings; absent until the "now" stream answers. */
  now: MachineNow | undefined;
}

/**
 * The seven machine cards the overview reads over the range (the eighth, machines connected, is a
 * count of the resource watch). Each card reads its metrics per machine, so its by-machine chart
 * needs no query of its own. Its value is the range average, from buckets reduced to their mean,
 * and its line the range peak, from buckets reduced to their extreme; the section shows the newest
 * reading from the "now" stream as the headline. CPU is weighted by logical CPUs, GPU by GPUs,
 * power is summed, temperature is the hottest GPU, RAM is used host memory, VRAM counts dedicated
 * GPU memory only and storage is the used and size of each machine's largest filesystem, summed.
 */
export function machineCardSpecs(facts: MachineFacts = noMachineFacts): MetricCardSpec[] {
  return machineCards.map((card) => machineCardSpec(card, facts));
}

/** A machine card's headline from the site's newest reading and the range's average and peak. */
export function machineCardHeadline(
  cardId: string,
  reading: MetricCardReading | undefined,
  context: MachineHeadlineContext,
): MachineHeadline {
  const card = machineCards.find((entry) => entry.id === cardId);
  return card ? card.headline(reading, context) : { value: "—" };
}

/** The by-machine split of the machine cards, naming each group like the console's machines. */
export function machineSplit(machines: readonly Machine[] = []): MetricCardSplit {
  const names = new Map(machines.map((machine) => [machine.machineId, machineName(machine)]));

  return {
    groupBy: [dimensionNames.node],
    id: machineSplitId,
    label: "By machine",
    seriesLabel: (labels) =>
      names.get(labels[dimensionNames.node] ?? "") ?? labels[dimensionNames.node] ?? "",
  };
}

/** A reading in its unit, such as "45%" or "150 W"; "—" when absent. */
export function formatReading(unit: ChartUnit, value: number | undefined): string {
  return value === undefined ? "—" : formatChartValue(unit, value);
}

/** Used against total memory as "12.3 / 64 GiB" over one unit, or in full when the units differ. */
export function formatUsed(used: number | undefined, total: number | undefined): string {
  const { unit, value } = usedHeadline(used, total);
  return unit ? `${value} ${unit}` : value;
}

/**
 * One machine card: the signal it shows, the metrics that signal reads, the extremes its peak
 * reads them at, and its headline wording.
 */
interface MachineCard {
  /** The signal charted dashed behind the value, such as total memory. */
  capacity?: { label: string; signal: MachineSignal };
  headline: (
    reading: MetricCardReading | undefined,
    context: MachineHeadlineContext,
  ) => MachineHeadline;
  id: MachineCardId;
  label: string;
  /** The label of the value's line on the chart. */
  lineLabel: string;
  /** The metrics read at their per-bucket mean, for the value. */
  metrics: readonly MachineMetric[];
  /**
   * The metrics read again at a per-bucket extreme for the peak, such as the highest utilization or
   * the lowest available memory; the others keep their mean. No peak when empty.
   */
  peak: Partial<Record<MachineMetric, MetricReduce>>;
  /** The label of a peak line on the chart; the chart shows no peak without one. */
  peakLineLabel?: string;
  signal: MachineSignal;
  unit: Exclude<ChartUnit, "requests" | "seconds" | "tokensPerSecond">;
}

const machineCards: readonly MachineCard[] = [
  {
    headline: (reading, { now }) => averageAndPeak("ratio", reading, now?.cpu),
    id: "cpu",
    label: "CPU usage",
    lineLabel: "CPU",
    metrics: ["cpu"],
    peak: { cpu: MetricReduce.MAX },
    signal: "cpu",
    unit: "ratio",
  },
  {
    capacity: { label: "Total", signal: "ramTotal" },
    headline: (reading, { now }) => ({
      ...usedHeadline(now?.ramUsed, now?.ramTotal),
      line: reading ? `peak ${peakText("bytes", reading)}` : undefined,
    }),
    id: "ram",
    label: "RAM",
    lineLabel: "Used",
    metrics: ["ramTotal", "ramAvailable"],
    peak: { ramAvailable: MetricReduce.MIN },
    signal: "ramUsed",
    unit: "bytes",
  },
  {
    headline: (reading, { now }) => averageAndPeak("ratio", reading, now?.gpu),
    id: "gpu",
    label: "GPU usage",
    lineLabel: "Average",
    metrics: ["gpu"],
    peak: { gpu: MetricReduce.MAX },
    peakLineLabel: "Peak",
    signal: "gpu",
    unit: "ratio",
  },
  {
    capacity: { label: "Total", signal: "vramTotal" },
    headline: (reading, { facts, factsPending, now }) => {
      if (factsPending) return { value: "—" };
      if (!facts.dedicated.size) return { line: "unified memory, see RAM", value: "—" };

      return {
        ...usedHeadline(now?.vramUsed, now?.vramTotal),
        line: reading ? `peak ${peakText("bytes", reading)}` : undefined,
      };
    },
    id: "vram",
    label: "VRAM",
    lineLabel: "Used",
    metrics: ["vramUsed", "vramTotal"],
    peak: { vramUsed: MetricReduce.MAX },
    signal: "vramUsed",
    unit: "bytes",
  },
  {
    headline: (reading, { now }) => averageAndPeak("watts", reading, now?.power),
    id: "gpu-power",
    label: "GPU power",
    lineLabel: "GPU power",
    metrics: ["power"],
    peak: { power: MetricReduce.MAX },
    signal: "power",
    unit: "watts",
  },
  {
    headline: (reading, { machineCount, now }) => ({
      ...headlineParts("celsius", now?.temperature),
      line: reading
        ? `${machineCount > 1 ? "hottest GPU · " : ""}peak ${peakText("celsius", reading)}`
        : undefined,
    }),
    id: "gpu-temperature",
    label: "GPU temperature",
    lineLabel: "Hottest GPU",
    metrics: ["temperature"],
    peak: { temperature: MetricReduce.MAX },
    signal: "temperature",
    unit: "celsius",
  },
  {
    capacity: { label: "Total", signal: "storageSize" },
    // Used storage, against the size of each machine's largest filesystem.
    headline: (reading, { now }) => ({
      ...usedHeadline(now?.storageUsed, now?.storageSize),
      line: reading ? `peak ${peakText("bytes", reading)}` : undefined,
    }),
    id: "storage",
    label: "Storage",
    lineLabel: "Used",
    metrics: ["storageUsed", "storageSize"],
    peak: { storageUsed: MetricReduce.MAX },
    signal: "storageUsed",
    unit: "bytes",
  },
];

function machineCardSpec(card: MachineCard, facts: MachineFacts): MetricCardSpec {
  const peaks = Object.entries(card.peak) as [MachineMetric, MetricReduce][];
  const means = (answers: readonly MetricAnswer[]) => gridsOf(card.metrics, answers);
  const highs = (answers: readonly MetricAnswer[]): MachineGrids => ({
    ...means(answers),
    ...gridsOf(
      peaks.map(([metric]) => metric),
      answers.slice(card.metrics.length),
    ),
  });

  return {
    chart: (answers) => {
      const grids = means(answers);
      return [
        line(card.signal, card.lineLabel, "chart-1", siteSignal(card.signal, grids, facts)),
        ...(card.peakLineLabel
          ? [
              line(
                "peak",
                card.peakLineLabel,
                "chart-4",
                siteSignal(card.signal, highs(answers), facts),
              ),
            ]
          : []),
        ...(card.capacity
          ? [
              dashed(
                card.capacity.signal,
                card.capacity.label,
                siteSignal(card.capacity.signal, grids, facts),
              ),
            ]
          : []),
      ];
    },
    id: card.id,
    label: card.label,
    mark: "line",
    queries: [
      ...card.metrics.map((metric) => ({ ...machineQueries[metric], reduce: MetricReduce.AVG })),
      ...peaks.map(([metric, reduce]) => ({ ...machineQueries[metric], reduce })),
    ],
    read: (answers) => {
      const steps = siteSignal(card.signal, means(answers), facts);
      const range = gaugeRange(
        steps?.values ?? [],
        peaks.length ? (siteSignal(card.signal, highs(answers), facts)?.values ?? []) : undefined,
      );

      return {
        line: peaks.length ? { label: "peak", unit: card.unit, value: range.peak } : undefined,
        sparkline: steps?.values ?? [],
        value: range.average,
      };
    },
    splitChart: (answers, split) =>
      machineLines(machineSignal(card.signal, means(answers), facts), split),
    splits: [machineSplitId],
    unit: card.unit,
  };
}

/** The answers of the given metrics, in order, by metric. */
function gridsOf(
  metrics: readonly MachineMetric[],
  answers: readonly MetricAnswer[],
): MachineGrids {
  return Object.fromEntries(metrics.map((metric, index) => [metric, answers[index]?.stats]));
}

/** A headline of the newest reading over "avg … · peak …" of the range. */
function averageAndPeak(
  unit: ChartUnit,
  reading: MetricCardReading | undefined,
  now: number | undefined,
): MachineHeadline {
  return {
    ...headlineParts(unit, now),
    line: reading
      ? `avg ${formatReading(unit, reading.value ?? undefined)} · peak ${peakText(unit, reading)}`
      : undefined,
  };
}

function peakText(unit: ChartUnit, reading: MetricCardReading): string {
  return formatReading(unit, reading.line?.value ?? undefined);
}

function headlineParts(unit: ChartUnit, value: number | undefined): MachineHeadline {
  return value === undefined ? { value: "—" } : formatChartValueParts(unit, value);
}

/** Used against total memory as "12.3 / 64" over one unit, or in full when the units differ. */
function usedHeadline(used: number | undefined, total: number | undefined): MachineHeadline {
  if (used === undefined || total === undefined) return { value: "—" };

  const usedParts = formatChartValueParts("bytes", used);
  const totalParts = formatChartValueParts("bytes", total);

  return usedParts.unit === totalParts.unit
    ? { unit: totalParts.unit, value: `${usedParts.value} / ${totalParts.value}` }
    : { value: `${formatChartValue("bytes", used)} / ${formatChartValue("bytes", total)}` };
}

function line(
  id: string,
  label: string,
  color: ChartSeries["color"],
  steps: Steps | undefined,
): ChartSeries {
  return {
    color,
    id,
    label,
    mark: "line",
    provisional: steps?.provisional,
    values: steps?.values ?? [],
  };
}

function dashed(id: string, label: string, steps: Steps | undefined): ChartSeries {
  return { ...line(id, label, "muted", steps), dashed: true };
}

/** One line per machine in its marker color. */
function machineLines(machines: ReadonlyMap<string, Steps>, split: MetricCardSplit): ChartSeries[] {
  return [...machines].map(([machine, steps]) =>
    line(
      machine,
      split.seriesLabel?.({ [dimensionNames.node]: machine }) ?? machine,
      { resource: machine },
      steps,
    ),
  );
}
