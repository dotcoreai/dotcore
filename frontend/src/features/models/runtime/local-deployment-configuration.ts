import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { Model, ModelOptionValues, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ParameterDefinition, ParameterValues } from "@/gen/dotcore/catalog/v1/parameter_pb";
import { formatQuantity } from "@/lib/format";

import {
  CONTEXT_PARAMETER_IDS,
  effectiveEngineParameters,
  engineParameterError,
  engineParameterInputs,
  engineParameterValues,
  profileStartingValues,
} from "./engine-parameter-values";

/** The configuration of one local model deployment, as the create, preview, and estimate requests carry it. */
export interface LocalDeploymentConfigurationValues {
  customArgumentTokens: string[];
  engineParameterValues: ParameterValues;
  modelArtifactBindingId: string;
  /** Sparse option values layered over the profile's option defaults. */
  optionOverrides: ModelOptionValues[];
}

/** The edits layered over a profile's defaults. */
export interface LocalDeploymentConfigurationEdits {
  customArgumentTokens?: string[];
  /** Engine parameter values replacing the profile's defaults, such as those of a sizing scenario. */
  declaredValues?: ParameterValues;
  /** Parameters catalog sizing derives for the profile, which an empty input leaves to it. */
  derivedParameterIds?: ReadonlySet<string>;
  inputs: ReadonlyMap<string, string>;
  optionOverrides?: ModelOptionValues[];
}

/**
 * Builds the configuration a deployment of `profile` carries for the given edits, or `undefined`
 * when an exposed engine parameter has no valid value. It leaves the model artifact unset, so
 * resolution takes the profile's weights. The deploy dialog and the Catalog table's estimate
 * loader share it, so an untouched profile produces one request everywhere.
 */
export function localDeploymentConfiguration(
  profile: ModelProfile,
  engineRelease: EngineRelease | undefined,
  edits: LocalDeploymentConfigurationEdits,
): LocalDeploymentConfigurationValues | undefined {
  const values = engineParameterValues(
    edits.declaredValues ?? profileStartingValues(engineRelease, profile),
    effectiveEngineParameters(engineRelease, profile),
    edits.inputs,
    edits.derivedParameterIds,
  );
  if (!values) return undefined;

  return {
    customArgumentTokens: edits.customArgumentTokens ?? [],
    engineParameterValues: values,
    modelArtifactBindingId: "",
    optionOverrides: edits.optionOverrides ?? [],
  };
}

/**
 * Builds the configuration of an untouched profile: its default engine parameter inputs, and no
 * custom arguments or option overrides.
 */
export function defaultLocalDeploymentConfiguration(
  model: Model,
  profile: ModelProfile,
  engineReleases: readonly EngineRelease[],
): LocalDeploymentConfigurationValues | undefined {
  const engineRelease = profileEngineRelease(model, profile, engineReleases);
  return localDeploymentConfiguration(profile, engineRelease, {
    inputs: engineParameterInputs(profileStartingValues(engineRelease, profile)),
  });
}

/**
 * Describes the settings of a deployment draft that a profile change resets: edited engine
 * parameters in schema order. The context parameters and custom arguments belong to the launch
 * rather than the profile, so a profile change keeps them.
 */
export function runtimeCustomizations(
  profile: ModelProfile | undefined,
  engineRelease: EngineRelease | undefined,
  edits: LocalDeploymentConfigurationEdits,
): string[] {
  return changedEngineParameters(profile, engineRelease, edits).map(
    ({ parameter, value }) => `${parameter.displayName} ${value}`,
  );
}

/** One engine parameter whose input differs from its profile's default, with the input as text. */
export interface ChangedEngineParameter {
  parameter: ParameterDefinition;
  value: string;
}

/**
 * The engine parameters of a deployment draft whose input differs from the profile's default, in
 * schema order, leaving out the context parameters, which a launch describes on their own.
 */
export function changedEngineParameters(
  profile: ModelProfile | undefined,
  engineRelease: EngineRelease | undefined,
  edits: LocalDeploymentConfigurationEdits,
): ChangedEngineParameter[] {
  if (!profile) return [];

  const defaults = engineParameterInputs(
    edits.declaredValues ?? profileStartingValues(engineRelease, profile),
  );
  return effectiveEngineParameters(engineRelease, profile).flatMap((parameter) => {
    const input = edits.inputs.get(parameter.parameterId) ?? "";
    return CONTEXT_PARAMETER_IDS.has(parameter.parameterId) ||
      input === (defaults.get(parameter.parameterId) ?? "")
      ? []
      : [{ parameter, value: engineParameterInputLabel(parameter, edits.inputs) }];
  });
}

/**
 * The engine release of the first engine artifact of a profile's configuration, with only the
 * engine parameters the configuration keeps and the parameter groups left with a member. Every
 * release a configuration approves shares one
 * parameter schema, so this release describes them all.
 */
export function profileEngineRelease(
  model: Model,
  profile: ModelProfile | undefined,
  engineReleases: readonly EngineRelease[],
): EngineRelease | undefined {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationIds[0],
  );
  const release = engineReleases.find(
    (candidate) => candidate.engineReleaseId === configuration?.engineArtifacts[0]?.engineReleaseId,
  );
  const excluded = configuration?.excludedEngineParameterIds ?? [];
  if (!release?.parameterSchema || !excluded.length) return release;

  const parameters = release.parameterSchema.parameters.filter(
    (parameter) => !excluded.includes(parameter.parameterId),
  );
  return {
    ...release,
    parameterSchema: {
      ...release.parameterSchema,
      parameterGroups: release.parameterSchema.parameterGroups.filter((group) =>
        parameters.some((parameter) => parameter.groupId === group.groupId),
      ),
      parameters,
    },
  };
}

// Formats valid values and keeps an invalid input as typed, so a summary never rounds a value the
// form still rejects into a plausible one.
function engineParameterInputLabel(
  parameter: ParameterDefinition,
  inputs: ReadonlyMap<string, string>,
): string {
  const input = inputs.get(parameter.parameterId) ?? "";
  if (!input || engineParameterError(parameter, inputs)) return input || "empty";
  if (parameter.parameterType.case === "unsignedInteger")
    return formatQuantity(BigInt(input), parameter.parameterType.value.unit);
  if (parameter.parameterType.case === "boolean") return input === "true" ? "enabled" : "disabled";
  return input;
}
