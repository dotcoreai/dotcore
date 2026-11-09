import { executionTargetKey } from "@/domain/execution-target";
import type { EngineParameterValues } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { MemoryFit as ProtoMemoryFit } from "@/gen/dotcore/deployment/v1/memory_fit_pb";
import {
  type ListModelProfileCompatibilityResponse,
  type LocalModelDeploymentAdvisory,
  type ModelCompatibilityReason,
  ModelCompatibilityStatus,
  type ModelProfileSizing,
  SizingScenarioUnsupportedReason,
  StartingTotalContextSource as ProtoStartingTotalContextSource,
} from "@/gen/dotcore/model/v1/service_pb";

import {
  type HardwareClassEstimate,
  profileEstimateKey,
  type ProfileEstimates,
} from "./catalog-model-row";

/** One configured execution target, identified across the site. */
export interface TargetIdentity {
  executionTargetId: string;
  siteNodeId: string;
}

/**
 * Where the Runs on filter looks for profiles that can run: every configured model execution
 * target of the site, or the targets of one machine.
 */
export type RunsOnScope = { kind: "site" } | { kind: "node"; siteNodeId: string };

/** Where a starting total context comes from: the largest fit, or the profile's default. */
export type StartingTotalContextSource = "largestFit" | "profileDefault";

/** Whether a profile runs, can't run, or can't be judged from the target's current facts. */
export type CompatibilityStatus = "cannot-run" | "runs" | "unknown";

/**
 * Whether a profile's estimate fits a target's memory, exceeds it, can't be judged, or can't be
 * sized because the profile can't serve the scenario on the target's hardware class.
 */
export type MemoryFit = "exceeds" | "fits" | "unknown" | "unsupported";

/**
 * The memory a profile's fit is judged against: what is free now beside the running models, less
 * a small margin, or the target's total memory.
 */
export type MemoryBasis = "free" | "total";

/** The memory basis profile tables start on. */
export const defaultMemoryBasis: MemoryBasis = "free";

/** What the compatibility read says about one profile on one target, or over a scope. */
export interface CompatibilityVerdict {
  /** Non-blocking advisories of a profile that runs. */
  advisories: readonly LocalModelDeploymentAdvisory[];
  /** The hardware class the target's facts select for a profile that runs. */
  hardwareClassId?: string;
  /** The advisory memory fit of a profile that runs, against what is free now. */
  freeMemoryFit?: MemoryFit;
  /** The advisory memory fit of a profile that runs, against the target's total memory. */
  memoryFit?: MemoryFit;
  /**
   * The total context a deployment starts from on the target: the largest fit in whole 1024-token
   * blocks, or the profile's default without one.
   */
  startingTotalContextTokens?: number;
  /** Whether the starting total context comes from the largest fit or the profile's default. */
  startingTotalContextSource?: StartingTotalContextSource;
  /** Why the profile can't run or is unknown; absent when it runs. */
  reason?: ModelCompatibilityReason;
  status: CompatibilityStatus;
  /** The target the verdict comes from, which its reason names. */
  target: TargetIdentity;
  /** Why the profile can't serve the scenario there, for an unsupported memory fit. */
  unsupportedReason?: SizingScenarioUnsupportedReason;
}

/**
 * A compatibility read indexed for lookups: the evaluated targets in the read's order, each
 * target's own verdict, each profile's verdict on the targets that pass target-level checks, and
 * each profile's memory estimates per supported hardware class.
 */
