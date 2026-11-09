import { create, type MessageInitShape } from "@bufbuild/protobuf";

import {
  type MemoryPool,
  MemoryPoolKind,
  type MemoryPoolSchema,
  MemorySegmentSource,
} from "@/gen/dotcore/capacity/v1/memory_pb";
import {
  type MemoryPoolPreview,
  type MemoryPoolPreviewSchema,
  type PlacementPreview,
  PlacementPreviewSchema,
} from "@/gen/dotcore/capacity/v1/placement_preview_pb";
import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";

export const gib = 1024n ** 3n;
export const mib = 1024n ** 2n;

/** A pool preview's fields beside its pool's own, segments naming their model deployment. */
type PoolInit = Omit<PreviewInit, "pool"> &
  Omit<MemoryPoolInit, "segments"> & {
    segments?: SegmentInit[];
  };
type PreviewInit = Exclude<MessageInitShape<typeof MemoryPoolPreviewSchema>, MemoryPoolPreview>;
type MemoryPoolInit = Exclude<MessageInitShape<typeof MemoryPoolSchema>, MemoryPool>;
interface SegmentInit {
  applicationDeploymentId?: string;
  bytes: bigint;
  modelDeploymentId?: string;
  source: MemorySegmentSource;
}

/** A placement preview of the given pools, observed a second ago and judged `fit` overall. */
export function placementPreview(
  pools: PoolInit[],
  fit: MemoryFit = MemoryFit.FITS,
): PlacementPreview {
  return create(PlacementPreviewSchema, {
    freeMemoryFit: fit,
    observedAt: { seconds: BigInt(Math.floor(Date.now() / 1000) - 1) },
    pools: pools.map(
      ({ beyondTotal, fitLineBytes, freeMemoryFit, overByBytes, segments, ...pool }) => ({
        beyondTotal,
        fitLineBytes,
        freeMemoryFit,
        overByBytes,
        pool: {
          ...pool,
          segments: (segments ?? []).map(
            ({ applicationDeploymentId, bytes, modelDeploymentId, source }) => ({
              bytes,
              owner: modelDeploymentId
                ? { case: "modelDeploymentId" as const, value: modelDeploymentId }
                : applicationDeploymentId
                  ? { case: "applicationDeploymentId" as const, value: applicationDeploymentId }
                  : undefined,
              source,
            }),
          ),
        },
      }),
    ),
  });
}

/**
 * A GPU's memory pool of `total` bytes holding `segments`, with its fit line 1% under the total
 * unless the total is unknown. It fits unless given a verdict.
 */
export function devicePool(
  total: bigint | undefined,
  segments: SegmentInit[],
  pool: Partial<PoolInit> = {},
): PoolInit {
  return {
    deviceId: "GPU-a",
    deviceName: "NVIDIA GeForce RTX 5090",
    fitLineBytes: fitLine(total),
    freeMemoryFit: total === undefined ? MemoryFit.UNKNOWN : MemoryFit.FITS,
    kind: MemoryPoolKind.DEVICE,
    segments,
    totalBytes: total,
    ...pool,
  };
}

/** A node's host memory pool of `total` bytes holding `segments`, as `devicePool` describes it. */
export function hostPool(
  total: bigint | undefined,
  segments: SegmentInit[],
  pool: Partial<PoolInit> = {},
): PoolInit {
  return {
    ...devicePool(total, segments, pool),
    deviceId: "",
    deviceName: "",
    kind: MemoryPoolKind.HOST,
    ...pool,
  };
}

/** A pool judged past its fit line by `bytes`, beyond its total or within the headroom. */
export function exceeding(bytes: bigint, beyondTotal: boolean): Partial<PoolInit> {
  return { beyondTotal, freeMemoryFit: MemoryFit.EXCEEDS, overByBytes: bytes };
}

export function measured(deploymentId: string, bytes: bigint): SegmentInit {
  return { bytes, modelDeploymentId: deploymentId, source: MemorySegmentSource.MEASURED };
}

export function estimated(deploymentId: string, bytes: bigint): SegmentInit {
  return { bytes, modelDeploymentId: deploymentId, source: MemorySegmentSource.ESTIMATED };
}

export function claimed(deploymentId: string, bytes: bigint): SegmentInit {
  return { bytes, modelDeploymentId: deploymentId, source: MemorySegmentSource.CLAIMED };
}

/** An application deployment's measured memory. */
export function application(applicationDeploymentId: string, bytes: bigint): SegmentInit {
  return { applicationDeploymentId, bytes, source: MemorySegmentSource.MEASURED };
}

export function other(bytes: bigint): SegmentInit {
  return { bytes, source: MemorySegmentSource.OTHER };
}

export function previewed(bytes: bigint): SegmentInit {
  return { bytes, source: MemorySegmentSource.PREVIEWED };
}

/**
 * The preview of a dedicated target: a 32 GiB GPU with 12 GiB in use and a 64 GiB host with 34 GiB
 * in use, with `accelerator` and `host` bytes previewed on them and the pools' verdicts given.
 */
export function dedicatedPreview({
  accelerator,
  device = {},
  host,
  hostVerdict = {},
}: {
  accelerator?: bigint;
  device?: Partial<PoolInit>;
  host?: bigint;
  hostVerdict?: Partial<PoolInit>;
}): PlacementPreview {
  return placementPreview([
    devicePool(
      32n * gib,
      [other(12n * gib), ...(accelerator === undefined ? [] : [previewed(accelerator)])],
      device,
    ),
    hostPool(64n * gib, [other(34n * gib), ...(host ? [previewed(host)] : [])], hostVerdict),
  ]);
}

function fitLine(total: bigint | undefined): bigint | undefined {
  return total === undefined ? undefined : total - total / 100n;
}
