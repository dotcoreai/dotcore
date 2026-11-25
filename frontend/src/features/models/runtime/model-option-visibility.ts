import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import { ParameterVisibility } from "@/gen/dotcore/catalog/v1/parameter_pb";

/**
 * Where the catalog shows a model option: basic, advanced or hidden. An option without a declared
 * visibility counts as advanced, so it never lands on the basic path by accident.
 */
export function modelOptionVisibility(model: Model, optionId: string): ParameterVisibility {
  const visibility = model.options.find((option) => option.optionId === optionId)?.visibility;
  return visibility === undefined || visibility === ParameterVisibility.UNSPECIFIED
    ? ParameterVisibility.ADVANCED
    : visibility;
}

/**
 * The visibility of one control over several options, such as the speculative decoding choice:
 * basic when any option is basic, else advanced when any is advanced, else hidden.
 */
export function mostVisible(
  model: Model,
  optionIds: readonly string[],
): ParameterVisibility | undefined {
  const visibilities = optionIds.map((optionId) => modelOptionVisibility(model, optionId));
  return [ParameterVisibility.BASIC, ParameterVisibility.ADVANCED, ParameterVisibility.HIDDEN].find(
    (visibility) => visibilities.includes(visibility),
  );
}
