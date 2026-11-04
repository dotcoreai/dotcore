import type { Timestamp } from "@bufbuild/protobuf/wkt";

import type { MeasurementGap } from "./unavailable-memory-codes";

/**
 * The site's memory as rows by memory type: VRAM (dedicated GPU memory) and RAM (system memory,
 * which is where unified machines run their models). Each row holds one span per machine, and each
 * span splits into deployment segments (models and applications), memory in use outside any
 * deployment, and free memory.
 */
export interface SiteMemory {
  /** Each drawn machine's own spans, by machine ID. */
  machines: ReadonlyMap<string, MachineMemory>;
  /** VRAM first when any machine has dedicated GPUs, then RAM. */
  rows: MemoryRow[];
  /** Whether any drawn value may be outdated after a failed read or a stale reading. */
  stale: boolean;
  /** Machines or pools with no readable memory, with the deployments they hide. */
  unavailable: UnavailableMemory[];
}

export type MemoryKind = "ram" | "vram";

export interface MemoryRow {
  kind: MemoryKind;
  spans: MemorySpan[];
  totalBytes: bigint;
  /** Drawn use of the row's spans. */
  usedBytes: bigint;
}

/** One machine's share of a memory row. */
export interface MemorySpan {
  /** Per-GPU readings summed into a VRAM span, for detail on hover. */
  devices: MemoryDevice[];
  kind: MemoryKind;
  machine: MemoryMachine;
  observedAt?: Timestamp;
  /** Used memory no drawn segment accounts for. */
  otherBytes: bigint;
  otherLabel: "Other" | "System and other";
  /** Drawn use past the span's total, as the capacity component states it. */
  overTotalBytes: bigint;
  segments: MemorySegment[];
  stale: boolean;
  totalBytes: bigint;
  /** Deployments expected to hold memory in this span without a drawable size. */
  unmeasured: UnmeasuredUsage[];
  /** Drawn use: the segments and other use together. */
  usedBytes: bigint;
}

/** One machine's spans, with one VRAM span per GPU pool for the machine's own inspector. */
export interface MachineMemory {
  /**
   * One VRAM span per dedicated GPU of known size, keyed by device ID, holding only that GPU's
   * deployments. The summed VRAM span in `spans` stays the machine's total.
   */
  deviceSpans: Map<string, MemorySpan>;
  spans: MemorySpan[];
  unavailable: UnavailableMemory[];
}

export interface MemoryMachine {
  machineId: string;
  name: string;
}

export interface MemoryDevice {
  label: string;
  totalBytes: bigint;
  usedBytes: bigint;
}

export interface MemoryDeploymentRef {
  /** Card identity color key, so a segment matches its running card. */
  colorKey: string;
  deploymentId: string;
  name: string;
  /** Absent for a model deployment; an application deployment has no model card to link to. */
  owner?: "application";
  profileId?: string;
}

/** A drawn deployment size: memory observed for its processes, an estimate, or a claim. */
export interface MemorySegment extends MemoryDeploymentRef {
  bytes: bigint;
  machineId: string;
  /**
   * `claimed` marks a starting deployment's expected use beyond what is measured of it, and
   * `predicted` a deployment not created yet, drawn on top of what occupies the pool.
   */
  source: "claimed" | "estimated" | "observed" | "predicted";
}

/**
 * Why a deployment has no drawn size: its first reading hasn't arrived, its process isn't running,
 * measurement failed, it is ready with nothing to size it, it isn't ready yet, or its machine's
 * reading is stale.
 */
export type UnmeasuredReason = MeasurementGap | "noEstimate" | "notReady" | "stale";

export interface UnmeasuredUsage extends MemoryDeploymentRef {
  reason: UnmeasuredReason;
}

/** A machine without a readable memory reading, or one of its pools of unknown size. */
export interface UnavailableMemory {
  deployments: MemoryDeploymentRef[];
  /** The unreadable pool; absent when the whole machine is unreadable. */
  kind?: MemoryKind;
  machine: MemoryMachine;
}

/** Groups spans into rows by memory type, VRAM first, keeping span order within each row. */
export function rowsFromSpans(spans: readonly MemorySpan[]): MemoryRow[] {
  return memoryKinds.flatMap((kind): MemoryRow[] => {
    const rowSpans = spans.filter((span) => span.kind === kind);
    if (!rowSpans.length) return [];

    return [
      {
        kind,
        spans: rowSpans,
        totalBytes: sum(rowSpans.map((span) => span.totalBytes)),
        usedBytes: sum(rowSpans.map((span) => span.usedBytes)),
      },
    ];
  });
}

/** One machine's share of the site's memory: its rows and its unreadable pools. */
export function machineMemory(
  memory: SiteMemory,
  machineId: string,
): { rows: MemoryRow[]; unavailable: UnavailableMemory[] } {
  return {
    rows: rowsFromSpans(memory.machines.get(machineId)?.spans ?? []),
    unavailable: memory.unavailable.filter((entry) => entry.machine.machineId === machineId),
  };
}

const memoryKinds: readonly MemoryKind[] = ["vram", "ram"];

function sum(values: readonly bigint[]): bigint {
  return values.reduce((total, value) => total + value, 0n);
}