export interface ProfileCompatibility {
  /**
   * Each profile's engine parameter values: its defaults, with the scenario's values applied in a
   * read for a scenario, hidden parameters included and derived values left out. A profile the
   * scenario can't serve has no entry.
   */
  engineValues: ReadonlyMap<string, EngineParameterValues>;
  /** Evaluated configured model execution targets, ordered by node then target. */
  targets: readonly TargetIdentity[];
  entries: ReadonlyMap<string, CompatibilityVerdict>;
  /**
   * Each profile's estimates on the hardware classes that report accelerator memory, whether or
   * not any target reports. A profile without such a class has no entry.
   */
  estimates: ProfileEstimates;
  targetVerdicts: ReadonlyMap<string, CompatibilityVerdict>;
  /** Why each profile the scenario can't serve on any hardware class can't, keyed like estimates. */
  unsupported: ReadonlyMap<string, SizingScenarioUnsupportedReason>;
  /** The hardware classes each profile can't serve the scenario on, keyed like estimates. */
  unsupportedClasses: ReadonlyMap<string, readonly string[]>;
}

/** Indexes a compatibility read by target and by model, profile, and target. */
export function indexProfileCompatibility(
  response: ListModelProfileCompatibilityResponse,
): ProfileCompatibility {
  const targets = response.targets.flatMap(({ target }) => (target ? [identity(target)] : []));
  const targetVerdicts = new Map(
    response.targets.flatMap((entry) =>
      entry.target
        ? [
            [
              executionTargetKey(entry.target),
              verdict(entry.target, { ...entry, advisories: [] }),
            ] as const,
          ]
        : [],
    ),
  );
  const entries = new Map(
    response.entries.flatMap((entry) =>
      entry.target
        ? [
            [
              entryKey(entry.modelId, entry.profileId, entry.target),
              verdict(entry.target, entry),
            ] as const,
          ]
        : [],
    ),
  );

  return {
    engineValues: new Map(
      response.profiles.flatMap(({ engineParameterValues, modelId, profileId }) =>
        engineParameterValues
          ? [[profileEstimateKey(modelId, profileId), engineParameterValues] as const]
          : [],
      ),
    ),
    entries,
    estimates: profileEstimates(response.profiles),
    targetVerdicts,
    targets,
    unsupportedClasses: new Map(
      response.profiles.flatMap(({ modelId, profileId, unsupportedHardwareClassIds }) =>
        unsupportedHardwareClassIds.length
          ? [[profileEstimateKey(modelId, profileId), [...unsupportedHardwareClassIds]] as const]
          : [],
      ),
    ),
    unsupported: new Map(
      response.profiles.flatMap(({ modelId, profileId, unsupportedReason }) =>
        unsupportedReason === SizingScenarioUnsupportedReason.UNSPECIFIED
          ? []
          : [[profileEstimateKey(modelId, profileId), unsupportedReason] as const],
      ),
    ),
  };
}

/**
 * A profile's verdict on one target: the target's own verdict when it fails target-level checks,
 * otherwise the profile's entry. Undefined when the read didn't evaluate the target or profile.
 */
export function profileTargetVerdict(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  target: TargetIdentity,
): CompatibilityVerdict | undefined {
  const targetVerdict = compatibility.targetVerdicts.get(executionTargetKey(target));
  if (!targetVerdict || targetVerdict.status !== "runs") return targetVerdict;
  return compatibility.entries.get(entryKey(modelId, profileId, target));
}

/**
 * A profile's verdict over a scope, derived from its targets: it runs when one target runs it
 * (preferring one without advisories), else it is unknown when one target is unknown, else it
 * can't run. Undefined when the scope holds no evaluated target.
 */
export function profileScopeVerdict(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  scope: RunsOnScope,
): CompatibilityVerdict | undefined {
  const verdicts = profileTargetVerdicts(compatibility, modelId, profileId, scope);
  const runs = verdicts.filter(({ status }) => status === "runs");

  return (
    runs.find(({ advisories }) => !advisories.length) ??
    runs[0] ??
    verdicts.find(({ status }) => status === "unknown") ??
    verdicts[0]
  );
}

/**
 * A profile's verdict on each evaluated target of a scope, in the read's order, skipping targets
 * where the read has no verdict for the profile.
 */
