import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type {
  Model,
  ModelEngineConfiguration,
  ModelProfile,
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

/** The context size parameters, which a launch sets on its own step apart from the runtime. */
export const CONTEXT_PARAMETER_IDS: ReadonlySet<string> = new Set([
  TOTAL_CONTEXT_PARAMETER_ID,
  CONTEXT_PER_REQUEST_PARAMETER_ID,
]);

/**
 * A profile's exposed engine parameters, grouped as the deploy dialog presents them: the context
 * sizes form the context group of the Context step. On the Runtime step, parallel requests come
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
 * Groups exposed engine parameters for the Context and Runtime steps. Host-memory parameters are
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

export function effectiveEngineParameters(
  release: EngineRelease | undefined,
  profile: ModelProfile | undefined,
): ParameterDefinition[] {
  const parameters = release?.parameterSchema?.parameters ?? [];
  if (!profile?.exposedParameterIds.length)
    return parameters.filter((parameter) => parameter.visibility !== ParameterVisibility.HIDDEN);

  return profile.exposedParameterIds.flatMap((parameterId) => {
    const parameter = parameters.find((candidate) => candidate.parameterId === parameterId);
    return parameter ? [parameter] : [];
  });
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
      values.push({ parameterId: parameter.parameterId, value: engineDefault } as ParameterValue);
    }
  }
  return { values } as ParameterValues;
}

export function engineParameterInputs(values: ParameterValues | undefined): Map<string, string> {
  return new Map(
    (values?.values ?? []).flatMap((parameter): Array<[string, string]> => {
      switch (parameter.value.case) {
        case "unsignedInteger":
        case "boolean":
          return [[parameter.parameterId, String(parameter.value.value)]];
        default:
          return [];
      }
    }),
  );
}

export function engineParameterValues(
  declared: ParameterValues | undefined,
  parameters: readonly ParameterDefinition[],
  inputs: ReadonlyMap<string, string>,
  derived: ReadonlySet<string> = noDerivedParameters,
): ParameterValues | undefined {
  if (parameters.some((parameter) => engineParameterError(parameter, inputs, derived)))
    return undefined;

  const omitted = parameters.filter((parameter) =>
    omitsOptionalValue(parameter, inputs.get(parameter.parameterId) ?? "", derived),
  );
  const edited = parameters
    .filter((parameter) => !omitted.includes(parameter))
    .map((parameter) =>
      editedEngineParameterValue(parameter, inputs.get(parameter.parameterId) ?? ""),
    );
  if (edited.some((entry) => entry === undefined)) return undefined;

  const values = (declared?.values ?? []).filter(
    (value) => !omitted.some((parameter) => parameter.parameterId === value.parameterId),
  );
  for (const entry of edited) {
    const index = values.findIndex((value) => value.parameterId === entry!.parameterId);
    if (index < 0) values.push(entry!);
    else values[index] = entry!;
  }
  return { values } as ParameterValues;
}

/**
 * Why a parameter's input in `inputs` can't launch, or undefined when it can: a value outside the
 * schema range, or a context per request above the total context. An empty input of a parameter
 * in `derived` is valid.
 */
export function engineParameterError(
  parameter: ParameterDefinition,
  inputs: ReadonlyMap<string, string>,
  derived: ReadonlySet<string> = noDerivedParameters,
): string | undefined {
  const input = inputs.get(parameter.parameterId) ?? "";
  if (omitsOptionalValue(parameter, input, derived)) return undefined;

  const value = editedEngineParameterValue(parameter, input);
  if (value) {
    const total = inputs.get(TOTAL_CONTEXT_PARAMETER_ID) ?? "";
    return parameter.parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID &&
      /^\d+$/.test(total) &&
      BigInt(input) > BigInt(total)
      ? `${parameter.displayName} must be at most the total context.`
      : undefined;
  }
  if (parameter.parameterType.case === "unsignedInteger") {
    const { maximum, minimum, unit } = parameter.parameterType.value;
    return `${parameter.displayName} must be an integer from ${minimum} to ${maximum} ${unit}.`;
  }
  return `${parameter.displayName} must be enabled or disabled.`;
}

/**
 * The short line under a parameter's field, a fixed one for the shared parameters every engine
 * declares; undefined for the others, whose unit the field shows. The schema description
 * documents the engine and stays out of the field.
 */
export function engineParameterHelp(parameter: ParameterDefinition): string | undefined {
  return sharedParameterHelp.get(parameter.parameterId);
}

/** Common context sizes, in tokens, offered as presets wherever a context size is entered. */
export const contextSizePresets: readonly number[] = [8192, 32768, 65536, 131072, 262144, 1048576];

const sharedParameterHelp: ReadonlyMap<string, string> = new Map([
  [TOTAL_CONTEXT_PARAMETER_ID, "Memory the model holds for all requests at once."],
  [PARALLEL_REQUESTS_PARAMETER_ID, "Requests generated at once; more wait in a queue."],
]);

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

// An empty context per request or sizing-derived parameter is left out rather than rejected.
function omitsOptionalValue(
  parameter: ParameterDefinition,
  input: string,
  derived: ReadonlySet<string>,
): boolean {
  return (
    input === "" &&
    (parameter.parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID ||
      derived.has(parameter.parameterId))
  );
}

function editedEngineParameterValue(
  parameter: ParameterDefinition,
  input: string,
): ParameterValue | undefined {
  switch (parameter.parameterType.case) {
    case "boolean":
      return input === "true" || input === "false"
        ? ({
            parameterId: parameter.parameterId,
            value: { case: "boolean", value: input === "true" },
          } as ParameterValue)
        : undefined;
    case "unsignedInteger": {
      if (!/^\d+$/.test(input)) return undefined;
      const value = BigInt(input);
      const { maximum, minimum } = parameter.parameterType.value;
      return value >= minimum && value <= maximum
        ? ({
            parameterId: parameter.parameterId,
            value: { case: "unsignedInteger", value },
          } as ParameterValue)
        : undefined;
    }
    default:
      return undefined;
  }
}

// The catalog default of a parameter, mirroring the backend helper; undefined when it declares none.
function parameterDefaultValue(
  parameter: ParameterDefinition,
): ParameterValue["value"] | undefined {
  const type = parameter.parameterType;
  switch (type.case) {
    case "boolean":
      return type.value.defaultValue === undefined
        ? undefined
        : { case: "boolean", value: type.value.defaultValue };
    case "unsignedInteger":
      return type.value.defaultValue === undefined
        ? undefined
        : { case: "unsignedInteger", value: type.value.defaultValue };
    case "stringEnum":
      return type.value.defaultValue === undefined
        ? undefined
        : { case: "string", value: type.value.defaultValue };
    default:
      return undefined;
  }
}
