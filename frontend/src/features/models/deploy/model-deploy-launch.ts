import { create } from "@bufbuild/protobuf";
import { useMemo } from "react";

import type { deploymentExposure } from "@/features/models/deployment-exposure";
import { configurationSpeculativeOptions } from "@/features/models/profiles/model-row";
import {
  sameSizingScenario,
  scenarioFromInputs,
  type ScenarioInput,
  type SizingScenario,
} from "@/features/models/profiles/sizing-scenario";
import {
  CONTEXT_PARAMETER_IDS,
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  effectiveEngineParameters,
  engineParameterInputs as engineParameterInputsOf,
  PARALLEL_REQUESTS_PARAMETER_ID,
  profileStartingValues,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import {
  configurationKvCachePrecision,
  overriddenKvCachePrecision,
} from "@/features/models/runtime/kv-cache-precision";
import {
  type LocalDeploymentConfigurationValues,
  profileEngineRelease,
} from "@/features/models/runtime/local-deployment-configuration";
import {
  defaultSpeculativeChoice,
  overriddenSpeculativeChoice,
  type SpeculativeChoice,
} from "@/features/models/runtime/speculative-choice";
import {
  configurationVisionOption,
  overriddenVisionEnabled,
} from "@/features/models/runtime/vision-option";
import { type EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { type Model, type ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterValues } from "@/gen/dotcore/catalog/v1/parameter_pb";
import {
  type ExactPlacement,
  ExactPlacementSchema,
} from "@/gen/dotcore/deployment/v1/placement_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import type { FieldErrors } from "@/lib/form-validation";

import type { LocalResourceEstimateValues } from "./use-resource-estimate";

/** A model deployment the dialog previews or submits: its configuration, name, route and target. */
export interface LocalDeploymentValues extends LocalResourceEstimateValues {
  exposure: ReturnType<typeof deploymentExposure>;
  name: string;
  placement: ExactPlacement;
}

/**
 * The context a launch carries, which the Context step edits: the scenario the context's
 * compatibility read sizes, and the operator's inputs over the values the read starts the profile
 * from on the target. The total context follows the context per request and parallel requests
 * until the operator or the scenario sets it.
 */
export interface LaunchContext {
  /** The operator's inputs on the Context step; each absent while its starting value stands. */
  edits: Partial<Record<ScenarioInput, string>>;
  /** The latest valid chosen values, which the context's compatibility read sizes. */
  scenario?: SizingScenario;
}

/** The context values a deployment starts from on its target, from the compatibility read. */
export interface ContextStart {
  /** The context per request, when lowered because one request at the asked size doesn't fit. */
  contextTokensPerRequest?: number;
  totalContextTokens?: number;
}

export type ModelDeployStep = (typeof MODEL_DEPLOY_STEPS)[number];

/** A field the operator can change: a named field or an engine parameter ID. */
export type ModelDeployField = string;

/**
 * Every step in flow order. Capabilities shows only when the profile has a basic option to ask, and
 * Performance & memory and Command only in advanced mode.
 */
export const MODEL_DEPLOY_STEPS = [
  "Model & machine",
  "Capabilities",
  "Context",
  "Performance & memory",
  "Command",
  "Review & deploy",
] as const;

/** The step bar's short names, which keep its labels on one line. */
export const MODEL_DEPLOY_STEP_LABELS: Readonly<Record<ModelDeployStep, string>> = {
  Capabilities: "Capabilities",
  Command: "Command",
  Context: "Context",
  "Model & machine": "Model",
  "Performance & memory": "Performance",
  "Review & deploy": "Review",
};

/** The steps that show only in advanced mode. */
export const MODEL_DEPLOY_ADVANCED_STEPS: readonly ModelDeployStep[] = [
  "Performance & memory",
  "Command",
];

/** Keys a launch rejection among the Context step's errors. */
export const launchRejectionField = "launchRejection";

/**
 * The engine parameter inputs a launch shows and the declared values it submits beneath them. The
 * declared values are the starting values: the backend's read for the dialog's scenario, else its
 * read without a scenario, else the profile's defaults with the release defaults they leave out. A
 * total context the profile hides is left out, so resolution derives it by the engine's rule. The
 * inputs overlay them with the operator's parameter edits, then the launch's context: each value
 * the operator chose, else the target's starting value, else the scenario's, else the read's; a
 * total context known from none of them is the context per request times the parallel requests.
 */
export function useLaunchEngineValues(
  scenarioValues: ParameterValues | undefined,
  engineRelease: EngineRelease | undefined,
  profile: ModelProfile | undefined,
  edits: ReadonlyMap<string, string>,
  context: LaunchContext,
  start: ContextStart | undefined,
) {
  return useMemo(() => {
    const read = scenarioValues ?? profileStartingValues(engineRelease, profile);
    const totalShown = effectiveEngineParameters(engineRelease, profile).some(
      (parameter) => parameter.parameterId === TOTAL_CONTEXT_PARAMETER_ID,
    );
    const startingValues =
      read && !totalShown
        ? {
            ...read,
            values: read.values.filter((value) => value.parameterId !== TOTAL_CONTEXT_PARAMETER_ID),
          }
        : read;
    const inputs = new Map([...engineParameterInputsOf(read), ...edits]);
    const set = (parameterId: string, input: string | undefined) => {
      if (input !== undefined) inputs.set(parameterId, input);
    };
    const { edits: chosen, scenario } = context;
    set(PARALLEL_REQUESTS_PARAMETER_ID, chosen.parallel ?? tokenInput(scenario?.parallelRequests));
    set(
      CONTEXT_PER_REQUEST_PARAMETER_ID,
      chosen.perRequest ??
        tokenInput(start?.contextTokensPerRequest) ??
        tokenInput(scenario?.contextTokensPerRequest),
    );
    set(
      TOTAL_CONTEXT_PARAMETER_ID,
      chosen.total ??
        tokenInput(start?.totalContextTokens) ??
        tokenInput(scenario?.totalContextTokens) ??
        inputs.get(TOTAL_CONTEXT_PARAMETER_ID) ??
        product(
          inputs.get(CONTEXT_PER_REQUEST_PARAMETER_ID),
          inputs.get(PARALLEL_REQUESTS_PARAMETER_ID),
        ),
    );
    return { declaredValues: startingValues, engineParameterInputs: inputs };
  }, [context, edits, engineRelease, profile, scenarioValues, start]);
}

/** The context a scenario chooses, or the default context without one. */
export function scenarioContext(scenario: SizingScenario | undefined): LaunchContext {
  return { edits: {}, scenario };
}

/**
 * Whether the launch's total context follows the context per request and parallel requests: the
 * operator hasn't edited it and the scenario sets none.
 */
export function totalFollows(context: LaunchContext): boolean {
  return context.edits.total === undefined && context.scenario?.totalContextTokens === undefined;
}

/**
 * The context with the operator's inputs settled: the scenario takes the shown context per request
 * with the parallel requests and total context the operator or the scenario chose, when they are
 * valid. Unchanged, the same context.
 */
export function settledContext(context: LaunchContext, shownPerRequest: string): LaunchContext {
  const { edits, scenario } = context;
  const settled = scenarioFromInputs({
    parallel: edits.parallel ?? tokenInput(scenario?.parallelRequests) ?? "",
    perRequest: edits.perRequest ?? shownPerRequest,
    total: edits.total ?? tokenInput(scenario?.totalContextTokens) ?? "",
  });
  return settled && !sameSizingScenario(settled, scenario)
    ? { ...context, scenario: settled }
    : context;
}

/**
 * The context with its total context following again: the operator's total and the scenario's
 * are dropped, so the read starts it from the context per request and parallel requests.
 */
export function followingContext(context: LaunchContext): LaunchContext {
  const edits = { ...context.edits };
  delete edits.total;
  if (!context.scenario) return { edits };

  const scenario = { ...context.scenario };
  delete scenario.totalContextTokens;
  return { edits, scenario };
}

/**
 * The errors without those of the context parameters, which a context chosen or reset elsewhere
 * replaces.
 */
export function withoutChosenContextErrors(errors: FieldErrors): FieldErrors {
  const nextErrors = new Map(errors);
  for (const parameterId of CONTEXT_PARAMETER_IDS) nextErrors.delete(parameterId);
  return nextErrors;
}

/** The scenario input an engine parameter edits, if it is one of the context parameters. */
export function scenarioInput(parameterId: string): ScenarioInput | undefined {
  if (parameterId === TOTAL_CONTEXT_PARAMETER_ID) return "total";
  if (parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID) return "perRequest";
  if (parameterId === PARALLEL_REQUESTS_PARAMETER_ID) return "parallel";
  return undefined;
}

/**
 * The runtime fields a launch starts from on a profile: a configuration the dialog submitted
 * before, read back into its fields, or the profile's defaults without one. Its engine parameters
 * become edits, except the context and parallel requests, which the launch's context holds. The
 * weights always come from the profile, so a configuration's model artifact is not read back.
 */
export function configurationStart(
  model: Model,
  engineReleases: readonly EngineRelease[],
  profileId: string,
  configuration: LocalDeploymentConfigurationValues | undefined,
): ConfigurationStart {
  if (!configuration)
    return {
      customArgumentTokens: [],
      edits: new Map(),
      kvCachePrecision: "",
      speculativeChoice: profileSpeculativeChoice(model, profileId),
    };

  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const modelConfiguration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const exposedIds = new Set(
    effectiveEngineParameters(profileEngineRelease(model, profile, engineReleases), profile).map(
      (parameter) => parameter.parameterId,
    ),
  );
  const inputs = engineParameterInputsOf(configuration.engineParameterValues);
  const { optionOverrides } = configuration;

  return {
    customArgumentTokens: [...configuration.customArgumentTokens],
    edits: new Map(
      [...inputs].filter(
        ([parameterId]) =>
          exposedIds.has(parameterId) &&
          !CONTEXT_PARAMETER_IDS.has(parameterId) &&
          parameterId !== PARALLEL_REQUESTS_PARAMETER_ID,
      ),
    ),
    kvCachePrecision: overriddenKvCachePrecision(
      configurationKvCachePrecision(model, modelConfiguration, profile),
      optionOverrides,
    ),
    speculativeChoice: overriddenSpeculativeChoice(
      configurationSpeculativeOptions(model, modelConfiguration, profile),
      optionOverrides,
    ),
    vision: overriddenVisionEnabled(
      configurationVisionOption(model, modelConfiguration, profile),
      optionOverrides,
    ),
  };
}

/** The runtime field values a launch starts from, in the form the dialog's fields hold them. */
export interface ConfigurationStart {
  customArgumentTokens: string[];
  /** Engine parameter inputs over the profile's starting values. */
  edits: Map<string, string>;
  /** The picked KV cache precision; empty for the profile's default. */
  kvCachePrecision: string;
  speculativeChoice: SpeculativeChoice;
  /** Whether vision is on; absent for the profile's default. */
  vision?: boolean;
}

export function exactPlacement(target: ConfiguredExecutionTarget): ExactPlacement {
  return create(ExactPlacementSchema, {
    siteNodeId: target.siteNodeId,
    executionTargetId: target.executionTargetId,
  });
}

/** The backend's rejection reason as a sentence of the form. */
export function operatorSentence(reason: string): string {
  return `${reason.charAt(0).toUpperCase()}${reason.slice(1)}.`;
}

function profileSpeculativeChoice(model: Model, profileId: string): SpeculativeChoice {
  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  return defaultSpeculativeChoice(configurationSpeculativeOptions(model, configuration, profile));
}

function tokenInput(value: number | undefined): string | undefined {
  return value === undefined ? undefined : String(value);
}

// The context per request times the parallel requests, while both inputs hold whole numbers.
function product(perRequest: string | undefined, parallel: string | undefined): string | undefined {
  const values = [perRequest, parallel].map((input) => input?.trim() ?? "");
  return values.every((value) => /^\d+$/.test(value))
    ? String(BigInt(values[0]) * BigInt(values[1]))
    : undefined;
}
