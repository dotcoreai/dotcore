import { type MemoryFocusStatus, type MemoryRowCapacity } from "@/features/memory/memory-rows";
import { deploymentRefs, deviceLabel, poolKinds, poolSpans } from "@/features/memory/pool-spans";
import {
  type MemoryDeploymentRef,
  type MemoryKind,
  type MemoryRow,
  rowsFromSpans,
  type UnavailableMemory,
} from "@/features/memory/site-memory";
import { type ResourceEstimate } from "@/features/models/deploy/use-resource-estimate";
import type { ApplicationDeployment } from "@/gen/dotcore/application/v1/deployment_pb";
import {
  type MemoryPool,
  MemoryPoolKind,
  MemorySegmentSource,
} from "@/gen/dotcore/capacity/v1/memory_pb";
import {
  type MemoryPoolPreview,
  type PlacementPreview,
} from "@/gen/dotcore/capacity/v1/placement_preview_pb";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import { type ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import { type ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * The selected target's memory with this deployment added, as the estimate's placement preview
 * gives it: one row per memory kind, each pool's segments named after the site's deployments, and
 * the pools the backend judges past their fit line, which mark their rows. A pending or failed estimate says so per row
 * while an earlier preview of the same target is drawn, dimmed when its refresh failed, or in place
 * of the rows without one.
 */
export function previewMemory({
  applications,
  deployment,
  deployments,
  estimate,
  target,
  targetsLoading,
}: PreviewMemoryInput): PredictedMemory {
  if (!target) return targetsLoading ? { kind: "loading" } : { kind: "noTarget" };

  const targetName = target.name;
  // A preview read for another target, such as the one picked before, says nothing of this one.
  const preview = isPlacedOn(estimate.placement, target) ? estimate.placementPreview : undefined;
  if (!preview) {
    if (estimate.error) return { kind: "unavailable", message: estimateUnavailable, targetName };
    if (estimate.pending) return { kind: "loading", targetName };
    return { kind: "unavailable", message: readingsUnavailable, targetName };
  }

  const { spans, unavailable } = poolSpans({
    machine: { machineId: target.siteNodeId, name: target.nodeName },
    observedAt: preview.observedAt,
    pools: previewPools(preview).map(({ pool }) => ({ pool })),
    refs: deploymentRefs({
      applications,
      deployments,
      previewed: {
        ...deployment,
        colorKey: predictedDeploymentId,
        deploymentId: predictedDeploymentId,
      },
    }),
    stale: Boolean(estimate.stale),
  });
  const rows = rowsFromSpans(spans);
  if (!rows.length) return { kind: "unavailable", message: readingsUnavailable, targetName };

  return {
    capacity: rowCapacity(preview),
    deploymentId: predictedDeploymentId,
    focusStatus: focusStatus(estimate, preview),
    kind: "ready",
    overCapacity: overCapacity(preview),
    rows,
    targetName,
    unavailable,
  };
}

export interface PreviewMemoryInput {
  /** The site's application deployments, which name the preview's application segments. */
  applications?: readonly ApplicationDeployment[];
  /** This deployment as the rows name and color it. */
  deployment: Omit<MemoryDeploymentRef, "colorKey" | "deploymentId">;
  /** The site's model deployments, which name the preview's segments. */
  deployments: readonly ModelDeployment[];
  estimate: ResourceEstimate;
  target?: PreviewTarget;
  /** Whether the targets are still loading, so a missing target is not yet a choice. */
  targetsLoading: boolean;
}

/** The selected execution target: its ID and display name, and its SiteNode's ID and name. */
export interface PreviewTarget {
  executionTargetId: string;
  name: string;
  nodeName: string;
  siteNodeId: string;
}

/**
 * A memory preview: loading, no target selected, nothing to draw for the target and why, or the
 * target's rows with this deployment's status per row and the pools it pushes past their fit line.
 */
export type PredictedMemory =
  | { kind: "loading"; targetName?: string }
  | { kind: "noTarget"; targetName?: undefined }
  | { kind: "unavailable"; message: string; targetName: string }
  | {
      /** Where each row's use stands against the line its pools' verdicts judge. */
      capacity: Partial<Record<MemoryKind, MemoryRowCapacity>>;
      /** The ID this deployment's segments carry in `rows`. */
      deploymentId: string;
      focusStatus: Partial<Record<MemoryKind, MemoryFocusStatus>>;
      kind: "ready";
      overCapacity: MemoryOverCapacity[];
      rows: MemoryRow[];
      targetName: string;
      unavailable: UnavailableMemory[];
    };

/**
 * A pool this deployment pushes past its fit line: either beyond the pool's total, or into the
 * headroom kept free just under it.
 */
export interface MemoryOverCapacity {
  /** Whether drawn use is beyond the pool's total, not only past the fit line. */
  beyondTotal: boolean;
  /** Drawn use beyond the pool's total, or beyond the fit line while use is still under the total. */
  bytes: bigint;
  /** Share of the pool's total, in whole percent, that the fit line keeps free. */
  headroomPercent: number;
  /** The pool's name in a sentence: its memory kind, and its GPU when the target has several. */
  label: string;
}

const predictedDeploymentId = "model-deploy-dialog-prediction";

const estimateUnavailable = "Estimate unavailable.";

const readingsUnavailable = "Memory readings unavailable.";

const estimating: MemoryFocusStatus = {
  detail: "This deployment: estimating memory",
  readout: "Estimating…",
};

const kindLabels: Record<MemoryKind, string> = { ram: "RAM", vram: "VRAM" };

/**
 * The pools the backend judges this deployment exceeds there, past the total or into the headroom.
 * A pool that only reads exceeds for a declared minimum above its total has nothing drawn past its
 * fit line; the minimum's own notice covers it.
 */
function overCapacity(preview: PlacementPreview): MemoryOverCapacity[] {
  const pools = previewPools(preview);

  return pools.flatMap((entry): MemoryOverCapacity[] => {
    if (entry.freeMemoryFit !== MemoryFit.EXCEEDS || entry.overByBytes === 0n) return [];

    const kind = poolKinds.find((candidate) => candidate.pool === entry.pool.kind)?.kind;
    if (!kind) return [];

    const several = pools.filter((candidate) => candidate.pool.kind === entry.pool.kind).length > 1;
    return [
      {
        beyondTotal: entry.beyondTotal,
        bytes: entry.overByBytes,
        headroomPercent: headroomPercent(entry),
        label: several
          ? `${kindLabels[kind]} on ${deviceLabel(entry.pool, true)}`
          : kindLabels[kind],
      },
    ];
  });
}

/**
 * The rows the backend judges this deployment exceeds there, each marking the line its pools
 * cross. A row holding several pools reads as its worst: beyond the total when any pool is, at the
 * row's total; otherwise within the headroom, at its pools' fit lines together.
 */
function rowCapacity(preview: PlacementPreview): Partial<Record<MemoryKind, MemoryRowCapacity>> {
  const capacity: Partial<Record<MemoryKind, MemoryRowCapacity>> = {};

  for (const { kind, pool } of poolKinds) {
    const pools = previewPools(preview).filter((candidate) => candidate.pool.kind === pool);
    const exceeding = pools.filter(
      (candidate) => candidate.freeMemoryFit === MemoryFit.EXCEEDS && candidate.overByBytes > 0n,
    );
    if (!exceeding.length) continue;

    const beyondTotal = exceeding.some((candidate) => candidate.beyondTotal);
    const crossed = exceeding.filter((candidate) => candidate.beyondTotal === beyondTotal);
    capacity[kind] = {
      beyondTotal,
      headroomPercent: headroomPercent(crossed[0]),
      lineBytes: sum(
        pools.map(
          (candidate) =>
            (beyondTotal ? candidate.pool.totalBytes : candidate.fitLineBytes) ??
            candidate.pool.totalBytes ??
            0n,
        ),
      ),
      overBytes: sum(crossed.map((candidate) => candidate.overByBytes)),
    };
  }

  return capacity;
}

/**
 * What each row says about this deployment when its size isn't drawn: a pending estimate whose
 * feedback is due on every row, no demand in the pool models run in, or no extra host memory on a
 * dedicated-GPU target's RAM.
 */
function focusStatus(
  estimate: ResourceEstimate,
  preview: PlacementPreview,
): Partial<Record<MemoryKind, MemoryFocusStatus>> {
  if (estimate.loadingVisible) return { ram: estimating, vram: estimating };
  // A quick read says nothing until its feedback is due.
  if (estimate.pending) return {};

  const pools = previewPools(preview).map((entry) => entry.pool);
  const dedicated = pools.some((pool) => pool.kind === MemoryPoolKind.DEVICE);
  const previewed = (kind: MemoryPoolKind) =>
    pools.some(
      (pool) =>
        pool.kind === kind &&
        pool.segments.some(
          (segment) => segment.source === MemorySegmentSource.PREVIEWED && segment.bytes > 0n,
        ),
    );
  const status: Partial<Record<MemoryKind, MemoryFocusStatus>> = {};
  if (dedicated && !previewed(MemoryPoolKind.HOST)) {
    status.ram = { detail: "This deployment: no extra host memory" };
  }
  if (!previewed(dedicated ? MemoryPoolKind.DEVICE : MemoryPoolKind.HOST)) {
    status[dedicated ? "vram" : "ram"] = {
      detail: "This deployment: no accelerator memory estimate for these settings",
      readout: "No estimate",
    };
  }

  return status;
}

/** Whether `placement` names `target`. */
function isPlacedOn(placement: ExactPlacement | undefined, target: PreviewTarget): boolean {
  return (
    placement?.siteNodeId === target.siteNodeId &&
    placement.executionTargetId === target.executionTargetId
  );
}

/** A preview's pools with their verdicts; a pool preview without its pool has nothing to draw. */
function previewPools(preview: PlacementPreview): Array<MemoryPoolPreview & { pool: MemoryPool }> {
  return preview.pools.flatMap((entry) => (entry.pool ? [{ ...entry, pool: entry.pool }] : []));
}

/** The share of a pool's total its fit line keeps free, rounded to a whole percent. */
function headroomPercent({ fitLineBytes, pool }: MemoryPoolPreview & { pool: MemoryPool }): number {
  if (!pool.totalBytes || fitLineBytes === undefined) return 0;
  return Math.round((Number(pool.totalBytes - fitLineBytes) * 100) / Number(pool.totalBytes));
}

function sum(values: readonly bigint[]): bigint {
  return values.reduce((total, value) => total + value, 0n);
}
