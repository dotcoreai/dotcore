import { create } from "@bufbuild/protobuf";

import {
  type Model,
  type ModelEngineConfiguration,
  type ModelOptionValues,
  ModelOptionValuesSchema,
  type ModelProfile,
} from "@/gen/dotcore/catalog/v1/model_pb";

/**
 * The vision option a configuration declares: whether requests may include images, and whether
 * the profile turns it on by default.
 */
export interface VisionOption {
  defaultEnabled: boolean;
  description: string;
  displayName: string;
  optionId: string;
}

// Parameter id the catalog contract fixes for the switch of a vision option.
const ENABLED_PARAMETER_ID = "enabled";

/**
 * The vision option a configuration declares, with the profile's default replacing the
 * configuration's. Undefined when the configuration doesn't declare one with its switch.
 */
export function configurationVisionOption(
  model: Model,
  configuration: ModelEngineConfiguration | undefined,
  profile?: ModelProfile,
): VisionOption | undefined {
  for (const option of model.options) {
    if (option.kind.case !== "vision") continue;

    const declared = configuration?.options.find(
      (candidate) => candidate.optionId === option.optionId,
    );
    const parameterType = declared?.parameters.find(
      (parameter) => parameter.parameterId === ENABLED_PARAMETER_ID,
    )?.parameterType;
    if (!declared || parameterType?.case !== "boolean") continue;

    const profileDefault = enabledValue(
      profile?.optionDefaults.find((item) => item.optionId === option.optionId),
    );
    return {
      defaultEnabled: profileDefault ?? enabledValue(declared.defaultValues) ?? false,
      description: option.description,
      displayName: option.displayName,
      optionId: option.optionId,
    };
  }
  return undefined;
}

/** Whether vision is on: the operator's choice when made, otherwise the profile's default. */
export function currentVisionEnabled(
  option: VisionOption | undefined,
  picked: boolean | undefined,
): boolean {
  if (!option) return false;
  return picked ?? option.defaultEnabled;
}

/** The choice a launch's option overrides make for the option; undefined for the default. */
export function overriddenVisionEnabled(
  option: VisionOption | undefined,
  overrides: readonly ModelOptionValues[],
): boolean | undefined {
  if (!option) return undefined;
  return enabledValue(overrides.find((override) => override.optionId === option.optionId));
}

/** The option overrides a choice sends: none while it matches the default, otherwise the switch. */
export function visionOverrides(
  option: VisionOption | undefined,
  enabled: boolean,
): ModelOptionValues[] {
  if (!option || enabled === option.defaultEnabled) return [];

  return [
    create(ModelOptionValuesSchema, {
      optionId: option.optionId,
      values: [{ parameterId: ENABLED_PARAMETER_ID, value: { case: "boolean", value: enabled } }],
    }),
  ];
}

function enabledValue(values: ModelOptionValues | undefined): boolean | undefined {
  const value = values?.values.find(
    (candidate) => candidate.parameterId === ENABLED_PARAMETER_ID,
  )?.value;
  return value?.case === "boolean" ? value.value : undefined;
}
