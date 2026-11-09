import { type LocalDeploymentConfigurationValues } from "@/features/models/runtime/local-deployment-configuration";
import { type PlacementPreview } from "@/gen/dotcore/capacity/v1/placement_preview_pb";
import { type ExactPlacement } from "@/gen/dotcore/deployment/v1/placement_pb";
import { useKeyedRead } from "@/hooks/use-keyed-read";

/**
 * Estimates one local deployment configuration on its target, the request's key naming it. A
 * result shows only for the configuration it was asked for, and an unchanged key reuses the
 * request in flight, so re-running effects ask once. A new revision reads the same configuration
 * again in the background, so the target's memory stays current.
 */
export function useResourceEstimate(
  onEstimate: ResourceEstimateLoader | undefined,
  request: ResourceEstimateRequest | undefined,
): ResourceEstimate {
  const read = useKeyedRead(
    onEstimate && request
      ? {
          failureLabel: "Resource estimate unavailable",
          key: request.key,
          load: async () => ({
            estimate: await onEstimate(request.values),
            placement: request.values.placement,
          }),
          revision: request.revision,
        }
      : undefined,
  );

  return {
    error: read.error,
    launchRejection: read.value?.estimate.launchRejection,
    loadingVisible: read.loadingVisible,
    pending: read.pending,
    placement: read.value?.placement,
    placementPreview: read.value?.estimate.placementPreview,
    stale: read.refreshFailed,
  };
}

/** Asks the model service for the estimate of one configuration. */
export type ResourceEstimateLoader = (
  values: LocalResourceEstimateValues,
) => Promise<LocalResourceEstimate>;

/** What the model service estimated for one configuration. */
export interface LocalResourceEstimate {
  /** Why a deploy with this configuration would be rejected, for the operator. */
  launchRejection?: string;
  /** The placement's target memory with the configuration added; absent without a placement. */
  placementPreview?: PlacementPreview;
}

/** One configuration to estimate, with a key that changes whenever its values do. */
export interface ResourceEstimateRequest {
  key: string;
  /** Reads the configuration again when it changes, such as on each compatibility poll. */
  revision?: number;
  values: LocalResourceEstimateValues;
}

export interface LocalResourceEstimateValues {
  configuration: LocalDeploymentConfigurationValues;
  /** The selected target, whose hardware decides the estimate when the profile supports several. */
  placement?: ExactPlacement;
  profileId: string;
}

/**
 * The estimate of the current configuration: the placement preview, why a deploy would be
 * rejected, and whether the read is pending or failed. While a read is pending, the values are the
 * previous configuration's last successful estimate, if any, and `placement` names where it was
 * read for.
 */
export interface ResourceEstimate {
  error?: string;
  launchRejection?: string;
  /** Whether pending feedback should show, after the reveal delay and for its minimum time. */
  loadingVisible: boolean;
  pending: boolean;
  /** The placement the shown values were estimated for. */
  placement?: ExactPlacement;
  placementPreview?: PlacementPreview;
  /** Whether the latest refresh failed, so the values may be outdated. */
  stale?: boolean;
}
