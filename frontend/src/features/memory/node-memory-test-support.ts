import { create, type MessageInitShape } from "@bufbuild/protobuf";

import {
  type MemoryOwner,
  type MemoryOwnerSchema,
  type MemoryPool,
  MemoryPoolKind,
  type MemoryPoolSchema,
  type MemorySegment,
  type MemorySegmentSchema,
  MemorySegmentSource,
  type NodeMemory,
  NodeMemorySchema,
  NodeMemoryState,
} from "@/gen/dotcore/capacity/v1/memory_pb";
import {
  type ListNodeMemoryResponse,
  ListNodeMemoryResponseSchema,
} from "@/gen/dotcore/capacity/v1/service_pb";
import { type ModelDeployment, ModelDeploymentSchema } from "@/gen/dotcore/model/v1/deployment_pb";
import { ModelDeploymentStatus } from "@/gen/dotcore/model/v1/state_pb";

export const gib = 1024n ** 3n;
export const mib = 1024n ** 2n;

type PoolInit = Exclude<MessageInitShape<typeof MemoryPoolSchema>, MemoryPool>;
type SegmentInit = Exclude<MessageInitShape<typeof MemorySegmentSchema>, MemorySegment>;
type OwnerInit = Exclude<MessageInitShape<typeof MemoryOwnerSchema>, MemoryOwner>;

/** One pool of a node with the deployments it can't size. */
interface NodePoolInit {
  pool: PoolInit;
  unsized: OwnerInit[];
}

/** The node memory watch payload of the given nodes, in site order. */
export function nodeMemoryPayload(nodes: NodeMemory[]): ListNodeMemoryResponse {
  return create(ListNodeMemoryResponseSchema, { nodes });
}

/** One node's memory, observed a second ago unless it is unavailable. */
export function nodeMemory(
  siteNodeId: string,
  pools: NodePoolInit[],
  state = NodeMemoryState.AVAILABLE,
): NodeMemory {
  return create(NodeMemorySchema, {
    observedAt:
      state === NodeMemoryState.UNAVAILABLE
        ? undefined
        : { seconds: BigInt(Math.floor(Date.now() / 1000) - 1) },
    pools,
    siteNodeId,
    state,
  });
}

/** An RTX 5090's memory pool of `total` bytes, unknown when undefined, holding `segments`. */
export function devicePool(
  total: bigint | undefined,
  segments: SegmentInit[],
  pool: Partial<PoolInit> = {},
  unsizedOwners: OwnerInit[] = [],
): NodePoolInit {
  return {
    pool: {
      deviceId: "GPU-a",
      deviceName: "NVIDIA GeForce RTX 5090",
      kind: MemoryPoolKind.DEVICE,
      segments,
      totalBytes: total,
      ...pool,
    },
    unsized: unsizedOwners,
  };
}

/** A node's host memory pool, as `devicePool` describes it. */
export function hostPool(
  total: bigint | undefined,
  segments: SegmentInit[],
  pool: Partial<PoolInit> = {},
  unsizedOwners: OwnerInit[] = [],
): NodePoolInit {
  return devicePool(
    total,
    segments,
    { deviceId: "", deviceName: "", kind: MemoryPoolKind.HOST, ...pool },
    unsizedOwners,
  );
}

export function measured(modelDeploymentId: string, bytes: bigint): SegmentInit {
  return modelSegment(modelDeploymentId, bytes, MemorySegmentSource.MEASURED);
}

export function estimated(modelDeploymentId: string, bytes: bigint): SegmentInit {
  return modelSegment(modelDeploymentId, bytes, MemorySegmentSource.ESTIMATED);
}

export function claimed(modelDeploymentId: string, bytes: bigint): SegmentInit {
  return modelSegment(modelDeploymentId, bytes, MemorySegmentSource.CLAIMED);
}

/** An application deployment's measured memory. */
export function appSegment(applicationDeploymentId: string, bytes: bigint): SegmentInit {
  return {
    bytes,
    owner: { case: "applicationDeploymentId", value: applicationDeploymentId },
    source: MemorySegmentSource.MEASURED,
  };
}

export function other(bytes: bigint): SegmentInit {
  return { bytes, source: MemorySegmentSource.OTHER };
}

/** A deployment a pool lists as unsized. */
export function unsized(
  owner: { applicationDeploymentId: string } | { modelDeploymentId: string },
): OwnerInit {
  return {
    owner:
      "modelDeploymentId" in owner
        ? { case: "modelDeploymentId", value: owner.modelDeploymentId }
        : { case: "applicationDeploymentId", value: owner.applicationDeploymentId },
  };
}

function modelSegment(
  modelDeploymentId: string,
  bytes: bigint,
  source: MemorySegmentSource,
): SegmentInit {
  return { bytes, owner: { case: "modelDeploymentId", value: modelDeploymentId }, source };
}

/** A ready local deployment of one model, as the site's deployment list holds it. */
export function runningDeployment(id: string, name: string): ModelDeployment {
  return create(ModelDeploymentSchema, {
    modelDeploymentId: id,
    snapshot: {
      source: {
        case: "local",
        value: { modelDisplayName: name, modelId: `model-${id}`, profileId: "default" },
      },
    },
    state: { status: ModelDeploymentStatus.READY },
  });
}
