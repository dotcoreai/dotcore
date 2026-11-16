import {
  parameterDefaultValue,
  parameterValueCase,
  parameterView,
} from "@/domain/parameter-definitions";
import {
  omitsParameterValue,
  parameterInputError,
  parameterInputValue,
  type ParameterView,
  type ParameterViewValue,
} from "@/domain/parameter-view";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type {
  Model,
  ModelEngineConfiguration,
  ModelProfile,
  ParameterVisibilityOverride,
} from "@/gen/dotcore/catalog/v1/model_pb";
import {
  type ParameterDefinition,
  type ParameterValue,
  type ParameterValues,
  ParameterVisibility,
} from "@/gen/dotcore/catalog/v1/parameter_pb";

/** The shared engine parameter that sets the total context in tokens, declared by every engine. */
export const TOTAL_CONTEXT_PARAMETER_ID = "total-context";

/**
 * The shared engine parameter capping one request's context. It is optional: left empty, it is
 * omitted and resolution fills it from the maximum context and the total context.
 */
export const CONTEXT_PER_REQUEST_PARAMETER_ID = "context-per-request";

/** The shared engine parameter setting how many requests an engine generates at once. */
export const PARALLEL_REQUESTS_PARAMETER_ID = "parallel-requests";

/** The context size parameters, which lead the deploy dialog's Settings step. */
export const CONTEXT_PARAMETER_IDS: ReadonlySet<string> = new Set([
  TOTAL_CONTEXT_PARAMETER_ID,
  CONTEXT_PER_REQUEST_PARAMETER_ID,
]);

/**
 * A profile's exposed engine parameters, grouped as the deploy dialog presents them: the context
 * sizes form the context group of the Settings step. On the Advanced step, parallel requests come
 * first, host-memory parameters form their own group, and every other parameter stays in the
 * generic form.
 */
export interface RuntimeParameterGroups {
  context: ContextParameterGroup;
  hostMemory: ParameterDefinition[];
  other: ParameterDefinition[];
  parallelRequests?: ParameterDefinition;
}

/** The context size parameters a profile exposes; every engine declares both. */
export interface ContextParameterGroup {
  contextPerRequest?: ParameterDefinition;
  totalContext?: ParameterDefinition;
}

/**
 * Groups exposed engine parameters for the Settings and Advanced steps. Host-memory parameters are
 * the ones the model configuration's engine settings declare as host budgets (vLLM KV offload,
 * ninfer host context), so an engine without such a declaration keeps every parameter in the
 * generic group.
 */
export function runtimeParameterGroups(
  parameters: readonly ParameterDefinition[],
  configuration: ModelEngineConfiguration | undefined,
): RuntimeParameterGroups {
  const hostMemoryIds = hostMemoryParameterIds(configuration);
  const unsigned = (parameterId: string) =>
    parameters.find(
      (parameter) =>
        parameter.parameterId === parameterId && parameter.parameterType.case === "unsignedInteger",
    );
  const context = {
    contextPerRequest: unsigned(CONTEXT_PER_REQUEST_PARAMETER_ID),
    totalContext: unsigned(TOTAL_CONTEXT_PARAMETER_ID),
  };
  const parallelRequests = unsigned(PARALLEL_REQUESTS_PARAMETER_ID);
  const grouped = new Set([...Object.values(context), parallelRequests]);
  const rest = parameters.filter((parameter) => !grouped.has(parameter));

  return {
    context,
    hostMemory: rest.filter((parameter) => hostMemoryIds.has(parameter.parameterId)),
    other: rest.filter((parameter) => !hostMemoryIds.has(parameter.parameterId)),
    parallelRequests,
  };
}

/**
 * The model's maximum context, or its configuration's lower maximum. Undefined when neither is
 * known.
 */
export function maximumContext(
  model: Model,
  configuration: ModelEngineConfiguration | undefined,
): number | undefined {
  const maxima = [model.maximumContextTokens || undefined, configuration?.maximumContextTokens];
  const known = maxima.filter((maximum): maximum is number => maximum !== undefined);
  return known.length ? Math.min(...known) : undefined;
}

/**
 * The engine parameters the deploy dialog shows for `profile`, in schema order: each parameter's
 * visibility is the engine's unless the profile overrides it, and hidden ones are left out.
 */
export function effectiveEngineParameters(
  release: EngineRelease | undefined,
  profile: ModelProfile | undefined,
): ParameterDefinition[] {
  const overrides = profile?.engineParameterVisibilityOverrides ?? [];
  return (release?.parameterSchema?.parameters ?? []).filter(
    (parameter) => effectiveVisibility(parameter, overrides) !== ParameterVisibility.HIDDEN,
  );
}

/** A parameter's visibility under a profile's overrides: the override, else the engine's. */
export function effectiveVisibility(
  parameter: ParameterDefinition,
  overrides: readonly ParameterVisibilityOverride[],
): ParameterVisibility {
  return (
    overrides.find((override) => override.parameterId === parameter.parameterId)?.visibility ??
    parameter.visibility
  );
}

/**
 * The values a launch of `profile` starts from: the profile's defaults, then the engine default of
 * each parameter of the release schema the profile leaves out.
 */
