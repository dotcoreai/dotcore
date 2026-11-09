import type { Timestamp } from "@bufbuild/protobuf/wkt";

import { applicationDeploymentPresentationName } from "@/domain/application-deployment-name";
import { localModelSnapshot, modelDeploymentPresentationName } from "@/domain/model-projection";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type MemoryOwner,
  type MemoryPool,
  MemoryPoolKind,
  MemorySegmentSource,
} from "@/gen/dotcore/capacity/v1/memory_pb";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

import type { MemoryRowCapacity } from "./memory-rows";
import type {
  MachineMemory,
  MemoryDeploymentRef,
  MemoryKind,
  MemoryMachine,
  MemoryRow,
  MemorySegment,
  MemorySpan,
  UnavailableMemory,
  UnmeasuredReason,
  UnmeasuredUsage,
} from "./site-memory";

/** One pool to draw: what occupies it, and the deployments it holds without a size. */
export interface DrawnPool {
  pool: MemoryPool;
  unsized?: readonly MemoryOwner[];
}

/** A segment or unsized deployment's owner, as the capacity component names it. */
export type PoolOwner = MemoryOwner["owner"];

/** Names the deployments a machine's pools hold, as their cards do. */
export interface DeploymentRefs {
  /** The previewed demand's deployment; without it, previewed bytes read as other use. */
  previewed?: MemoryDeploymentRef;
  /** The deployment an owner names; undefined for a segment without one. */
  ref: (owner: PoolOwner) => MemoryDeploymentRef | undefined;
}

/**
 * One machine's pools as spans, one per memory kind: device pools sum into one VRAM span with a
 * line per GPU, and the host pool is the RAM span. Each GPU pool of known size also gets its own
 * VRAM span. A pool of unknown size draws nothing and is listed as unreadable with the deployments
 * it holds. Segments merge per deployment across a span's pools, reading as their least certain
 * source; a deployment the caller doesn't list yet still draws under a generic name.
 */
export function poolSpans({
  machine,
  observedAt,
  pools,
  refs,
  stale,
  unsizedReason = () => "notReady",
}: {
  machine: MemoryMachine;
  observedAt?: Timestamp;
  pools: readonly DrawnPool[];
  refs: DeploymentRefs;
  stale: boolean;
  /** Why an unsized deployment has no drawn size. */
  unsizedReason?: (owner: PoolOwner) => UnmeasuredReason;
}): MachineMemory {
  const deviceSpans = new Map<string, MemorySpan>();
  const spans: MemorySpan[] = [];
  const unavailable: UnavailableMemory[] = [];
  const span = (kind: MemoryKind, known: readonly KnownPool[], devices: boolean): MemorySpan => ({
    devices: devices
      ? known.map(({ pool }) => ({
          label: deviceLabel(pool, known.length > 1),
          totalBytes: pool.totalBytes,
          usedBytes: drawnBytes(pool),
        }))
      : [],
    kind,
    machine,
    observedAt,
    otherBytes: sum(known.flatMap(({ pool }) => otherSegmentBytes(pool, refs))),
    otherLabel: kind === "vram" ? "Other" : "System and other",
    overTotalBytes: sum(known.map(({ pool }) => pool.overTotalBytes)),
    segments: mergedSegments(known, machine.machineId, refs),
    stale,
    totalBytes: sum(known.map(({ pool }) => pool.totalBytes)),
    unmeasured: unsizedUsages(known, refs, unsizedReason),
    usedBytes: sum(known.map(({ pool }) => drawnBytes(pool))),
  });

  for (const { kind, pool: poolKind } of poolKinds) {
    const ofKind = pools.filter(({ pool }) => pool.kind === poolKind);
    const known = ofKind.flatMap((entry): KnownPool[] =>
      entry.pool.totalBytes === undefined
        ? []
        : [{ ...entry, pool: { ...entry.pool, totalBytes: entry.pool.totalBytes } }],
    );
    const unknown = ofKind.filter(({ pool }) => pool.totalBytes === undefined);
    if (unknown.length) {
      unavailable.push({ deployments: heldDeployments(unknown, refs), kind, machine });
    }
    if (!known.length) continue;

    spans.push(span(kind, known, kind === "vram"));
    if (kind === "vram") {
      for (const entry of known) {
        if (entry.pool.deviceId) deviceSpans.set(entry.pool.deviceId, span(kind, [entry], false));
      }
    }
  }

  return { deviceSpans, spans, unavailable };
}

/**
 * Names pool owners after the site's model and application deployments. A deployment the lists
 * don't hold yet, such as one created a moment ago, keeps its ID as its color key under a generic
 * name.
 */
export function deploymentRefs({
  applications = [],
  deployments,
  previewed,
}: {
  applications?: readonly ApplicationDeployment[];
  deployments: readonly ModelDeployment[];
  previewed?: MemoryDeploymentRef;
}): DeploymentRefs {
  const models = new Map(deployments.map((entry) => [entry.modelDeploymentId, entry]));
  const apps = new Map(applications.map((entry) => [entry.applicationDeploymentId, entry]));

  return {
    previewed,
    ref: (owner) => {
      switch (owner.case) {
        case "modelDeploymentId": {
          const entry = models.get(owner.value);
          return {
            colorKey: owner.value,
            deploymentId: owner.value,
            name: entry ? modelDeploymentPresentationName(entry) : "Model deployment",
            profileId: (entry && localModelSnapshot(entry.snapshot)?.profileId) || undefined,
          };
        }
        case "applicationDeploymentId": {
          const entry = apps.get(owner.value);
          return {
            colorKey: owner.value,
            deploymentId: owner.value,
            name: entry ? applicationDeploymentPresentationName(entry) : "Application deployment",
            owner: "application",
          };
        }
        case undefined:
          return undefined;
      }
    },
  };
}