export function profileTargetVerdicts(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  scope: RunsOnScope = { kind: "site" },
): CompatibilityVerdict[] {
  return compatibility.targets
    .filter((target) => inScope(target, scope))
    .flatMap((target) => profileTargetVerdict(compatibility, modelId, profileId, target) ?? []);
}

/** A verdict's memory fit on a basis. */
export function verdictFit(
  verdict: CompatibilityVerdict | undefined,
  basis: MemoryBasis,
): MemoryFit | undefined {
  return basis === "free" ? verdict?.freeMemoryFit : verdict?.memoryFit;
}

/**
 * A profile's memory fit on a basis over one or more scopes, from the targets that run it: it fits
 * when one of them fits, else it is unknown, exceeds, or is unsupported, in that order. Undefined
 * when no target in the scopes runs it.
 */
export function profileScopeFit(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  scope: RunsOnScope | readonly RunsOnScope[],
  basis: MemoryBasis,
): { fit: MemoryFit; verdict: CompatibilityVerdict } | undefined {
  const scopes: readonly RunsOnScope[] = Array.isArray(scope) ? scope : [scope as RunsOnScope];
  return profileTargetVerdicts(compatibility, modelId, profileId)
    .flatMap((verdict) => {
      const fit = verdictFit(verdict, basis);
      return verdict.status === "runs" &&
        fit &&
        scopes.some((candidate) => inScope(verdict.target, candidate))
        ? [{ fit, verdict }]
        : [];
    })
    .sort((left, right) => fitRank(left.fit) - fitRank(right.fit))[0];
}

/**
 * The starting total context of the targets in the scopes that run a profile when it comes from
 * their largest fit, the most any of them starts from. Undefined when none starts from a fit.
 */
export function profileScopeStartingTotalContext(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  scopes: readonly RunsOnScope[],
): number | undefined {
  const starting = profileTargetVerdicts(compatibility, modelId, profileId).flatMap((verdict) => {
    const tokens = fitStartingTotalContext(verdict);
    return verdict.status === "runs" &&
      tokens !== undefined &&
      scopes.some((scope) => inScope(verdict.target, scope))
      ? [tokens]
      : [];
  });
  return starting.length ? Math.max(...starting) : undefined;
}

/**
 * A verdict's starting total context when it comes from the largest fit. Without a fit of one
 * block, the starting value is the profile's default, which the launch already holds.
 */
export function fitStartingTotalContext(
  verdict: CompatibilityVerdict | undefined,
): number | undefined {
  return verdict?.startingTotalContextSource === "largestFit"
    ? verdict.startingTotalContextTokens
    : undefined;
}

/**
 * The hardware classes the targets of a scope that run a profile select for it, in the read's
 * order. A machine has one memory architecture, so its targets select one class.
 */
export function profileScopeClasses(
  compatibility: ProfileCompatibility,
  modelId: string,
  profileId: string,
  scope: RunsOnScope,
): string[] {
  return [
    ...new Set(
      profileTargetVerdicts(compatibility, modelId, profileId, scope).flatMap(
        ({ hardwareClassId, status }) =>
          status === "runs" && hardwareClassId ? [hardwareClassId] : [],
      ),
    ),
  ];
}

/** Whether a target belongs to a scope. */
export function inScope(target: TargetIdentity, scope: RunsOnScope): boolean {
  return scope.kind === "site" || target.siteNodeId === scope.siteNodeId;
}

/** The filter value of a scope: `site` or `node:<node>`. */
export function runsOnScopeKey(scope: RunsOnScope): string {
  switch (scope.kind) {
    case "site":
      return "site";
    case "node":
      return `node:${scope.siteNodeId}`;
  }
}

const acceleratorMemoryResource = "acceleratorMemory";
const hostMemoryResource = "hostMemory";

