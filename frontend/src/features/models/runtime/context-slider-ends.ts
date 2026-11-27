import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { Model, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterDefinition } from "@/gen/dotcore/catalog/v1/parameter_pb";
import { parseSafeInteger, safeNumber } from "@/lib/safe-integer";

import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  engineParameterInputs,
  maximumContext,
  PARALLEL_REQUESTS_PARAMETER_ID,
  profileStartingValues,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "./engine-parameter-values";
import { profileEngineRelease } from "./local-deployment-configuration";

/**
 * Where the context size sliders start and end, in tokens, all taken from declared data: the
 * engine's context parameters, the model's maximum context and the parallel requests.
 */
export interface ContextSliderEnds {
  /** The left end of both sliders: the lowest minimum the context parameters declare. */
  minimum: number;
  /** The right end of the context per request slider. */
  perRequest: number;
  /** The right end of the total context slider. */
  total: number;
}

/**
 * The slider ends of one engine configuration. The context per request ends at the smaller of the
 * model's maximum context and the engine's own maximum, or the model's alone when the engine
 * declares none; the total context ends at that context for each parallel request, within the
 * engine's total maximum. Undefined without a context parameter or a context per request end.
 */
export function contextSliderEnds({
  contextPerRequest,
  maximumContext: modelMaximum,
  parallelRequests,
  totalContext,
}: {
  contextPerRequest?: ParameterDefinition;
  /** The model's maximum context, or its configuration's lower one. */
  maximumContext?: number;
  /** The requests generated at once; one when the engine sets no count. */
  parallelRequests: number;
  totalContext?: ParameterDefinition;
}): ContextSliderEnds | undefined {
  const perRequestRange = integerRange(contextPerRequest);
  const totalRange = integerRange(totalContext);
  const minimums = [perRequestRange, totalRange].flatMap((range) => range?.minimum ?? []);
  const perRequest = smallest([modelMaximum, perRequestRange?.maximum]);
  if (!minimums.length || perRequest === undefined) return undefined;

  return {
    minimum: Math.min(...minimums),
    perRequest,
    total: Math.min(perRequest * parallelRequests, totalRange?.maximum ?? Infinity),
  };
}

/**
 * The slider ends of a profile's configuration as the catalog sizes it: the engine release's
 * context parameters and the profile's default parallel requests.
 */
export function profileContextSliderEnds(
  model: Model,
  profile: ModelProfile,
  engineReleases: readonly EngineRelease[],
): ContextSliderEnds | undefined {
  const release = profileEngineRelease(model, profile, engineReleases);
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile.configurationIds[0],
  );
  const parameter = (parameterId: string) =>
    release?.parameterSchema?.parameters.find(
      (candidate) =>
        candidate.parameterId === parameterId && candidate.parameterType.case === "unsignedInteger",
    );
  const starting = engineParameterInputs(profileStartingValues(release, profile));
  return contextSliderEnds({
    contextPerRequest: parameter(CONTEXT_PER_REQUEST_PARAMETER_ID),
    maximumContext: maximumContext(model, configuration),
    parallelRequests: requestCount(starting.get(PARALLEL_REQUESTS_PARAMETER_ID)),
    totalContext: parameter(TOTAL_CONTEXT_PARAMETER_ID),
  });
}

/**
 * The ends that cover every given configuration, for one scenario across profiles: the lowest
 * minimum and the largest right ends. Undefined without any.
 */
export function widestContextSliderEnds(
  ends: readonly ContextSliderEnds[],
): ContextSliderEnds | undefined {
  if (!ends.length) return undefined;

  return {
    minimum: Math.min(...ends.map((end) => end.minimum)),
    perRequest: Math.max(...ends.map((end) => end.perRequest)),
    total: Math.max(...ends.map((end) => end.total)),
  };
}

/**
 * The parallel requests an input holds, or one while it holds no valid count, such as during
 * typing or for an engine without the parameter.
 */
export function requestCount(input: string | undefined): number {
  const count = parseSafeInteger(input?.trim() ?? "");
  return count === undefined || count === 0 ? 1 : count;
}

// A context parameter's declared range as numbers; a maximum too large for a number is left open.
function integerRange(
  parameter: ParameterDefinition | undefined,
): { maximum?: number; minimum: number } | undefined {
  if (parameter?.parameterType.case !== "unsignedInteger") return undefined;

  const { maximum, minimum } = parameter.parameterType.value;
  return {
    maximum: maximum === undefined ? undefined : safeNumber(maximum),
    minimum: Number(minimum),
  };
}

function smallest(values: readonly (number | undefined)[]): number | undefined {
  const known = values.filter((value): value is number => value !== undefined);
  return known.length ? Math.min(...known) : undefined;
}
