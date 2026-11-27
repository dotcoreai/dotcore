import type { ProfileCompatibilityLoader } from "@/features/models/profiles/use-profile-compatibility";
import { maximumContext } from "@/features/models/runtime/engine-parameter-values";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ListModelProfileCompatibilityResponse } from "@/gen/dotcore/model/v1/service_pb";

import type { ResourceEstimateLoader } from "./use-resource-estimate";

/**
 * An estimate loader for `model` that answers with the context per request the backend fills for
 * engines whose requests share the total context: the total context, capped by the maximum context,
 * when the configuration leaves the context per request out.
 */
export function sharedContextEstimate(model: Model): ResourceEstimateLoader {
  return ({ configuration, profileId }) => {
    const values = configuration.engineParameterValues.values;
    const unsigned = (parameterId: string) => {
      const value = values.find((candidate) => candidate.parameterId === parameterId)?.value;
      return value?.case === "unsignedInteger" ? Number(value.value) : undefined;
    };
    const total = unsigned("total-context");
    const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
    const modelConfiguration = model.configurations.find(
      (candidate) => candidate.configurationId === profile?.configurationIds[0],
    );
    const maximum = maximumContext(model, modelConfiguration);
    const filled =
      total === undefined || unsigned("context-per-request") !== undefined
        ? undefined
        : Math.min(total, maximum ?? total);
    return Promise.resolve({ filledContextPerRequest: filled });
  };
}

/**
 * Pairs `listProfileCompatibility` with an estimate loader answering like the backend at a
 * profile's values: the context per request `sharedContextEstimate` fills, and the largest context
 * per request and total context of the latest compatibility read's entry for the profile and
 * placement. The pass-through
 * loader records each read without adding calls, so tests keep counting the reads they asked for.
 */
export function compatibilityFitEstimates(
  model: Model,
  listProfileCompatibility: ProfileCompatibilityLoader | undefined,
): {
  listProfileCompatibility?: ProfileCompatibilityLoader;
  onEstimate: ResourceEstimateLoader;
} {
  let latest: ListModelProfileCompatibilityResponse | undefined;
  const filled = sharedContextEstimate(model);
  return {
    listProfileCompatibility:
      listProfileCompatibility &&
      (async (...read: Parameters<ProfileCompatibilityLoader>) => {
        const response = await listProfileCompatibility(...read);
        latest = response;
        return response;
      }),
    onEstimate: async (values) => {
      const entry = latest?.entries.find(
        (candidate) =>
          candidate.profileId === values.profileId &&
          candidate.target?.siteNodeId === values.placement?.siteNodeId &&
          candidate.target?.executionTargetId === values.placement?.executionTargetId,
      );
      const number = (value: bigint | undefined) =>
        value === undefined ? undefined : Number(value);
      return {
        ...(await filled(values)),
        largestContextPerRequest: number(entry?.largestContextTokensPerRequest),
        largestTotalContext: number(entry?.largestTotalContextTokens),
      };
    },
  };
}
