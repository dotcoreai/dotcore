import { create } from "@bufbuild/protobuf";

import {
  type Model,
  type ModelEngineConfiguration,
  type ModelOptionValues,
  ModelOptionValuesSchema,
  type ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";
import type { ResolvedLocalModelDeploymentSnapshot } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * The KV cache precision option a configuration declares: the precisions an operator may choose
 * and the one the configuration or profile resolves to.
 */
export interface KvCachePrecisionOption {
  choices: string[];
  defaultPrecision: string;
  displayName: string;
  optionId: string;
}

// Parameter id the catalog contract fixes for the precision of a KV cache precision option.
const PRECISION_PARAMETER_ID = "precision";

/**
 * The KV cache precision option a configuration declares, with the profile's default replacing
 * the configuration's while it is one of the choices. Undefined when the configuration does not
 * declare it or declares no valid default.
 */
export function configurationKvCachePrecision(
  model: Model,
  configuration: ModelEngineConfiguration | undefined,
  profile?: ModelProfile,
): KvCachePrecisionOption | undefined {
  for (const option of model.options) {
    if (option.kind.case !== "kvCachePrecision") continue;

    const declared = configuration?.options.find(
      (candidate) => candidate.optionId === option.optionId,
    );
    const parameterType = declared?.parameters.find(
      (parameter) => parameter.parameterId === PRECISION_PARAMETER_ID,
    )?.parameterType;
    if (!declared || parameterType?.case !== "stringEnum") continue;

    const choices = parameterType.value.values;
    const profilePrecision = precisionValue(
      profile?.optionDefaults.find((item) => item.optionId === option.optionId),
    );
    const configurationPrecision = precisionValue(declared.defaultValues);
    const defaultPrecision = [profilePrecision, configurationPrecision].find(
      (candidate) => candidate !== undefined && choices.includes(candidate),
    );
    if (defaultPrecision === undefined) continue;

    return {
      choices,
      defaultPrecision,
      displayName: option.displayName,
      optionId: option.optionId,
    };
  }
  return undefined;
}

/**
 * The precision to show: the operator's pick while the option still offers it, otherwise the
 * default, as when a profile change or a catalog refresh drops the pick. An empty pick means the
 * default.
 */
export function currentKvCachePrecision(
  option: KvCachePrecisionOption | undefined,
  picked: string,
): string {
  if (!option) return "";
  return option.choices.includes(picked) ? picked : option.defaultPrecision;
}

/** The precision a launch's option overrides pick for the option; empty for the default. */
export function overriddenKvCachePrecision(
  option: KvCachePrecisionOption | undefined,
  overrides: readonly ModelOptionValues[],
): string {
  if (!option) return "";
  return precisionValue(overrides.find((override) => override.optionId === option.optionId)) ?? "";
}

/**
 * The option overrides a precision sends: none while it matches the default, otherwise the
 * precision for the option.
 */
export function kvCachePrecisionOverrides(
  option: KvCachePrecisionOption | undefined,
  precision: string,
): ModelOptionValues[] {
  if (!option || precision === option.defaultPrecision) return [];

  return [
    create(ModelOptionValuesSchema, {
      optionId: option.optionId,
      values: [
        { parameterId: PRECISION_PARAMETER_ID, value: { case: "string", value: precision } },
      ],
    }),
  ];
}

/** Describes a precision that differs from the default for the deploy summary; empty otherwise. */
export function kvCachePrecisionCustomizations(
  option: KvCachePrecisionOption | undefined,
  precision: string,
): string[] {
  if (!option || precision === option.defaultPrecision) return [];

  return [`${option.displayName} ${precision}`];
}

/**
 * The KV cache precision a deployment resolved to, or undefined when the catalog model is
 * unavailable or the deployment carries no such option.
 */
export function deploymentKvCachePrecisionLabel(
  snapshot: ResolvedLocalModelDeploymentSnapshot,
  catalogModel: Model | undefined,
): string | undefined {
  if (!catalogModel) return undefined;

  for (const option of snapshot.resolvedOptions) {
    const declared = catalogModel.options.find(
      (candidate) =>
        candidate.optionId === option.values?.optionId &&
        candidate.kind.case === "kvCachePrecision",
    );
    const precision = precisionValue(option.values);
    if (declared && precision !== undefined) return precision;
  }
  return undefined;
}

function precisionValue(values: ModelOptionValues | undefined): string | undefined {
  const value = values?.values.find(
    (candidate) => candidate.parameterId === PRECISION_PARAMETER_ID,
  )?.value;
  return value?.case === "string" ? value.value : undefined;
}
