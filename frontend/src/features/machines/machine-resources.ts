import type { ChartSeries } from "@/components/metrics/time-series-chart/chart-data";
import { nodeConnectionLabel } from "@/domain/execution-target";
import { machineName } from "@/domain/machine-name";
import type { DescribedMachine } from "@/features/memory/described-machines";
import { memoryQueryGroups } from "@/features/memory/memory-history";
import { acceleratorFacts } from "@/features/memory/snapshot-facts";
import type { MachineSnapshotDetail } from "@/features/memory/use-hardware-snapshots";
import {
  alignedSeries,
  deviceSeries,
  type HistoryBreakdown,
  nodeSeries,
  type StepValues,
  usedAgainstCapacity,
} from "@/features/metrics/history-series";
import { currentValue } from "@/features/metrics/metric-current";
import type { MetricGrid, MetricGridSeries } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import type { MetricQueryGroup } from "@/features/metrics/use-metric-panel";
import type { MetricHistoryQuery } from "@/features/metrics/use-metric-watch";
import {
  AcceleratorSignal,
  type HardwareSnapshot,
  type HostFacts,
} from "@/gen/dotcore/observation/v1/observation_pb";
import { type Machine, NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";

/** A listed machine left out of the overview's current values, with its state in a word. */
export interface UncountedMachine {
  machineId: string;
  name: string;
  /** Lowercase, such as "stale", "disconnected" or "hardware unavailable". */
  state: string;
}

/** One machine's part of a combined reading. */
export interface ResourcePart {
  /** The part's weight and capacity: logical CPUs, reporting GPUs, or storage bytes. */
  capacity: number;
  machineId: string;
  name: string;
  /** A ratio for CPU and GPU, bytes for storage; undefined without a current reading. */
  value: number | undefined;
}

/** A machine whose part a combined reading leaves out, and why. */
export interface LeftOutPart {
  name: string;
  reason: "noReading" | "unsupported";
}

/**
 * A reading combined across the counted machines: an average ratio weighted by capacity (CPU,
 * GPU), or summed bytes against summed capacity (storage). Only parts with a current reading count,
 * so `capacity` covers exactly the parts `value` covers.
 */
export interface ResourceReading {
  capacity: number;
  leftOut: LeftOutPart[];
  parts: ResourcePart[];
  value: number | undefined;
}

/** The combined GPU reading, with the power drawn by the GPUs that report it. */
export interface GpuReading extends ResourceReading {
  powerWatts: number | undefined;
}

// Machines report every second; a reading a few seconds old no longer counts as current. A gauge's
// open bucket is a real reading, so it counts.
export const resourcesCurrentWindow = { durationMs: 60_000, stepMs: 1000 } as const;
const freshSteps = 5;
const fresh = { includeOpen: true } as const;

/**
 * Splits the listed machines into those the overview counts and those it names as left out. A
 * connected machine whose first snapshot is still loading is in neither list, so it never flashes
 * as unavailable.
 */
export function countMachines(
  machines: readonly Machine[],
  details: ReadonlyMap<string, MachineSnapshotDetail>,
): { counted: DescribedMachine[]; uncounted: UncountedMachine[] } {
  const counted: DescribedMachine[] = [];
  const uncounted: UncountedMachine[] = [];

  for (const machine of machines) {
    const ref = { machineId: machine.machineId, name: machineName(machine) };
    const state = machine.observationFreshness?.state;
    if (state !== NodeObservationState.CONNECTED) {
      uncounted.push({ ...ref, state: nodeConnectionLabel(state).toLowerCase() });
      continue;
    }

    const detail = details.get(machine.machineId);
    if (detail?.kind === "loading") continue;
    if (detail?.kind === "success") counted.push({ ...ref, snapshot: detail.snapshot });
    else uncounted.push({ ...ref, state: "hardware unavailable" });
  }

  return { counted, uncounted };
}

/**
 * The metric queries the overview reads, grouped by node (and device) with no node filter, so
 * history also covers machines not counted now. Groups that no described machine's hardware can
 * fill, such as a GPU signal no device supports, are left out, and nothing is read while no machine
 * is described. Described machines keep their last snapshot, so the queries stay put while a
 * machine drops out and returns.
 */
export function resourceQueryGroups(described: readonly DescribedMachine[]): MetricQueryGroup[] {
  if (!described.length) return [];

  const query = (metric: string, ...groupBy: string[]): MetricHistoryQuery => ({
    groupBy,
    matchers: {},
    metric,
  });
  const byNode = (metric: string) => query(metric, dimensionNames.node);
  const byDevice = (metric: string) => query(metric, dimensionNames.node, dimensionNames.device);
  const devices = described.flatMap((machine) => acceleratorFacts(machine.snapshot));
  const groups: MetricQueryGroup[] = [
    {
      current: [byNode(metricNames.nodeHostCpuUtilization)],
      history: [byNode(metricNames.nodeHostCpuUtilization)],
      id: "cpu",
    },
    ...memoryQueryGroups(described),
  ];

  if (described.some((machine) => hostFacts(machine.snapshot)?.filesystems.length)) {
    groups.push({
      current: [byNode(metricNames.nodeFilesystemUsed)],
      history: [byNode(metricNames.nodeFilesystemUsed), byNode(metricNames.nodeFilesystemSize)],
      id: "storage",
    });
  }
  if (devices.some((device) => device.supportedSignals.includes(AcceleratorSignal.UTILIZATION))) {
    const power = devices.some((device) =>
      device.supportedSignals.includes(AcceleratorSignal.POWER),
    );
    groups.push({
      current: [
        byDevice(metricNames.nodeAcceleratorUtilization),
        ...(power ? [byDevice(metricNames.nodeAcceleratorPower)] : []),
      ],
      history: [byDevice(metricNames.nodeAcceleratorUtilization)],
      id: "gpu",
    });
  }

  return groups;
}

/** CPU utilization across the counted machines, weighted by their logical CPUs. */
export function cpuReading(
  counted: readonly DescribedMachine[],
  utilization: MetricGrid | undefined,
): ResourceReading {
  const parts = counted.flatMap((machine): ResourcePart[] => {
    const cpus = hostFacts(machine.snapshot)?.cpuCount ?? 0;
    if (cpus <= 0) return [];

    return [
      {
        capacity: cpus,
        machineId: machine.machineId,
        name: machine.name,
        value: currentValue(nodeSeries(utilization, machine.machineId), freshSteps, fresh),
      },
    ];
  });

  return weightedAverage(parts);
}

/**
 * GPU utilization averaged over the GPUs reporting it, with their summed power. A machine whose
 * GPUs support no utilization reading is left out as unsupported, one whose GPUs have no current
 * reading as without a reading. Undefined when no counted GPU supports utilization.
 */
export function gpuReading(
  counted: readonly DescribedMachine[],
  utilization: MetricGrid | undefined,
  power: MetricGrid | undefined,
): GpuReading | undefined {
  const leftOut: LeftOutPart[] = [];
  const parts: ResourcePart[] = [];
  let utilizationSum = 0;
  let reporting = 0;
  let powerWatts: number | undefined;

  for (const machine of counted) {
    const devices = acceleratorFacts(machine.snapshot);
    const supported = devices.filter((device) =>
      device.supportedSignals.includes(AcceleratorSignal.UTILIZATION),
    );
    if (!supported.length) {
      if (devices.length) leftOut.push({ name: machine.name, reason: "unsupported" });
      continue;
    }

    const readings = supported.flatMap((device) => {
      const value = currentValue(
        deviceSeries(utilization, machine.machineId, device.deviceId),
        freshSteps,
        fresh,
      );
      return value === undefined ? [] : [value];
    });
    for (const device of devices) {
      if (!device.supportedSignals.includes(AcceleratorSignal.POWER)) continue;
      const watts = currentValue(
        deviceSeries(power, machine.machineId, device.deviceId),
        freshSteps,
        fresh,
      );
      if (watts !== undefined) powerWatts = (powerWatts ?? 0) + watts;
    }
    // A machine with some GPUs unread is named with its share, so a partial value says why.
    if (readings.length < supported.length) {
      leftOut.push({
        name: readings.length
          ? `${machine.name} (${supported.length - readings.length} of ${supported.length} GPUs)`
          : machine.name,
        reason: "noReading",
      });
    }
    utilizationSum += readings.reduce((total, value) => total + value, 0);
    reporting += readings.length;
    parts.push({
      capacity: supported.length,
      machineId: machine.machineId,
      name: machine.name,
      value: readings.length
        ? readings.reduce((total, value) => total + value, 0) / readings.length
        : undefined,
    });
  }
  if (!parts.length) return undefined;

  return {
    capacity: reporting,
    leftOut,
    parts,
    powerWatts,
    value: reporting ? utilizationSum / reporting : undefined,
  };
}

/** Used storage across the counted machines' real filesystems, against their summed sizes. */
export function storageReading(
  counted: readonly DescribedMachine[],
  used: MetricGrid | undefined,
): ResourceReading | undefined {
  const parts = counted.flatMap((machine): ResourcePart[] => {
    const filesystems = hostFacts(machine.snapshot)?.filesystems ?? [];
    if (!filesystems.length) return [];

    return [
      {
        capacity: filesystems.reduce(
          (total, filesystem) => total + Number(filesystem.sizeBytes),
          0,
        ),
        machineId: machine.machineId,
        name: machine.name,
        value: currentValue(nodeSeries(used, machine.machineId), freshSteps, fresh),
      },
    ];
  });
  if (!parts.length) return undefined;

  const reporting = parts.filter((part) => part.value !== undefined);
  return {
    capacity: reporting.reduce((total, part) => total + part.capacity, 0),
    leftOut: parts
      .filter((part) => part.value === undefined)
      .map((part) => ({ name: part.name, reason: "noReading" })),
    parts,
    value: reporting.length
      ? reporting.reduce((total, part) => total + (part.value ?? 0), 0)
      : undefined,
  };
}

/**
 * CPU utilization history. Combined, each step averages the machines reporting in it, weighted by
 * the logical CPUs of their latest snapshot, so a machine not counted now keeps its share of the
 * steps it reported; a machine never described has no CPU count and is left out of the combined
 * line. Per machine, every machine that reported keeps its line until it stopped.
 */
export function cpuHistory(
  utilization: MetricGrid,
  described: readonly DescribedMachine[],
  names: ReadonlyMap<string, string>,
  breakdown: HistoryBreakdown,
): ChartSeries[] {
  if (breakdown === "machines") return machineLines(utilization, names);

  const cpus = new Map(
    described.map((machine) => [machine.machineId, hostFacts(machine.snapshot)?.cpuCount ?? 0]),
  );
  const members = utilization.series.flatMap((series) => {
    const weight = cpus.get(series.labels[dimensionNames.node]) ?? 0;
    return weight > 0 ? [{ series, weight }] : [];
  });

  return [
    {
      color: "chart-1",
      id: "cpu",
      label: "CPU",
      ...averagePerStep(members, utilization.stepCount),
    },
  ];
}

/** Used storage history against filesystem size, paired per step like memory. */
export function storageHistory(
  used: MetricGrid,
  size: MetricGrid,
  names: ReadonlyMap<string, string>,
  breakdown: HistoryBreakdown,
): ChartSeries[] {
  const members = used.series.map((usedSeries) => {
    const node = usedSeries.labels[dimensionNames.node];
    return {
      capacity: alignedSeries(used, size, nodeSeries(size, node)),
      key: node,
      used: usedSeries,
    };
  });

  return usedAgainstCapacity(members, used.stepCount, names, breakdown);
}

/**
 * GPU utilization history: each step averages the GPUs reporting in it, across the site or per
 * machine.
 */
export function gpuHistory(
  utilization: MetricGrid,
  names: ReadonlyMap<string, string>,
  breakdown: HistoryBreakdown,
): ChartSeries[] {
  const members = utilization.series.map((series) => ({ series, weight: 1 }));
  if (breakdown === "combined") {
    return [
      {
        color: "chart-1",
        id: "gpu",
        label: "GPU",
        ...averagePerStep(members, utilization.stepCount),
      },
    ];
  }

  return groupByNode(utilization.series).map(([node, series]) => ({
    color: { resource: node },
    id: node,
    label: names.get(node) ?? node,
    ...averagePerStep(
      series.map((entry) => ({ series: entry, weight: 1 })),
      utilization.stepCount,
    ),
  }));
}

/**
 * A machine card's summary: its GPU models, repeated ones counted, and its logical CPUs, such as
 * "2 × NVIDIA RTX 4090 · 64 CPUs".
 */
export function machineSummary(snapshot: HardwareSnapshot): string {
  const models = new Map<string, number>();
  for (const device of acceleratorFacts(snapshot)) {
    models.set(device.model, (models.get(device.model) ?? 0) + 1);
  }
  const cpus = hostFacts(snapshot)?.cpuCount;

  return [
    ...[...models].map(([model, count]) => (count > 1 ? `${count} × ${model}` : model)),
    ...(cpus ? [`${cpus} CPUs`] : []),
  ].join(" · ");
}

/** The host facts of a snapshot, when its host observation succeeded. */
export function hostFacts(snapshot: HardwareSnapshot): HostFacts | undefined {
  const result = snapshot.host?.result;
  return result?.case === "facts" ? result.value : undefined;
}

/** The weighted average over the parts with a reading; the left-out parts are named. */
function weightedAverage(parts: ResourcePart[]): ResourceReading {
  const reporting = parts.filter((part) => part.value !== undefined);
  const capacity = reporting.reduce((total, part) => total + part.capacity, 0);

  return {
    capacity,
    leftOut: parts
      .filter((part) => part.value === undefined)
      .map((part) => ({ name: part.name, reason: "noReading" })),
    parts,
    value: capacity
      ? reporting.reduce((total, part) => total + (part.value ?? 0) * part.capacity, 0) / capacity
      : undefined,
  };
}

/** Per step, the weighted average of the members reporting in it; absent when none does. */
function averagePerStep(
  members: readonly { series: MetricGridSeries; weight: number }[],
  stepCount: number,
): StepValues {
  const values: (number | null)[] = [];
  const provisional: boolean[] = [];

  for (let index = 0; index < stepCount; index += 1) {
    let sum = 0;
    let weights = 0;
    let open = false;
    for (const { series, weight } of members) {
      const value = series.values[index];
      if (value === null || value === undefined) continue;
      sum += value * weight;
      weights += weight;
      open ||= series.provisional[index] ?? false;
    }
    values.push(weights ? sum / weights : null);
    provisional.push(open);
  }

  return { provisional, values };
}

/** One line per machine that reported, in its marker color. */
function machineLines(grid: MetricGrid, names: ReadonlyMap<string, string>): ChartSeries[] {
  return grid.series.map((series) => {
    const node = series.labels[dimensionNames.node];
    return {
      color: { resource: node },
      id: node,
      label: names.get(node) ?? node,
      provisional: series.provisional,
      values: series.values,
    };
  });
}

function groupByNode(series: readonly MetricGridSeries[]): [string, MetricGridSeries[]][] {
  const byNode = new Map<string, MetricGridSeries[]>();
  for (const entry of series) {
    const node = entry.labels[dimensionNames.node];
    byNode.set(node, [...(byNode.get(node) ?? []), entry]);
  }
  return [...byNode];
}
