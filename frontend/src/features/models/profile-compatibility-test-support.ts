import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { vi } from "vitest";

import { MemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import {
  type ListModelProfileCompatibilityResponse,
  ListModelProfileCompatibilityResponseSchema,
  type LocalModelDeploymentAdvisory,
  ModelCompatibilityReasonCode,
  type ModelCompatibilityReasonSchema,
  ModelCompatibilityStatus,
  type ModelProfileSizingSchema,
  SizingScenarioUnsupportedReason,
  StartingTotalContextSource,
} from "@/gen/dotcore/model/v1/service_pb";

/** One profile's entry on a target: why it can't run, or its advisories, class, and memory fit. */
export interface ProfileEntryInit {
  advisories?: LocalModelDeploymentAdvisory[];
  /** The selected hardware class of a profile that runs; defaults to `nvidia`. */
  hardwareClassId?: string;
  /** The memory fit on free memory of a profile that runs; defaults to its fit on the total. */
  freeMemoryFit?: MemoryFit;
  /** The memory fit on the total of a profile that runs; defaults to fits. */
  memoryFit?: MemoryFit;
  reason?: MessageInitShape<typeof ModelCompatibilityReasonSchema>;
  /** Where the starting total context comes from; the largest fit when one is given. */
  startingTotalContextSource?: StartingTotalContextSource;
  /** The total context a deployment starts from on the target, for a profile that runs. */
  startingTotalContextTokens?: number;
  unsupportedReason?: SizingScenarioUnsupportedReason;
}

/** One target of a compatibility read: its own verdict and, when it runs, its profile entries. */
export interface TargetCompatibilityInit {
  entries?: (ProfileEntryInit & { profileId: string })[];
  reason?: MessageInitShape<typeof ModelCompatibilityReasonSchema>;
  status: ModelCompatibilityStatus;
  target: { executionTargetId: string; siteNodeId: string };
}

/** A reason a profile can't run: its NVIDIA driver minimum is above the observed version. */
export const driverTooOld = {
  code: ModelCompatibilityReasonCode.NVIDIA_DRIVER_BELOW_MINIMUM,
  minimumNvidiaDriverMajorVersion: 575,
  observedNvidiaDriverMajorVersion: 560,
};

/** A target that passes target-level checks, with each profile running unless given a reason. */
export function targetCompatibility(
  target: TargetCompatibilityInit["target"],
  profiles: Record<string, ProfileEntryInit>,
): TargetCompatibilityInit {
  return {
    entries: Object.entries(profiles).map(([profileId, values]) => ({ profileId, ...values })),
    status: ModelCompatibilityStatus.RUNS,
    target,
  };
}

/** A target whose machine isn't reporting, so every profile is unknown there. */
export function notReporting(target: TargetCompatibilityInit["target"]): TargetCompatibilityInit {
  return {
    reason: { code: ModelCompatibilityReasonCode.TARGET_NOT_REPORTING },
    status: ModelCompatibilityStatus.UNKNOWN,
    target,
  };
}

/** The compatibility read of one model over the given targets. */
export function compatibilityResponse(
  modelId: string,
  ...targets: TargetCompatibilityInit[]
): ListModelProfileCompatibilityResponse {
  return create(ListModelProfileCompatibilityResponseSchema, {
    entries: targets.flatMap(({ entries = [], target }) =>
      entries.map(
        ({
          advisories = [],
          hardwareClassId = "nvidia",
          memoryFit = MemoryFit.FITS,
          freeMemoryFit = memoryFit,
          profileId,
          reason,
          startingTotalContextTokens,
          startingTotalContextSource = startingTotalContextTokens === undefined
            ? StartingTotalContextSource.UNSPECIFIED
            : StartingTotalContextSource.LARGEST_FIT,
          unsupportedReason = SizingScenarioUnsupportedReason.UNSPECIFIED,
        }) => ({
          advisories,
          modelId,
          profileId,
          reason,
          status: reason ? ModelCompatibilityStatus.CANNOT_RUN : ModelCompatibilityStatus.RUNS,
          target,
          ...(reason
            ? {}
            : {
                freeMemoryFit,
                hardwareClassId,
                memoryFit,
                startingTotalContextSource,
                startingTotalContextTokens,
                unsupportedReason,
              }),
        }),
      ),
    ),
    targets: targets.map(({ reason, status, target }) => ({ reason, status, target })),
  });
}

/** A compatibility loader answering every model with the given targets. */
export function compatibilityLoader(...targets: TargetCompatibilityInit[]) {
  return vi.fn((modelId: string) => Promise.resolve(compatibilityResponse(modelId, ...targets)));
}

/** One hardware class's estimate of a profile, in bytes; host memory is pinned beside it. */
export interface ClassEstimateInit {
  acceleratorMemoryBytes?: bigint;
  hardwareClassId: string;
  hostMemoryBytes?: bigint;
}

/** The target-independent sizing of one profile, with the given estimates per hardware class. */
export function profileSizing(
  modelId: string,
  profileId: string,
  ...classes: ClassEstimateInit[]
): MessageInitShape<typeof ModelProfileSizingSchema> {
  return {
    hardwareClassEstimates: classes.map(
      ({ acceleratorMemoryBytes, hardwareClassId, hostMemoryBytes }) => ({
        estimates: [
          ...(acceleratorMemoryBytes === undefined
            ? []
            : [{ resource: "acceleratorMemory", unit: "bytes", value: acceleratorMemoryBytes }]),
          ...(hostMemoryBytes === undefined
            ? []
            : [{ resource: "hostMemory", unit: "bytes", value: hostMemoryBytes }]),
        ],
        hardwareClassId,
      }),
    ),
    modelId,
    profileId,
  };
}
