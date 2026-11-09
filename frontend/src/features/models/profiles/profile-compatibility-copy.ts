import { executionTargetKey, placedExecutionTargetName } from "@/domain/execution-target";
import type { OciPlatform } from "@/gen/dotcore/catalog/v1/oci_pb";
import {
  LocalModelDeploymentAdvisory,
  type ModelCompatibilityReason,
  ModelCompatibilityReasonCode,
} from "@/gen/dotcore/model/v1/service_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { formatBytes } from "@/lib/format";

import { engineKindLabel, type ProfileRow } from "./model-row";
import type { CompatibilityVerdict, TargetIdentity } from "./profile-compatibility";
import type { ProfileStatus } from "./profile-fields";

/** What the copy reads beyond the verdict: the profile and the site's configured targets. */
export interface CompatibilityCopyContext {
  /** The profile, for its engine and declared memory minimums. */
  profile: ProfileRow;
  /** The configured execution targets, for machine names and target platforms. */
  targets: readonly ConfiguredExecutionTarget[];
}

/**
 * The status line of a profile from its verdict: why it can't run or is unknown, or its advisories
 * when it runs with some. Undefined when it runs without advisories or nothing is known.
 */
export function profileCompatibilityStatus(
  verdict: CompatibilityVerdict | undefined,
  context: CompatibilityCopyContext,
): ProfileStatus | undefined {
  if (!verdict) return undefined;

  if (verdict.status === "runs") {
    const advisories = verdict.advisories.flatMap(
      (advisory) => compatibilityAdvisoryText(advisory, context.profile) ?? [],
    );
    return advisories.length ? { text: advisories.join("; "), tone: "advisory" } : undefined;
  }

  return {
    text: compatibilityReasonText(verdict, context),
    tone: verdict.status === "unknown" ? "unknown" : "blocked",
  };
}

/** Why a profile can't run on, or can't be judged for, the verdict's target. */
export function compatibilityReasonText(
  verdict: CompatibilityVerdict,
  context: CompatibilityCopyContext,
): string {
  const { reason, target } = verdict;
  const machine = machineName(target, context.targets);

  switch (reason?.code) {
    case ModelCompatibilityReasonCode.TARGET_NOT_REPORTING:
      return `${machine} isn't reporting`;
    case ModelCompatibilityReasonCode.NVIDIA_DRIVER_UNKNOWN:
      return `${machine}'s NVIDIA driver version is unknown`;
    case ModelCompatibilityReasonCode.ACCELERATOR_MEMORY_UNKNOWN:
      return `${machine}'s GPU memory type is unknown`;
    case ModelCompatibilityReasonCode.TARGET_PLATFORM_INVALID:
      return `${placedExecutionTargetName(target, context.targets)} has no supported platform configured`;
    case ModelCompatibilityReasonCode.PLATFORM_MISMATCH:
      return `${machine} reports ${platformText(reason.observedPlatform)}, not the configured ${platformText(reason.configuredPlatform)}`;
    case ModelCompatibilityReasonCode.NO_ACCELERATOR:
      return `${machine} reports no usable GPU`;
    case ModelCompatibilityReasonCode.NO_MATCHING_ENGINE_ARTIFACT:
      return missingArtifactText(reason, context.profile, targetPlatform(target, context.targets));
    case ModelCompatibilityReasonCode.NO_SUPPORTED_HARDWARE_CLASS:
      return `Not supported on ${machine}'s hardware`;
    case ModelCompatibilityReasonCode.NVIDIA_DRIVER_BELOW_MINIMUM:
      return `Needs NVIDIA driver ${reason.minimumNvidiaDriverMajorVersion} or later (has ${reason.observedNvidiaDriverMajorVersion})`;
    case ModelCompatibilityReasonCode.ACCELERATOR_DEVICE_COUNT_MISMATCH:
      return deviceCountText(reason);
    case ModelCompatibilityReasonCode.PROFILE_UNRESOLVABLE:
      return `Its launch settings don't resolve on ${machine}`;
    default:
      return verdict.status === "unknown"
        ? `Can't check ${machine} right now`
        : `Can't run on ${machine}`;
  }
}

