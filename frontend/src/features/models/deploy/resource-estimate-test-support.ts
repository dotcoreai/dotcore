import { maximumContext } from "@/features/models/runtime/engine-parameter-values";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";

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
      (candidate) => candidate.configurationId === profile?.configurationId,
    );
    const maximum = maximumContext(model, modelConfiguration);
    const filled =
      total === undefined || unsigned("context-per-request") !== undefined
        ? undefined
        : Math.min(total, maximum ?? total);
    return Promise.resolve({ filledContextPerRequest: filled });
  };
}