export function profileStartingValues(
  release: EngineRelease | undefined,
  profile: ModelProfile | undefined,
): ParameterValues | undefined {
  if (!profile) return undefined;

  const values = [...(profile.engineParameterDefaults?.values ?? [])];
  for (const parameter of release?.parameterSchema?.parameters ?? []) {
    const engineDefault = parameterDefaultValue(parameter);
    if (
      engineDefault !== undefined &&
      !values.some((value) => value.parameterId === parameter.parameterId)
    ) {
      values.push(engineParameterValue(parameter.parameterId, engineDefault));
    }
  }
  return { values } as ParameterValues;
}

/** Form inputs holding each typed value, keyed by parameter ID. */
export function engineParameterInputs(values: ParameterValues | undefined): Map<string, string> {
  return new Map(
    (values?.values ?? []).flatMap((parameter): Array<[string, string]> =>
      parameter.value.case === undefined
        ? []
        : [[parameter.parameterId, String(parameter.value.value)]],
    ),
  );
}

/**
 * The values a launch sends: the declared values with each exposed parameter's input applied, and
 * the ones whose input leaves the value out removed. Undefined while any input is invalid.
 */
export function engineParameterValues(
  declared: ParameterValues | undefined,
  parameters: readonly ParameterDefinition[],
  inputs: ReadonlyMap<string, string>,
  derived: ReadonlySet<string> = noDerivedParameters,
): ParameterValues | undefined {
  if (parameters.some((parameter) => engineParameterError(parameter, inputs, derived)))
    return undefined;

  const omitted = new Set<string>();
  const edited: ParameterValue[] = [];
  for (const parameter of parameters) {
    const view = parameterView(parameter);
    if (!view) continue;
    const input = inputs.get(view.id) ?? "";
    const value = parameterInputValue(view, input);
    if (omitsEngineValue(view, input, derived)) omitted.add(view.id);
    else if (value !== undefined) edited.push(engineParameterValue(view.id, value));
  }

  const values = (declared?.values ?? []).filter((value) => !omitted.has(value.parameterId));
  for (const entry of edited) {
    const index = values.findIndex((value) => value.parameterId === entry.parameterId);
    if (index < 0) values.push(entry);
    else values[index] = entry;
  }
  return { values } as ParameterValues;
}

/**
 * Why a parameter's input in `inputs` can't launch, or undefined when it can: a value its type
 * refuses, or a context per request above the total context. An empty input is valid for a secret,
 * which is generated at deploy, for the context per request, and for a parameter in `derived`.
 */
export function engineParameterError(
  parameter: ParameterDefinition,
  inputs: ReadonlyMap<string, string>,
  derived: ReadonlySet<string> = noDerivedParameters,
): string | undefined {
  const view = parameterView(parameter);
  const input = inputs.get(parameter.parameterId) ?? "";
  if (!view || omitsEngineValue(view, input, derived)) return undefined;

  const typeError = parameterInputError(view, input);
  if (typeError) return typeError;

  const total = inputs.get(TOTAL_CONTEXT_PARAMETER_ID) ?? "";
  return view.id === CONTEXT_PER_REQUEST_PARAMETER_ID &&
    /^\d+$/.test(total) &&
    BigInt(input) > BigInt(total)
    ? `${view.label} must be at most the total context.`
    : undefined;
}

/** Common context sizes, in tokens, offered as presets wherever a context size is entered. */
export const contextSizePresets: readonly number[] = [8192, 32768, 65536, 131072, 262144, 1048576];

/** The unsigned integer value a deployment resolved for an engine parameter, if it holds one. */
export function resolvedEngineParameterValue(
  values: ParameterValues | undefined,
  parameterId: string,
): bigint | undefined {
  const parameter = values?.values.find(
    (candidate) =>
      candidate.parameterId === parameterId && candidate.value.case === "unsignedInteger",
  );
  return parameter?.value.case === "unsignedInteger" ? parameter.value.value : undefined;
}

function hostMemoryParameterIds(configuration: ModelEngineConfiguration | undefined): Set<string> {
  const engine = configuration?.engineConfiguration?.configuration;
  const id =
    engine?.case === "vllm"
      ? engine.value.fixedSettings?.kvOffloadingSizeGibParameterId
      : engine?.case === "ninfer"
        ? engine.value.fixedSettings?.hostContextMibParameterId
        : undefined;
  return new Set(id ? [id] : []);
}

const noDerivedParameters: ReadonlySet<string> = new Set();

// An empty context per request or sizing-derived parameter is left out rather than rejected, on
// top of the inputs every parameter type leaves out.
function omitsEngineValue(
  view: ParameterView,
  input: string,
  derived: ReadonlySet<string>,
): boolean {
  return (
    omitsParameterValue(view, input) ||
    (input === "" && (view.id === CONTEXT_PER_REQUEST_PARAMETER_ID || derived.has(view.id)))
  );
}

function engineParameterValue(parameterId: string, value: ParameterViewValue): ParameterValue {
  return { parameterId, value: parameterValueCase(value) } as ParameterValue;
}
