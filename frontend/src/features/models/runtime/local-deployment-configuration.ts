import type {
  EngineParameterDefinition,
  EngineParameterValues,
  EngineRelease,
} from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { Model, ModelOptionValues, ModelProfile } from "@/gen/dotcore/catalog/v1/model_pb";
import { formatQuantity } from "@/lib/format";

import {
  CONTEXT_PARAMETER_IDS,
  effectiveEngineParameters,
  engineParameterError,
  engineParameterInputs,
  engineParameterValues,
  PARALLEL_REQUESTS_PARAMETER_ID,
  profileStartingValues,
} from "./engine-parameter-values";

/** The configuration of one local model deployment, as the create, preview, and estimate requests carry it. */
export interface LocalDeploymentConfigurationValues {
  customArgumentTokens: string[];
  engineParameterValues: EngineParameterValues;
  modelArtifactBindingId: string;
  /** Sparse option values layered over the profile's option defaults. */
  optionOverrides: ModelOptionValues[];
}

/** The edits layered over a profile's defaults. */
export interface LocalDeploymentConfigurationEdits {
  bindingId: string;
  customArgumentTokens?: string[];
  /** Engine parameter values replacing the profile's defaults, such as those of a sizing scenario. */
  declaredValues?: EngineParameterValues;
  /** Parameters catalog sizing derives for the profile, which an empty input leaves to it. */
  derivedParameterIds?: ReadonlySet<string>;
  inputs: ReadonlyMap<string, string>;
  optionOverrides?: ModelOptionValues[];
}

/**
 * Builds the configuration a deployment of `profile` carries for the given edits, or `undefined`
 * when an exposed engine parameter has no valid value. The deploy dialog and the Catalog table's
 * estimate loader share it, so an untouched profile produces one request everywhere.
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
  if (!values || !edits.bindingId) return undefined;

  return {
    customArgumentTokens: edits.customArgumentTokens ?? [],
    engineParameterValues: values,
    modelArtifactBindingId: edits.bindingId,
    optionOverrides: edits.optionOverrides ?? [],
  };
}

/**
 * Builds the configuration of an untouched profile: its default binding, its default engine
 * parameter inputs, and no custom arguments or option overrides.
 */
export function defaultLocalDeploymentConfiguration(
  model: Model,
  profile: ModelProfile,
  engineReleases: readonly EngineRelease[],
): LocalDeploymentConfigurationValues | undefined {
  const engineRelease = profileEngineRelease(model, profile, engineReleases);
  return localDeploymentConfiguration(profile, engineRelease, {
    bindingId: profile.defaultModelArtifactBindingId,
    inputs: engineParameterInputs(profileStartingValues(engineRelease, profile)),
  });
}

/**
 * Describes the runtime settings of a deployment draft that differ from its profile's defaults:
 * edited engine parameters in schema order, then a changed model artifact. The context sizes,
 * parallel requests and custom arguments belong to the launch rather than the profile, so a profile
 * change keeps them and callers describe them separately.
 */
export function runtimeCustomizations(
  model: Model,
  profile: ModelProfile | undefined,
  engineRelease: EngineRelease | undefined,
  edits: LocalDeploymentConfigurationEdits,
): string[] {
  if (!profile) return [];

  const customizations = editedParameterLabels(
    profile,
    engineRelease,
    edits,
    (parameterId) =>
      !CONTEXT_PARAMETER_IDS.has(parameterId) && parameterId !== PARALLEL_REQUESTS_PARAMETER_ID,
  );
  if (edits.bindingId !== profile.defaultModelArtifactBindingId) {
    const binding = model.configurations
      .find((candidate) => candidate.configurationId === profile.configurationId)
      ?.artifactBindings.find((candidate) => candidate.bindingId === edits.bindingId);
    customizations.push(`Artifact ${binding?.modelArtifactId ?? "not selected"}`);
  }
  return customizations;
}

/** Describes a deployment draft's parallel requests when they differ from its profile's default. */
export function parallelRequestsCustomization(
  profile: ModelProfile | undefined,
  engineRelease: EngineRelease | undefined,
  edits: LocalDeploymentConfigurationEdits,
): string[] {
  if (!profile) return [];

  return editedParameterLabels(
    profile,
    engineRelease,
    edits,
    (parameterId) => parameterId === PARALLEL_REQUESTS_PARAMETER_ID,
  );
}

/**
 * The engine release of the first engine artifact of a profile's configuration. Every release a
 * configuration approves shares one parameter schema, so this release describes them all.
 */
export function profileEngineRelease(
  model: Model,
  profile: ModelProfile | undefined,
  engineReleases: readonly EngineRelease[],
): EngineRelease | undefined {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  return engineReleases.find(
    (candidate) => candidate.engineReleaseId === configuration?.engineArtifacts[0]?.engineReleaseId,
  );
}

/**
 * The distinct engine releases a profile's configuration approves artifacts from, in declaration
 * order. A release missing from `engineReleases` keeps its id with an empty version.
 */
export function approvedEngineReleases(
  model: Model,
  profile: ModelProfile | undefined,
  engineReleases: readonly EngineRelease[],
): EngineReleaseRef[] {
  const configuration = model.configurations.find(
    (candidate) => candidate.configurationId === profile?.configurationId,
  );
  const ids = [
    ...new Set(configuration?.engineArtifacts.map((artifact) => artifact.engineReleaseId) ?? []),
  ];
  return ids.map((engineReleaseId) => ({
    engineReleaseId,
    version:
      engineReleases.find((candidate) => candidate.engineReleaseId === engineReleaseId)?.version ??
      "",
  }));
}

/** An engine release as "id / version", or its id alone without a version. */
export function engineReleaseLabel({ engineReleaseId, version }: EngineReleaseRef): string {
  return version ? `${engineReleaseId} / ${version}` : engineReleaseId;
}

/** An engine release's identity and version. */
export interface EngineReleaseRef {
  engineReleaseId: string;
  version: string;
}

/** Labels the profile's engine parameters the predicate selects whose input differs from its default. */
function editedParameterLabels(
  profile: ModelProfile,
  engineRelease: EngineRelease | undefined,
  edits: LocalDeploymentConfigurationEdits,
  selected: (parameterId: string) => boolean,
): string[] {
  const defaults = engineParameterInputs(
    edits.declaredValues ?? profileStartingValues(engineRelease, profile),
  );
  return effectiveEngineParameters(engineRelease, profile).flatMap((parameter) => {
    const input = edits.inputs.get(parameter.parameterId) ?? "";
    return !selected(parameter.parameterId) || input === (defaults.get(parameter.parameterId) ?? "")
      ? []
      : [`${parameter.displayName} ${engineParameterInputLabel(parameter, edits.inputs)}`];
  });
}

// Formats valid values and keeps an invalid input as typed, so a summary never rounds a value the
// form still rejects into a plausible one.
function engineParameterInputLabel(
  parameter: EngineParameterDefinition,
  inputs: ReadonlyMap<string, string>,
): string {
  const input = inputs.get(parameter.parameterId) ?? "";
  if (!input || engineParameterError(parameter, inputs)) return input || "empty";
  if (parameter.parameterType.case === "unsignedInteger")
    return formatQuantity(BigInt(input), parameter.parameterType.value.unit);
  return input === "true" ? "enabled" : "disabled";
}
