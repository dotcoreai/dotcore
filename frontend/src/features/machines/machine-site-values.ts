import type { DescribedMachine } from "@/features/memory/described-machines";
import { acceleratorFacts, isDedicated } from "@/features/memory/snapshot-facts";
import { currentValue } from "@/features/metrics/metric-current";
import type { MetricStatsGrid, MetricStatsSeries } from "@/features/metrics/metric-grid";
import { dimensionNames } from "@/features/metrics/metric-names";
import { perStepValues } from "@/features/metrics/metric-range-values";
import { AcceleratorSignal } from "@/gen/dotcore/observation/v1/observation_pb";

import { hostFacts } from "./machine-resources";

/** What the machines' latest snapshots say about how to combine their readings. */
export interface MachineFacts {
  /** Logical CPUs by machine id, the weight of its CPU reading. */
  cpus: ReadonlyMap<string, number>;
  /** Device keys (`machine/device`) of GPUs that own their memory; unified GPUs use the host's. */
  dedicated: ReadonlySet<string>;
  /** GPUs reporting utilization by machine id, the weight of its GPU reading. */
  gpus: ReadonlyMap<string, number>;
}

/** One value per grid step, `null` where absent, and whether each step's bucket is still open. */
export interface Steps {
  provisional: boolean[];
  values: (number | null)[];
}

/** The readings of the newest closed step, for the site and for each machine; absent without one. */
export interface MachineNow {
  cpu?: number;
  gpu?: number;
  power?: number;
  ramTotal?: number;
  ramUsed?: number;
  storageSize?: number;
  storageUsed?: number;
  temperature?: number;
  vramTotal?: number;
  vramUsed?: number;
}

export const noMachineFacts: MachineFacts = {
  cpus: new Map(),
  dedicated: new Set(),
  gpus: new Map(),
};

/** The weights and device kinds of the described machines. */
export function machineFacts(described: readonly DescribedMachine[]): MachineFacts {
  const cpus = new Map<string, number>();
  const gpus = new Map<string, number>();
  const dedicated = new Set<string>();

  for (const machine of described) {
    const cpuCount = hostFacts(machine.snapshot)?.cpuCount ?? 0;
    if (cpuCount > 0) cpus.set(machine.machineId, cpuCount);

    const devices = acceleratorFacts(machine.snapshot);
    const reporting = devices.filter((device) =>
      device.supportedSignals.includes(AcceleratorSignal.UTILIZATION),
    );
    if (reporting.length) gpus.set(machine.machineId, reporting.length);

    for (const device of devices) {
      if (isDedicated(device)) dedicated.add(deviceKey(machine.machineId, device.deviceId));
    }
  }

  return { cpus, dedicated, gpus };
}

/** A stable text for the facts, so equal facts can share one memoized value. */
export function machineFactsKey(facts: MachineFacts): string {
  return JSON.stringify([
    [...facts.cpus].sort(),
    [...facts.dedicated].sort(),
    [...facts.gpus].sort(),
  ]);
}

/** The facts a key stands for. */
export function machineFactsFromKey(key: string): MachineFacts {
  const [cpus, dedicated, gpus] = JSON.parse(key) as [
    [string, number][],
    string[],
    [string, number][],
  ];

  return { cpus: new Map(cpus), dedicated: new Set(dedicated), gpus: new Map(gpus) };
}

/**
 * The machine metrics the overview reads, each kept apart per machine (and per GPU for memory), so
 * the site's figures and each machine's come from the same answers.
 */
export type MachineMetric =
  | "cpu"
  | "gpu"
  | "power"
  | "ramAvailable"
  | "ramTotal"
  | "storageSize"
  | "storageUsed"
  | "temperature"
  | "vramTotal"
  | "vramUsed";

/** The answers of some machine metrics, by metric. */
export type MachineGrids = Partial<Record<MachineMetric, MetricStatsGrid>>;

/** What the overview shows of a machine, and of the site: the readings `MachineNow` holds. */
export type MachineSignal = keyof MachineNow;

/**
 * One signal per machine from the metrics it reads: used RAM is the total less what is available,
 * VRAM counts dedicated GPUs only, and every other signal is its metric as read.
 */
export function machineSignal(
  signal: MachineSignal,
  grids: MachineGrids,
  facts: MachineFacts,
): Map<string, Steps> {
  switch (signal) {
    case "ramUsed":
      return usedSteps(machineSteps(grids.ramTotal), machineSteps(grids.ramAvailable));
    case "vramTotal":
      return dedicatedSteps(grids.vramTotal, facts);
    case "vramUsed":
      return dedicatedSteps(grids.vramUsed, facts);
    default:
      return machineSteps(grids[signal]);
  }
}

/**
 * One signal for the site, its machines combined per step the way the signal adds up: CPU and GPU
 * weighted by each machine's CPUs and GPUs, temperature the hottest machine, total RAM and storage
 * size over the machines reporting the matching used figure, and every other signal summed. Absent
 * without any answer.
 */
export function siteSignal(
  signal: MachineSignal,
  grids: MachineGrids,
  facts: MachineFacts,
): Steps | undefined {
  const stepCount = Object.values(grids).find((grid) => grid)?.stepCount;
  if (stepCount === undefined) return undefined;

  const machines = machineSignal(signal, grids, facts);
  switch (signal) {
    case "cpu":
      return weightedSteps(machines, facts.cpus, stepCount);
    case "gpu":
      return weightedSteps(machines, facts.gpus, stepCount);
    case "temperature":
      return combine([...machines.values()], max, stepCount);
    case "ramTotal": {
      const used = machineSignal("ramUsed", grids, facts);
      return sumSteps(
        [...used.keys()].flatMap((key) => machines.get(key) ?? []),
        stepCount,
      );
    }
    case "storageSize": {
      const used = machineSignal("storageUsed", grids, facts);
      return sumSteps(
        [...used.keys()].flatMap((key) => machines.get(key) ?? []),
        stepCount,
      );
    }
    default:
      return sumSteps(machines.values(), stepCount);
  }
}