function profileEstimates(profiles: readonly ModelProfileSizing[]): ProfileEstimates {
  return new Map(
    profiles.flatMap(({ hardwareClassEstimates, modelId, profileId }) => {
      const classes = hardwareClassEstimates.flatMap(
        ({ estimates, hardwareClassId }): HardwareClassEstimate[] => {
          const value = (resource: string) =>
            estimates.find((candidate) => candidate.resource === resource)?.value;
          const acceleratorMemoryBytes = value(acceleratorMemoryResource);
          return acceleratorMemoryBytes === undefined
            ? []
            : [
                {
                  acceleratorMemoryBytes,
                  hardwareClassId,
                  hostMemoryBytes: value(hostMemoryResource),
                },
              ];
        },
      );
      return classes.length ? [[profileEstimateKey(modelId, profileId), classes] as const] : [];
    }),
  );
}

function verdict(
  target: TargetIdentity,
  entry: {
    advisories: readonly LocalModelDeploymentAdvisory[];
    hardwareClassId?: string;
    freeMemoryFit?: ProtoMemoryFit;
    memoryFit?: ProtoMemoryFit;
    reason?: ModelCompatibilityReason;
    startingTotalContextSource?: ProtoStartingTotalContextSource;
    startingTotalContextTokens?: number;
    status: ModelCompatibilityStatus;
    unsupportedReason?: SizingScenarioUnsupportedReason;
  },
): CompatibilityVerdict {
  // An unsupported scenario is not evaluated, so its reason, not the fit, reports it.
  const unsupportedReason =
    entry.unsupportedReason === SizingScenarioUnsupportedReason.UNSPECIFIED
      ? undefined
      : entry.unsupportedReason;
  const fit = (value: ProtoMemoryFit | undefined) =>
    unsupportedReason !== undefined
      ? "unsupported"
      : value === undefined
        ? undefined
        : memoryFits[value];
  return {
    advisories: entry.advisories,
    freeMemoryFit: fit(entry.freeMemoryFit),
    hardwareClassId: entry.hardwareClassId || undefined,
    memoryFit: fit(entry.memoryFit),
    reason: entry.reason,
    startingTotalContextSource:
      entry.startingTotalContextSource === undefined
        ? undefined
        : startingSources[entry.startingTotalContextSource],
    startingTotalContextTokens: entry.startingTotalContextTokens,
    status: compatibilityStatus(entry.status),
    target: identity(target),
    unsupportedReason,
  };
}

const startingSources: Record<
  ProtoStartingTotalContextSource,
  StartingTotalContextSource | undefined
> = {
  [ProtoStartingTotalContextSource.UNSPECIFIED]: undefined,
  [ProtoStartingTotalContextSource.LARGEST_FIT]: "largestFit",
  [ProtoStartingTotalContextSource.PROFILE_DEFAULT]: "profileDefault",
};

const memoryFits: Record<ProtoMemoryFit, MemoryFit | undefined> = {
  [ProtoMemoryFit.UNSPECIFIED]: undefined,
  [ProtoMemoryFit.FITS]: "fits",
  [ProtoMemoryFit.EXCEEDS]: "exceeds",
  [ProtoMemoryFit.UNKNOWN]: "unknown",
};

function fitRank(fit: MemoryFit | undefined): number {
  switch (fit) {
    case "fits":
      return 0;
    case "unknown":
      return 1;
    case "exceeds":
      return 2;
    default:
      return 3;
  }
}

// An unspecified status can't be relied on, so it reads as unknown.
function compatibilityStatus(status: ModelCompatibilityStatus): CompatibilityStatus {
  switch (status) {
    case ModelCompatibilityStatus.RUNS:
      return "runs";
    case ModelCompatibilityStatus.CANNOT_RUN:
      return "cannot-run";
    default:
      return "unknown";
  }
}

function identity({ executionTargetId, siteNodeId }: TargetIdentity): TargetIdentity {
  return { executionTargetId, siteNodeId };
}

function entryKey(modelId: string, profileId: string, target: TargetIdentity): string {
  return `${modelId}\0${profileId}\0${executionTargetKey(target)}`;
}