/**
 * The text of an advisory on a profile that runs, from the profile's declared minimums. Undefined
 * for an unspecified advisory.
 */
export function compatibilityAdvisoryText(
  advisory: LocalModelDeploymentAdvisory,
  profile: ProfileRow,
): string | undefined {
  switch (advisory) {
    case LocalModelDeploymentAdvisory.UNIFIED_MEMORY_POOL_BELOW_DEVICE_MINIMUM: {
      const minimum = Math.max(
        0,
        ...profile.acceleratorRequirements.map(
          ({ minimumMemoryBytesPerDevice }) => minimumMemoryBytesPerDevice,
        ),
      );
      return minimum
        ? `System memory is below this profile's ${formatBytes(BigInt(minimum))} minimum`
        : "System memory is below this profile's minimum";
    }
    case LocalModelDeploymentAdvisory.HOST_KV_OFFLOAD_SHARES_MEMORY:
      return "KV cache offload adds no capacity on this target";
    default:
      return undefined;
  }
}

/** A machine's display name, or its SiteNode ID when it has none or isn't configured. */
export function machineName(
  target: TargetIdentity,
  targets: readonly ConfiguredExecutionTarget[],
): string {
  const configured = configuredTarget(target, targets);
  return configured?.siteNodeDisplayName || target.siteNodeId;
}

// Without a platform match, the platforms are missing; with one, the accelerator vendor is. With
// no build the target's runtime can run, the profile's declared platforms tell whether the
// platform is the cause.
function missingArtifactText(
  reason: ModelCompatibilityReason,
  profile: ProfileRow,
  platform: string | undefined,
): string {
  const supported = reason.supportedSelectors;
  if (!supported.length) {
    const declared = unique(
      profile.platforms.map(
        ({ architecture, operatingSystem }) => `${operatingSystem}/${architecture}`,
      ),
    );
    return platform && declared.length && !declared.includes(platform)
      ? `Needs ${declared.join(" or ")}`
      : `No ${engineName(profile) ?? "engine"} build for this target's runtime`;
  }

  // The target's platform label carries no variant, so artifacts match it on OS and architecture.
  const onPlatform = supported.filter(
    (entry) =>
      entry.platform &&
      `${entry.platform.operatingSystem}/${entry.platform.architecture}` === platform,
  );
  if (!onPlatform.length) {
    return `Needs ${unique(supported.map((entry) => platformText(entry.platform))).join(" or ")}`;
  }
  return `Needs ${unique(onPlatform.flatMap((entry) => entry.accelerators).map(vendorText)).join(" or ")}`;
}

function vendorText(vendor: string): string {
  switch (vendor) {
    case "nvidia":
      return "an NVIDIA GPU";
    case "apple":
      return "Apple silicon";
    default:
      return `an accelerator from ${vendor}`;
  }
}

function deviceCountText(reason: ModelCompatibilityReason): string {
  const required = reason.requiredAcceleratorDeviceCount;
  return `Needs ${required} ${required === 1 ? "GPU" : "GPUs"}, target has ${reason.acceleratorDeviceCount}`;
}

function engineName(profile: ProfileRow): string | undefined {
  const [kind] = profile.engineKinds;
  return kind ? engineKindLabel(kind) : undefined;
}

function targetPlatform(
  target: TargetIdentity,
  targets: readonly ConfiguredExecutionTarget[],
): string | undefined {
  return configuredTarget(target, targets)?.platformLabel || undefined;
}

function configuredTarget(
  target: TargetIdentity,
  targets: readonly ConfiguredExecutionTarget[],
): ConfiguredExecutionTarget | undefined {
  const key = executionTargetKey(target);
  return targets.find((candidate) => executionTargetKey(candidate) === key);
}

function platformText(platform: OciPlatform | undefined): string {
  if (!platform) return "an unknown platform";
  return [platform.operatingSystem, platform.architecture, platform.variant]
    .filter(Boolean)
    .join("/");
}

function unique<T>(values: readonly T[]): T[] {
  return [...new Set(values)];
}