/** The answers of the "now" stream as readings of the newest closed step, for the site and each machine. */
export function machineNow(
  grids: MachineGrids,
  facts: MachineFacts,
): { machines: Map<string, MachineNow>; site: MachineNow } {
  const machines = new Map<string, MachineNow>();
  const site: MachineNow = {};

  for (const signal of machineSignals) {
    const value = newestClosed(siteSignal(signal, grids, facts));
    if (value !== undefined) site[signal] = value;

    for (const [machine, steps] of machineSignal(signal, grids, facts)) {
      const now = machines.get(machine) ?? {};
      machines.set(machine, now);
      const reading = newestClosed(steps);
      if (reading !== undefined) now[signal] = reading;
    }
  }

  return { machines, site };
}

const machineSignals: readonly MachineSignal[] = [
  "cpu",
  "gpu",
  "power",
  "ramTotal",
  "ramUsed",
  "storageSize",
  "storageUsed",
  "temperature",
  "vramTotal",
  "vramUsed",
];

/** Closed steps newer than this many steps count as current; older ones read as absent. */
const nowFreshSteps = 5;

/**
 * The newest closed step's value: an open step is skipped however recent, so a reading never
 * flickers with a bucket still filling. Absent when no closed step is fresh.
 */
function newestClosed(steps: Steps | undefined): number | undefined {
  return currentValue(steps, nowFreshSteps);
}

/** Series of a grid combined by the key of their labels; series sharing a key add up per step. */
function groupSteps(
  stats: MetricStatsGrid | undefined,
  keyOf: (labels: MetricStatsSeries["labels"]) => string | undefined,
): Map<string, Steps> {
  const groups = new Map<string, Steps>();

  for (const series of stats?.series ?? []) {
    const key = keyOf(series.labels);
    if (key === undefined) continue;

    const steps = { provisional: series.provisional, values: perStepValues(series.steps) };
    const held = groups.get(key);
    groups.set(key, held ? combine([held, steps], sum) : steps);
  }

  return groups;
}

/** The steps of a grid's series by machine id. */
function machineSteps(stats: MetricStatsGrid | undefined): Map<string, Steps> {
  return groupSteps(stats, (labels) => labels[dimensionNames.node]);
}

/** The per-machine steps of a per-device grid, from the dedicated GPUs only. */
function dedicatedSteps(stats: MetricStatsGrid | undefined, facts: MachineFacts) {
  return groupSteps(stats, (labels) =>
    facts.dedicated.has(
      deviceKey(labels[dimensionNames.node] ?? "", labels[dimensionNames.device] ?? ""),
    )
      ? labels[dimensionNames.node]
      : undefined,
  );
}

/** The device key a dedicated GPU is listed under. */
function deviceKey(machineId: string, deviceId: string): string {
  return `${machineId}/${deviceId}`;
}

/** Per step, the sum of the machines reporting in it. */
function sumSteps(machines: Iterable<Steps>, stepCount: number): Steps {
  return combine([...machines], sum, stepCount);
}

/**
 * Per step, the average of the machines reporting in it, weighted by `weights`; a machine without a
 * weight does not count. A step is open when any machine counted in it is.
 */
function weightedSteps(
  machines: ReadonlyMap<string, Steps>,
  weights: ReadonlyMap<string, number>,
  stepCount: number,
): Steps {
  const members = [...machines].flatMap(([machine, steps]) => {
    const weight = weights.get(machine) ?? 0;
    return weight > 0 ? [{ steps, weight }] : [];
  });

  return combine(
    members.map((member) => member.steps),
    (values, indexes) => {
      let weights = 0;
      let total = 0;
      values.forEach((value, position) => {
        const weight = members[indexes[position]].weight;
        weights += weight;
        total += value * weight;
      });
      return total / weights;
    },
    stepCount,
  );
}

/** Used memory per machine: its total minus what is available, never negative. */
function usedSteps(
  totals: ReadonlyMap<string, Steps>,
  available: ReadonlyMap<string, Steps>,
): Map<string, Steps> {
  const used = new Map<string, Steps>();

  for (const [machine, total] of totals) {
    const free = available.get(machine);
    if (!free) continue;

    used.set(machine, {
      provisional: total.provisional.map(
        (open, index) => open || (free.provisional[index] ?? false),
      ),
      values: total.values.map((value, index) => {
        const left = free.values[index];
        return value === null || left === null || left === undefined
          ? null
          : Math.max(0, value - left);
      }),
    });
  }

  return used;
}

type Reduce = (values: number[], indexes: number[]) => number;

function sum(values: number[]): number {
  return values.reduce((total, value) => total + value, 0);
}

function max(values: number[]): number {
  return Math.max(...values);
}

/** Per step, reduces the values present; a step no member has a value for stays absent. */
function combine(members: readonly Steps[], reduce: Reduce, stepCount?: number): Steps {
  const length = stepCount ?? Math.max(0, ...members.map((member) => member.values.length));
  const values: (number | null)[] = [];
  const provisional: boolean[] = [];

  for (let step = 0; step < length; step += 1) {
    const present: number[] = [];
    const indexes: number[] = [];
    let open = false;
    members.forEach((member, index) => {
      const value = member.values[step];
      if (value === null || value === undefined) return;

      present.push(value);
      indexes.push(index);
      open ||= member.provisional[step] ?? false;
    });
    values.push(present.length ? reduce(present, indexes) : null);
    provisional.push(open);
  }

  return { provisional, values };
}