/**
 * Each row whose pools hold drawn use past their total, as a capacity marker at the row's total.
 * Rows within their totals are left unmarked.
 */
export function pastTotalCapacity(
  rows: readonly MemoryRow[],
): Partial<Record<MemoryKind, MemoryRowCapacity>> {
  const capacity: Partial<Record<MemoryKind, MemoryRowCapacity>> = {};

  for (const row of rows) {
    const overBytes = sum(row.spans.map((span) => span.overTotalBytes));
    if (overBytes === 0n) continue;

    capacity[row.kind] = {
      beyondTotal: true,
      headroomPercent: 0,
      lineBytes: row.totalBytes,
      overBytes,
    };
  }

  return capacity;
}

/** A device pool's GPU name, with its index when the machine has several. */
export function deviceLabel(pool: MemoryPool, several: boolean): string {
  const name = pool.deviceName || "GPU";
  return several && pool.deviceIndex !== undefined ? `${name} · GPU ${pool.deviceIndex}` : name;
}

/** The memory kind each pool kind draws in. */
export const poolKinds: ReadonlyArray<{ kind: MemoryKind; pool: MemoryPoolKind }> = [
  { kind: "vram", pool: MemoryPoolKind.DEVICE },
  { kind: "ram", pool: MemoryPoolKind.HOST },
];

/** A pool whose total is known. */
interface KnownPool extends DrawnPool {
  pool: MemoryPool & { totalBytes: bigint };
}

/** What a deployment segment's bytes stand for in the rows; other use and unknown sources have none. */
const segmentSources: Partial<Record<MemorySegmentSource, MemorySegment["source"]>> = {
  [MemorySegmentSource.MEASURED]: "observed",
  [MemorySegmentSource.ESTIMATED]: "estimated",
  [MemorySegmentSource.CLAIMED]: "claimed",
  [MemorySegmentSource.PREVIEWED]: "predicted",
};

const sourceRank: Record<MemorySegment["source"], number> = {
  claimed: 2,
  estimated: 1,
  observed: 0,
  predicted: 3,
};

/**
 * Deployment segments of the pools, one per deployment in first-seen order with its bytes summed
 * across pools and sources. A merged segment reads as its least certain source.
 */
function mergedSegments(
  pools: readonly DrawnPool[],
  machineId: string,
  refs: DeploymentRefs,
): MemorySegment[] {
  const merged = new Map<string, MemorySegment>();

  for (const segment of pools.flatMap(({ pool }) => pool.segments)) {
    const source = segmentSources[segment.source];
    const ref = segmentRef(segment, refs);
    if (!source || !ref) continue;

    const existing = merged.get(refKey(ref));
    merged.set(refKey(ref), {
      ...ref,
      bytes: (existing?.bytes ?? 0n) + segment.bytes,
      machineId,
      source:
        existing && sourceRank[existing.source] > sourceRank[source] ? existing.source : source,
    });
  }

  return [...merged.values()];
}

/** The deployments the pools list as unsized, once each in first-seen order. */
function unsizedUsages(
  pools: readonly DrawnPool[],
  refs: DeploymentRefs,
  reason: (owner: PoolOwner) => UnmeasuredReason,
): UnmeasuredUsage[] {
  const usages = new Map<string, UnmeasuredUsage>();

  for (const { owner } of pools.flatMap((entry) => entry.unsized ?? [])) {
    const ref = refs.ref(owner);
    if (ref && !usages.has(refKey(ref))) {
      usages.set(refKey(ref), { ...ref, reason: reason(owner) });
    }
  }

  return [...usages.values()];
}

/** The deployments pools of unknown size hold, by segment or unsized, once each. */
function heldDeployments(pools: readonly DrawnPool[], refs: DeploymentRefs): MemoryDeploymentRef[] {
  const held = new Map<string, MemoryDeploymentRef>();
  const refsOf = pools.flatMap(({ pool, unsized = [] }) => [
    ...pool.segments.map((segment) => segmentRef(segment, refs)),
    ...unsized.map(({ owner }) => refs.ref(owner)),
  ]);
  for (const ref of refsOf) {
    if (ref && !held.has(refKey(ref))) held.set(refKey(ref), ref);
  }

  return [...held.values()];
}

/** The deployment a segment belongs to: its owner, or this deployment for previewed demand. */
function segmentRef(
  segment: MemoryPool["segments"][number],
  refs: DeploymentRefs,
): MemoryDeploymentRef | undefined {
  return segment.source === MemorySegmentSource.PREVIEWED
    ? refs.previewed
    : refs.ref(segment.owner);
}

/** A deployment's identity across lanes, since a model and an application may share an ID. */
function refKey(ref: MemoryDeploymentRef): string {
  return `${ref.owner ?? "model"}:${ref.deploymentId}`;
}

/** Bytes of a pool's segments without a drawn deployment, which the bars hatch as other use. */
function otherSegmentBytes(pool: MemoryPool, refs: DeploymentRefs): bigint[] {
  return pool.segments
    .filter((segment) => segmentSources[segment.source] === undefined || !segmentRef(segment, refs))
    .map((segment) => segment.bytes);
}

/** A pool's drawn use: all its segments together. */
function drawnBytes(pool: MemoryPool): bigint {
  return sum(pool.segments.map((segment) => segment.bytes));
}

function sum(values: readonly bigint[]): bigint {
  return values.reduce((total, value) => total + value, 0n);
}
