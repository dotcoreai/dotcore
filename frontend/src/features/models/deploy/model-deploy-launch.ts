import { create } from "@bufbuild/protobuf";
import { useMemo } from "react";

import type { deploymentExposure } from "@/features/models/deployment-exposure";
import { configurationSpeculativeOptions } from "@/features/models/profiles/model-row";
import {
  followContextInputs,
  sameSizingScenario,
  scenarioFromInputs,
  type ScenarioInput,
  scenarioInputs,
  type SizingScenario,
} from "@/features/models/profiles/sizing-scenario";
import {
  CONTEXT_PER_REQUEST_PARAMETER_ID,
  engineParameterInputs as engineParameterInputsOf,
  PARALLEL_REQUESTS_PARAMETER_ID,
  profileStartingValues,
  TOTAL_CONTEXT_PARAMETER_ID,
} from "@/features/models/runtime/engine-parameter-values";
import { defaultSpeculativeChoice } from "@/features/models/runtime/speculative-choice";
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
 * The context a launch carries, which the Context step and the Runs on chip's "Sized for" both
 * edit. Until the operator, the chip, or the opener's scenario chooses the total context and
 * context per request, the total context follows the default for the profile and target.
 */
export interface LaunchContext {
  /** The chosen total context and context per request inputs; absent while the default applies. */
  inputs?: Record<ScenarioInput, string>;
  /** The operator's parallel requests; absent while the profile's starting value stands. */
  parallelRequests?: string;
  /** The latest valid chosen values, which "Sized for" shows and the compatibility read sizes. */
  scenario?: SizingScenario;
}

export type ModelDeployStep = (typeof MODEL_DEPLOY_STEPS)[number];

/** A field the operator can change: a named field or an engine parameter ID. */
export type ModelDeployField = string;

/** Every step in flow order; the dialog shows a subset depending on Advanced and the targets. */
export const MODEL_DEPLOY_STEPS = ["Profile", "Target", "Context", "Runtime", "Deploy"] as const;

/** Keys a launch rejection among the Context step's errors. */
export const launchRejectionField = "launchRejection";

/**
 * The engine parameter inputs a launch shows and the declared values it submits beneath them. The
 * declared values are the starting values: the backend's read for the dialog's scenario, else its
 * read without a scenario, else the profile's defaults with the release defaults they leave out.
 * The inputs overlay them with the operator's runtime edits, then the launch's context, whose total
 * context follows the starting value of the target's largest fit until chosen.
 */
export function useLaunchEngineValues(
  scenarioValues: ParameterValues | undefined,
  engineRelease: EngineRelease | undefined,
  profile: ModelProfile | undefined,
  edits: ReadonlyMap<string, string>,
  context: LaunchContext,
  fitStart: number | undefined,
) {
  return useMemo(() => {
    const startingValues = scenarioValues ?? profileStartingValues(engineRelease, profile);
    const inputs = new Map([...engineParameterInputsOf(startingValues), ...edits]);
    if (context.parallelRequests !== undefined)
      inputs.set(PARALLEL_REQUESTS_PARAMETER_ID, context.parallelRequests);
    if (context.inputs) {
      inputs.set(TOTAL_CONTEXT_PARAMETER_ID, context.inputs.total);
      inputs.set(CONTEXT_PER_REQUEST_PARAMETER_ID, context.inputs.perRequest);
    } else if (fitStart !== undefined) {
      inputs.set(TOTAL_CONTEXT_PARAMETER_ID, String(fitStart));
    }
    return { declaredValues: startingValues, engineParameterInputs: inputs };
  }, [context, edits, engineRelease, fitStart, profile, scenarioValues]);
}

/** The context a scenario chooses, or the default context without one. */
export function scenarioContext(scenario: SizingScenario | undefined): LaunchContext {
  return scenario ? { inputs: scenarioInputs(scenario), scenario } : {};
}

/**
 * The context once the edited input is left: the other of total context and context per request
 * follows it, and "Sized for" takes the inputs when they are valid. Unchanged, the same context.
 */
export function settledContext(context: LaunchContext, edited: ScenarioInput): LaunchContext {
  if (!context.inputs) return context;

  const inputs = followContextInputs(context.inputs, edited);
  const scenario = scenarioFromInputs(inputs) ?? context.scenario;
  return inputs === context.inputs && sameSizingScenario(scenario, context.scenario)
    ? context
    : { ...context, inputs, scenario };
}

/**
 * The errors without those of the total context and context per request, which a context chosen
 * or reset elsewhere replaces. Parallel requests keep theirs, since their input stays.
 */
export function withoutChosenContextErrors(errors: FieldErrors): FieldErrors {
  const nextErrors = new Map(errors);
  nextErrors.delete(TOTAL_CONTEXT_PARAMETER_ID);
  nextErrors.delete(CONTEXT_PER_REQUEST_PARAMETER_ID);
  return nextErrors;
}

/** The scenario input an engine parameter edits, if it is the total context or per request. */
export function scenarioInput(parameterId: string): ScenarioInput | undefined {
  if (parameterId === TOTAL_CONTEXT_PARAMETER_ID) return "total";
  if (parameterId === CONTEXT_PER_REQUEST_PARAMETER_ID) return "perRequest";
  return undefined;
}

export function withoutUnsetContextPerRequest(
  values: ParameterValues | undefined,
  scenario: SizingScenario | undefined,
): ParameterValues | undefined {
  if (!values || scenario?.contextTokensPerRequest !== undefined) return values;

  return {
    ...values,
    values: values.values.filter((value) => value.parameterId !== CONTEXT_PER_REQUEST_PARAMETER_ID),
  };
}

export function profileDefaults(model: Model, profileId: string) {
  const profile = model.profiles.find((candidate) => candidate.profileId === profileId);
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );

  return {
    modelArtifactBindingId: profile?.defaultModelArtifactBindingId ?? "",
    speculativeChoice: defaultSpeculativeChoice(
      configurationSpeculativeOptions(model, configuration, profile),
    ),
  };
}

export function modelDeploySteps(target: boolean, advanced: boolean): ModelDeployStep[] {
  return MODEL_DEPLOY_STEPS.filter(
    (step) =>
      (step === "Target" && target) ||
      (step === "Runtime" && advanced) ||
      step === "Profile" ||
      step === "Context" ||
      step === "Deploy",
  );
}

// A vanished step falls forward: Target to Context, which always shows, and Runtime to Deploy.
export function fallbackStep(missingStep: string): string {
  return missingStep === "Target" ? "Context" : "Deploy";
}

export function exactPlacement(target: ConfiguredExecutionTarget): ExactPlacement {
  return create(ExactPlacementSchema, {
    siteNodeId: target.siteNodeId,
    executionTargetId: target.executionTargetId,
  });
}

export function moveToken(tokens: string[], index: number, direction: -1 | 1): string[] {
  const nextIndex = index + direction;
  if (nextIndex < 0 || nextIndex >= tokens.length) return tokens;
  const next = [...tokens];
  [next[index], next[nextIndex]] = [next[nextIndex], next[index]];
  return next;
}

/** The backend's rejection reason as a sentence of the form. */
export function operatorSentence(reason: string): string {
  return `${reason.charAt(0).toUpperCase()}${reason.slice(1)}.`;
}
